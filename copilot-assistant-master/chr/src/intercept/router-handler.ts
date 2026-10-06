import http from "node:http";
import https from "node:https";
import tls from "node:tls";
import type { ConfigRevision, Provider } from "../config/types.js";
import { resolveRoute } from "../routing/resolve.js";
import { ForwardError, forwardSSE } from "../providers/forward.js";
import { buildCatalogEntry, mergeCatalog } from "./catalog.js";
import { normalizeChatCompletionsBody } from "./chat-normalize.js";
import {
  chatCompletionsToResponses,
  ResponsesToChatCompletions,
} from "./responses-translate.js";
import type { RecordingMode } from "../recording/recorder.js";

export type InterceptLogger = (message: string) => void;
export type InterceptSink = {
  routeDecision?: (event: Record<string, string | number>) => void;
};
export type Forward = typeof forwardSSE;
export type UpstreamResponse = {
  status: number;
  headers: http.IncomingHttpHeaders;
  body: Buffer;
};
export type Upstream = (
  host: string,
  req: http.IncomingMessage,
  body: Buffer,
  headers?: Record<string, string | string[] | undefined>,
) => Promise<UpstreamResponse>;

const noop = () => undefined;
const bodyLimit = 32 * 1024 * 1024;

function upstreamRequest(
  host: string,
  req: http.IncomingMessage,
  body: Buffer,
  headers = req.headers,
): Promise<UpstreamResponse> {
  const forwarded = { ...headers };
  delete forwarded.connection;
  delete forwarded["proxy-connection"];
  return new Promise((resolve, reject) => {
    const request = https.request({
      hostname: host,
      port: 443,
      servername: host,
      method: req.method,
      path: req.url,
      headers: forwarded,
      ...( { ALPNProtocols: ["http/1.1"] } as tls.ConnectionOptions),
    }, (response) => {
      const chunks: Buffer[] = [];
      response.on("data", (chunk) => chunks.push(Buffer.from(chunk)));
      response.once("end", () => resolve({
        status: response.statusCode ?? 502,
        headers: response.headers,
        body: Buffer.concat(chunks),
      }));
    });
    request.once("error", reject);
    if (body.length) request.write(body);
    request.end();
  });
}

function writeResponse(res: http.ServerResponse, response: UpstreamResponse): void {
  res.writeHead(response.status, response.headers);
  res.end(response.body);
}

async function readBody(req: http.IncomingMessage): Promise<Buffer> {
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const chunk of req) {
    const value = Buffer.from(chunk);
    size += value.length;
    if (size > bodyLimit) throw new Error("request body exceeds 32 MiB limit");
    chunks.push(value);
  }
  return Buffer.concat(chunks);
}

function json(res: http.ServerResponse, status: number, value: unknown): void {
  if (res.writableEnded || res.destroyed) return;
  const body = Buffer.from(JSON.stringify(value));
  if (!res.headersSent) {
    res.writeHead(status, {
      "content-type": "application/json",
      "content-length": body.length,
    });
  }
  if (!res.writableEnded && !res.destroyed) res.end(body);
}

function writeStream(
  res: http.ServerResponse,
  chunk: Uint8Array,
  signal: AbortSignal,
): Promise<void> {
  if (res.write(chunk)) return Promise.resolve();
  return new Promise((resolve, reject) => {
    const drain = () => { cleanup(); resolve(); };
    const error = (reason: Error) => { cleanup(); reject(reason); };
    const abort = () => error(signal.reason instanceof Error ? signal.reason : new Error("client disconnected"));
    const cleanup = () => {
      res.removeListener("drain", drain);
      res.removeListener("error", error);
      signal.removeEventListener("abort", abort);
    };
    res.once("drain", drain);
    res.once("error", error);
    signal.addEventListener("abort", abort, { once: true });
    if (signal.aborted) abort();
  });
}

export function eligibleModels(revision: ConfigRevision, warn: InterceptLogger = noop): ConfigRevision["models"] {
  return revision.models.filter((model) => {
    const provider = revision.providers[model.provider];
    if (!provider) return false;
    if (provider.protocol === "openai-responses") {
      warn(`Enabling ${model.alias} in intercept mode with OpenAI Responses translation`);
    }
    return provider.protocol === "openai-chat-completions" || provider.protocol === "openai-responses";
  });
}

function translatedChunksToCompletion(chunks: readonly Uint8Array[]): Record<string, unknown> {
  let content = "";
  let id = "chatcmpl-responses";
  const tools = new Map<number, { index: number; id?: unknown; type: "function"; function: { name?: unknown; arguments: string } }>();
  let finishReason: unknown = "stop";
  let usage: unknown;
  for (const bytes of chunks) {
    for (const line of Buffer.from(bytes).toString("utf8").split(/\r?\n/)) {
      if (!line.startsWith("data: ") || line.slice(6) === "[DONE]") continue;
      const value = JSON.parse(line.slice(6)) as Record<string, unknown>;
      if (typeof value.id === "string") id = value.id;
      const choice = (value.choices as Array<Record<string, unknown>> | undefined)?.[0];
      const delta = (choice?.delta ?? {}) as Record<string, unknown>;
      if (typeof delta.content === "string") content += delta.content;
      for (const call of (delta.tool_calls ?? []) as Array<Record<string, unknown>>) {
        const index = Number(call.index ?? 0);
        const functionValue = (call.function ?? {}) as Record<string, unknown>;
        const existing = tools.get(index);
        if (existing) {
          if (typeof functionValue.arguments === "string") existing.function.arguments += functionValue.arguments;
          if (functionValue.name !== undefined) existing.function.name = functionValue.name;
        } else {
          tools.set(index, {
            index, id: call.id, type: "function",
            function: { name: functionValue.name, arguments: typeof functionValue.arguments === "string" ? functionValue.arguments : "" },
          });
        }
      }
      if (choice?.finish_reason !== null && choice?.finish_reason !== undefined) finishReason = choice.finish_reason;
      if (value.usage !== undefined) usage = value.usage;
    }
  }
  return {
    id,
    object: "chat.completion",
    choices: [{
      index: 0,
      message: { role: "assistant", content: content || null, ...(tools.size ? { tool_calls: [...tools.values()] } : {}) },
      finish_reason: finishReason,
    }],
    ...(usage === undefined ? {} : { usage }),
  };
}

/**
 * A request handler that can swap its configuration revision while running.
 * Calling it stays backwards compatible with the plain function form.
 */
export type InterceptHandler = ((
  req: http.IncomingMessage,
  res: http.ServerResponse,
  ctx: { host: string },
) => void) & {
  /** Swap in a newly loaded revision; in-flight requests keep the old one. */
  setRevision(revision: ConfigRevision): void;
  currentRevision(): ConfigRevision;
};

export function createInterceptHandler(
  revision: ConfigRevision,
  options: {
    logger?: InterceptLogger;
    sink?: InterceptSink;
    forward?: Forward;
    upstream?: Upstream;
    recorder?: { currentMode(): RecordingMode; record: (entry: any) => Promise<void>; maxBodyBytes?: number };
    /** Version gate: never inject or route custom models, pass everything through. */
    passthroughOnly?: boolean;
    /** Optional source of truth for the current revision (hot reload). */
    getRevision?: () => ConfigRevision;
  } = {},
): InterceptHandler {
  const log = options.logger ?? noop;
  const passthroughOnly = options.passthroughOnly === true;
  let current = revision;
  const eligible = new Map<number, { models: ConfigRevision["models"]; aliases: Set<string> }>();
  const entryFor = (value: ConfigRevision) => {
    const cached = eligible.get(value.revisionId);
    if (cached) return cached;
    const models = passthroughOnly ? [] : eligibleModels(value, log);
    const entry = { models, aliases: new Set(models.map((model) => model.alias)) };
    eligible.set(value.revisionId, entry);
    // Keep the cache bounded across long-lived sessions with many reloads.
    while (eligible.size > 8) eligible.delete(eligible.keys().next().value as number);
    return entry;
  };
  entryFor(current);
  const upstream = options.upstream ?? upstreamRequest;
  const forward = options.forward ?? forwardSSE;

  const handler: InterceptHandler = Object.assign(
    (req: http.IncomingMessage, res: http.ServerResponse, ctx: { host: string }): void => {
    void (async () => {
      // Capture the revision once so the request keeps a consistent view of
      // the configuration even if a hot reload lands mid-flight.
      const revision = options.getRevision ? options.getRevision() : current;
      const { models, aliases } = entryFor(revision);
      const path = new URL(req.url ?? "/", `https://${ctx.host}`).pathname;
      const body = await readBody(req);
      const abort = new AbortController();
      let recordingCanceled = false;
      const onAbort = () => {
        recordingCanceled = true;
        abort.abort(new Error("client disconnected"));
      };
      let responseFinished = false;
      const onFinish = () => { responseFinished = true; };
      const onClose = () => { if (!responseFinished) onAbort(); };
      req.once("aborted", onAbort);
      res.once("finish", onFinish);
      res.once("close", onClose);
      let streamStarted = false;
      let recordingMode: RecordingMode = "off";
      let recordingStarted = 0;
      let recordingModel = "";
      let recordingUpstreamModel = "";
      let recordingProviderName = "";
      let recordingProvider: Provider | undefined;
      let recordingStreaming = false;
      let recordingRequestBody: Record<string, unknown> | undefined;
      let recordingResponseBytes = 0;
      let recordingResponseTruncated = false;
      const recordingResponse: Uint8Array[] = [];
      const capture = (value: Uint8Array): void => {
        recordingResponseBytes += value.byteLength;
        const limit = options.recorder?.maxBodyBytes;
        if (recordingMode === "full" && (limit === undefined || recordingResponse.reduce((n, item) => n + item.byteLength, 0) < limit)) {
          const used = recordingResponse.reduce((n, item) => n + item.byteLength, 0);
          const allowed = Math.max(0, Math.min(value.byteLength, (limit ?? Infinity) - used));
          if (allowed < value.byteLength) recordingResponseTruncated = true;
          recordingResponse.push(value.slice(0, allowed));
        } else if (recordingMode === "full" && limit !== undefined) recordingResponseTruncated = true;
      };
      try {
        if (req.method === "GET" && path.endsWith("/models")) {
          const response = await upstream(ctx.host, req, body, { ...req.headers, "accept-encoding": "identity" });
          if (passthroughOnly) {
            // Version gate: hand the GitHub catalog back untouched.
            writeResponse(res, response);
            return;
          }
          try {
            const catalog = mergeCatalog(JSON.parse(response.body.toString("utf8")), models, log);
            const output = Buffer.from(JSON.stringify(catalog));
            res.writeHead(200, { "content-type": "application/json", "content-length": output.length });
            res.end(output);
          } catch {
            writeResponse(res, response);
          }
          return;
        }
        if (req.method === "GET" && aliases.has(path.split("/").pop() ?? "")) {
          const alias = path.split("/").pop() as string;
          log(`Synthesizing catalog entry for ${alias}`);
          const model = models.find((candidate) => candidate.alias === alias)!;
          const output = Buffer.from(JSON.stringify(buildCatalogEntry(model)));
          res.writeHead(200, { "content-type": "application/json", "content-length": output.length });
          res.end(output);
          return;
        }
        if (req.method !== "POST" || !path.endsWith("/chat/completions")) {
          writeResponse(res, await upstream(ctx.host, req, body));
          return;
        }
        const parsed: unknown = body.length ? JSON.parse(body.toString("utf8")) : {};
        const requestBody = parsed as Record<string, unknown>;
        const model = typeof requestBody.model === "string" ? requestBody.model : "";
        const route = resolveRoute(model, revision, requestBody);
        options.sink?.routeDecision?.({ requestedModel: model, route: route.kind, revisionId: revision.revisionId });
        if (!aliases.has(model) || route.kind !== "custom") {
          writeResponse(res, await upstream(ctx.host, req, body));
          return;
        }
        const provider: Provider | undefined = revision.providers[route.provider];
        if (!provider) throw new Error(`unknown provider: ${route.provider}`);
        recordingMode = options.recorder?.currentMode() ?? "off";
        if (recordingMode !== "off") {
          recordingStarted = Date.now();
          recordingModel = model;
          recordingUpstreamModel = route.upstreamModel;
          recordingProviderName = route.provider;
          recordingProvider = provider;
          recordingStreaming = requestBody.stream !== false;
          recordingRequestBody = requestBody;
        }
        const rewritten = normalizeChatCompletionsBody({
          ...requestBody,
          model: route.upstreamModel,
          stream: requestBody.stream === false ? false : true,
        });
        const chunks: Uint8Array[] = [];
        const streaming = requestBody.stream !== false;
        const responses = provider.protocol === "openai-responses";
        const upstreamBody = responses
          ? chatCompletionsToResponses({ ...rewritten, stream: true })
          : rewritten;
        if (streaming) {
          streamStarted = true;
          res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache", connection: "keep-alive" });
          res.flushHeaders();
        }
        // Include the provider base path (normally /v1); URL resolution with
        // just "/chat/completions" would otherwise send this to the root.
        const providerPath = `${new URL(provider.baseUrl).pathname.replace(/\/$/, "")}/${responses ? "responses" : "chat/completions"}`;
        const translator = responses ? new ResponsesToChatCompletions() : undefined;
        await forward(provider, providerPath, upstreamBody, abort.signal, async (chunk) => {
          if (streaming) {
            if (translator) {
              for (const output of translator.push(Buffer.from(chunk).toString("utf8"))) {
                const bytes = new TextEncoder().encode(output); capture(bytes);
                await writeStream(res, bytes, abort.signal);
              }
            } else {
              capture(chunk);
              await writeStream(res, chunk, abort.signal);
            }
          } else if (translator) {
            for (const output of translator.push(Buffer.from(chunk).toString("utf8"))) chunks.push(new TextEncoder().encode(output));
          } else {
            chunks.push(chunk);
          }
        });
        if (translator) {
          const output = translator.end();
          if (streaming) {
            for (const value of output) {
              const bytes = new TextEncoder().encode(value); capture(bytes);
              await writeStream(res, bytes, abort.signal);
            }
          } else {
            chunks.push(...output.map((value) => new TextEncoder().encode(value)));
          }
        }
        if (streaming) {
          if (!res.writableEnded && !res.destroyed) res.end();
        }
        else {
          if (responses) {
            const output = Buffer.from(JSON.stringify(translatedChunksToCompletion(chunks)));
            capture(output);
            res.writeHead(200, { "content-type": "application/json", "content-length": output.length });
            res.end(output);
          } else {
            // Providers commonly stream even when the client requested a
            // non-streaming completion.  Aggregate all chunks into the
            // standard chat.completion shape rather than returning the final
            // chat.completion.chunk verbatim.
            const output = Buffer.from(JSON.stringify(translatedChunksToCompletion(chunks)));
            capture(output);
            res.writeHead(200, { "content-type": "application/json", "content-length": output.length });
            res.end(output);
          }
        }
      } catch (error) {
        if (streamStarted && !res.writableEnded && !res.destroyed) {
          const message = error instanceof Error ? error.message : "provider request failed";
          try {
            res.write(`data: ${JSON.stringify({ error: { message, type: "provider_error" } })}\n\n`);
            if (!res.writableEnded && !res.destroyed) res.write("data: [DONE]\n\n");
            if (!res.writableEnded && !res.destroyed) res.end();
          } catch {
            if (!res.writableEnded && !res.destroyed) res.end();
          }
        } else if (!res.writableEnded && !res.destroyed) {
          json(res, error instanceof ForwardError ? 502 : 400, {
            error: { message: error instanceof Error ? error.message : "provider request failed", type: "invalid_request_error" },
          });
        }
      } finally {
        if (recordingMode !== "off" && recordingProvider && options.recorder) {
          const status = res.statusCode || (res.writableEnded ? 200 : 0);
          const entry: Record<string, unknown> = {
            timestamp: new Date(recordingStarted).toISOString(), revisionId: revision.revisionId,
            host: ctx.host, requestedModel: recordingModel, upstreamModel: recordingUpstreamModel,
            provider: recordingProviderName, protocol: recordingProvider.protocol, route: "custom",
            streaming: recordingStreaming, status, durationMs: Date.now() - recordingStarted,
            requestBytes: body.length, responseBytes: recordingResponseBytes,
            canceled: recordingCanceled,
          };
          if (recordingMode === "full") {
            entry.request = { headers: req.headers, body: recordingRequestBody };
            entry.response = {
              bodyText: Buffer.concat(recordingResponse.map((x) => Buffer.from(x))).toString("utf8"),
              truncated: recordingResponseTruncated,
            };
          }
          try { await options.recorder.record(entry); } catch { /* recording never affects the client */ }
        }
        req.removeListener("aborted", onAbort);
        res.removeListener("finish", onFinish);
        res.removeListener("close", onClose);
      }
    })();
    },
    {
      setRevision(value: ConfigRevision): void {
        current = value;
        entryFor(value);
      },
      currentRevision(): ConfigRevision {
        return options.getRevision ? options.getRevision() : current;
      },
    },
  );
  return handler;
}
