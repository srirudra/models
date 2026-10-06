import { SSEParser } from "../providers/sse.js";

type JsonObject = Record<string, unknown>;

function object(value: unknown): JsonObject {
  return value && typeof value === "object" ? value as JsonObject : {};
}

function textContent(content: unknown): unknown {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return content;
  return content.map((part) => {
    if (typeof part === "string") return part;
    const value = object(part);
    return typeof value.text === "string" ? value.text : JSON.stringify(part);
  }).join("");
}

function mapMessage(message: unknown): JsonObject[] {
  const value = object(message);
  const role = value.role;
  if (role === "tool") {
    return [{ type: "function_call_output", call_id: value.tool_call_id, output: textContent(value.content) }];
  }
  if (role === "assistant" && Array.isArray(value.tool_calls)) {
    const calls = value.tool_calls.map((call) => {
      const functionValue = object(object(call).function);
      return {
        type: "function_call",
        call_id: object(call).id,
        name: functionValue.name,
        arguments: functionValue.arguments ?? "",
      };
    });
    return [
      ...(value.content == null ? [] : [{ role, content: textContent(value.content) }]),
      ...calls as JsonObject[],
    ];
  }
  return [{ role, content: textContent(value.content) }];
}

export function chatCompletionsToResponses(body: Record<string, unknown>): Record<string, unknown> {
  const output: Record<string, unknown> = { model: body.model };
  if (Array.isArray(body.messages)) output.input = body.messages.flatMap(mapMessage);
  if (Array.isArray(body.tools)) {
    output.tools = body.tools.map((tool) => {
      const fn = object(object(tool).function);
      return {
        type: "function",
        name: fn.name,
        description: fn.description,
        parameters: fn.parameters,
      };
    });
  }
  if (body.tool_choice !== undefined) {
    const choice = body.tool_choice;
    output.tool_choice = choice && typeof choice === "object"
      ? { type: "function", name: object(object(choice).function).name }
      : choice;
  }
  for (const key of ["stream", "temperature", "top_p", "presence_penalty", "frequency_penalty"]) {
    if (body[key] !== undefined) output[key] = body[key];
  }
  const maxTokens = body.max_completion_tokens ?? body.max_tokens;
  if (maxTokens !== undefined) output.max_output_tokens = maxTokens;
  return output;
}

function chunk(delta: JsonObject, finishReason: string | null = null, usage?: unknown): string {
  const value: JsonObject = {
    id: "chatcmpl-responses",
    object: "chat.completion.chunk",
    choices: [{ index: 0, delta, finish_reason: finishReason }],
  };
  if (usage !== undefined) value.usage = usage;
  return `data: ${JSON.stringify(value)}\n\n`;
}

export class ResponsesToChatCompletions {
  private readonly parser = new SSEParser();
  private readonly toolIndexes = new Map<string, number>();
  private toolCount = 0;
  private completed = false;

  push(sseEventRawText: string): string[] {
    const events = this.parser.feed(new TextEncoder().encode(sseEventRawText));
    return events.flatMap((event) => this.translate(event.data));
  }

  end(): string[] {
    try {
      const trailing = this.parser.feed(new TextEncoder().encode("\n"));
      return [
        ...trailing.flatMap((event) => this.translate(event.data)),
        ...this.parser.feed(new Uint8Array(), true).flatMap((event) => this.translate(event.data)),
      ];
    } catch (error) {
      // Captured streams can end with a lone line break after the final frame.
      // The completed event has already produced the terminal chat chunk.
      // SSEParser treats a terminal blank line as an incomplete block when
      // the producer already supplied the final double newline.
      return [];
    }
  }

  private translate(data: string): string[] {
    let event: JsonObject;
    try {
      event = object(JSON.parse(data));
    } catch {
      return [];
    }
    const type = event.type;
    if (type === "response.output_item.added") {
      const item = object(event.item);
      if (item.type !== "function_call") return [];
      const key = String(item.id ?? `${event.output_index ?? this.toolCount}`);
      const index = typeof event.output_index === "number" ? event.output_index : this.toolCount++;
      this.toolIndexes.set(key, index);
      return [chunk({
        tool_calls: [{
          index,
          id: item.call_id,
          type: "function",
          function: { name: item.name, arguments: "" },
        }],
      })];
    }
    if (type === "response.output_text.delta") {
      return [chunk({ content: typeof event.delta === "string" ? event.delta : "" })];
    }
    if (type === "response.function_call_arguments.delta") {
      const key = String(event.item_id ?? "");
      const index = this.toolIndexes.get(key) ?? (
        typeof event.output_index === "number" ? event.output_index : 0
      );
      return [chunk({
        tool_calls: [{ index, function: { arguments: typeof event.delta === "string" ? event.delta : "" } }],
      })];
    }
    if (type === "response.completed") {
      this.completed = true;
      const response = object(event.response);
      const usageValue = object(response.usage);
      const usage = usageValue.input_tokens === undefined ? undefined : {
        prompt_tokens: usageValue.input_tokens,
        completion_tokens: usageValue.output_tokens,
        total_tokens: usageValue.total_tokens,
      };
      const finish = this.toolCount > 0 || this.toolIndexes.size > 0 ? "tool_calls" : "stop";
      return [chunk({}, finish, usage), "data: [DONE]\n\n"];
    }
    // Reasoning events are deliberately dropped; exposing private reasoning as
    // assistant content would make the chat-completions stream misleading.
    return [];
  }
}

export function responsesJsonToChatCompletion(responseJson: Record<string, unknown>): Record<string, unknown> {
  const output = Array.isArray(responseJson.output) ? responseJson.output : [];
  let content = "";
  const toolCalls: JsonObject[] = [];
  for (const raw of output) {
    const item = object(raw);
    if (item.type === "message" && Array.isArray(item.content)) {
      for (const part of item.content) {
        const value = object(part);
        if (typeof value.text === "string") content += value.text;
      }
    } else if (item.type === "function_call") {
      toolCalls.push({
        id: item.call_id ?? item.id,
        type: "function",
        function: { name: item.name, arguments: item.arguments ?? "" },
      });
    }
  }
  const usageValue = object(responseJson.usage);
  const usage = {
    prompt_tokens: usageValue.input_tokens ?? 0,
    completion_tokens: usageValue.output_tokens ?? 0,
    total_tokens: usageValue.total_tokens ?? 0,
  };
  return {
    id: responseJson.id ?? "chatcmpl-responses",
    object: "chat.completion",
    choices: [{
      index: 0,
      message: { role: "assistant", content: content || null, ...(toolCalls.length ? { tool_calls: toolCalls } : {}) },
      finish_reason: toolCalls.length ? "tool_calls" : "stop",
    }],
    usage,
  };
}
