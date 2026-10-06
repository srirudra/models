import { readFile } from "node:fs/promises";
import { describe, expect, it } from "vitest";
import {
  chatCompletionsToResponses,
  ResponsesToChatCompletions,
  responsesJsonToChatCompletion,
} from "../src/intercept/responses-translate.js";

async function fixture(name: string): Promise<string> {
  return readFile(new URL(`../../spike5/${name}`, import.meta.url), "utf8");
}

function dataLines(value: string): Record<string, unknown>[] {
  return value.split(/\r?\n/)
    .filter((line) => line.startsWith("data: ") && line.slice(6) !== "[DONE]")
    .map((line) => JSON.parse(line.slice(6)) as Record<string, unknown>);
}

describe("Responses translation", () => {
  it("translates visible text and drops reasoning", async () => {
    const translator = new ResponsesToChatCompletions();
    const output = translator.push(await fixture("responses-text-stream.txt")).concat(translator.end());
    const chunks = dataLines(output.join(""));
    const text = chunks.map((value) => String(((value.choices as Array<Record<string, unknown>>)[0].delta as Record<string, unknown>).content ?? "")).join("");
    expect(text).toContain("HELLO WORLD");
    expect(text).not.toContain("The user wants");
    expect(output.at(-1)).toBe("data: [DONE]\n\n");
    expect(chunks.at(-1)?.choices).toEqual([{ index: 0, delta: {}, finish_reason: "stop" }]);
  });

  it("translates streamed function calls", async () => {
    const translator = new ResponsesToChatCompletions();
    const output = translator.push(await fixture("responses-toolcall-stream.txt")).concat(translator.end());
    const chunks = dataLines(output.join(""));
    const calls = chunks.flatMap((value) => ((value.choices as Array<Record<string, unknown>>)[0].delta as Record<string, unknown>).tool_calls as Array<Record<string, unknown>> ?? []);
    expect(calls.some((call) => (call.function as Record<string, unknown>).name === "get_weather")).toBe(true);
    expect(calls.map((call) => (call.function as Record<string, unknown>).arguments ?? "").join("")).toContain("\"city\": \"Paris\"");
    expect((chunks.at(-1)?.choices as Array<Record<string, unknown>>)[0].finish_reason).toBe("tool_calls");
  });

  it("maps chat requests without mutating them", () => {
    const body = {
      model: "qwen", messages: [
        { role: "assistant", tool_calls: [{ id: "call-1", type: "function", function: { name: "weather", arguments: "{}" } }] },
        { role: "tool", tool_call_id: "call-1", content: "sunny" },
      ],
      tools: [{ type: "function", function: { name: "weather", description: "Weather", parameters: { type: "object" } } }],
      tool_choice: { type: "function", function: { name: "weather" } },
      max_tokens: 10, stream: true, stream_options: { include_usage: true },
    };
    const translated = chatCompletionsToResponses(body);
    expect(translated).toMatchObject({ model: "qwen", max_output_tokens: 10, stream: true, tool_choice: { type: "function", name: "weather" } });
    expect(translated).not.toHaveProperty("stream_options");
    expect(translated.tools).toEqual([{ type: "function", name: "weather", description: "Weather", parameters: { type: "object" } }]);
    expect(body).toHaveProperty("messages");
  });

  it("translates a non-stream response", () => {
    const output = responsesJsonToChatCompletion({
      id: "resp-1",
      output: [{ type: "message", content: [{ type: "output_text", text: "hello" }] }],
      usage: { input_tokens: 2, output_tokens: 3, total_tokens: 5 },
    });
    expect(output).toMatchObject({
      id: "resp-1", object: "chat.completion",
      choices: [{ message: { role: "assistant", content: "hello" }, finish_reason: "stop" }],
      usage: { prompt_tokens: 2, completion_tokens: 3, total_tokens: 5 },
    });
  });
});
