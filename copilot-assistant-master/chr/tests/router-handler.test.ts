import http from "node:http";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ConfigRevision, Provider } from "../src/config/types.js";
import { ForwardError } from "../src/providers/forward.js";
import { createInterceptHandler } from "../src/intercept/router-handler.js";
import { normalizeChatCompletionsBody } from "../src/intercept/chat-normalize.js";

const provider: Provider = {
  protocol: "openai-chat-completions",
  baseUrl: "http://provider.test/v1",
  credentialRef: "env:KEY",
  timeoutsMs: { connect: 50, firstByte: 50, streamIdle: 50, total: 200 },
};
const revision: ConfigRevision = {
  revisionId: 1,
  schemaVersion: 1,
  providers: { local: provider },
  models: [{
    alias: "qwen", displayName: "Qwen", provider: "local", upstreamModel: "Qwen/Qwen3",
    capabilities: { streaming: true, tools: true, vision: false, contextWindowTokens: 32768 },
  }],
  routing: {
    unmatchedGitHubModel: "github", unknownCustomModel: "error", crossProviderFallback: "error",
    auxiliary: { policy: "block" }, githubLeg: { mode: "forward" },
  },
};

type Setup = {
  request: (method: string, path: string, body?: unknown, headers?: Record<string, string>) => Promise<{
    status: number; headers: http.IncomingHttpHeaders; text: string;
  }>;
  upstream: ReturnType<typeof vi.fn>;
  forward: ReturnType<typeof vi.fn>;
  logs: string[];
  close: () => Promise<void>;
};

async function setup(upstreamResponse?: { status: number; headers?: Record<string, string>; body: string }): Promise<Setup> {
  const upstream = vi.fn().mockResolvedValue({
    status: upstreamResponse?.status ?? 200,
    headers: upstreamResponse?.headers ?? { "content-type": "application/json" },
    body: Buffer.from(upstreamResponse?.body ?? JSON.stringify({ data: [{ id: "gpt-5" }] })),
  });
  const forward = vi.fn().mockImplementation(async (
    _provider: Provider, _path: string, _body: Record<string, unknown>, _signal: AbortSignal,
    onChunk: (chunk: Uint8Array) => void | Promise<void>,
  ) => onChunk(new TextEncoder().encode('data: {"id":"local"}\n\ndata: [DONE]\n\n')));
  const logs: string[] = [];
  const handler = createInterceptHandler(revision, {
    upstream, forward, logger: (message) => logs.push(message),
  });
  const server = http.createServer((req, res) => handler(req, res, { host: "api.githubcopilot.com" }));
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = (server.address() as { port: number }).port;
  return {
    request: (method, path, body, headers = {}) => new Promise((resolve, reject) => {
      const payload = body === undefined ? undefined : JSON.stringify(body);
      const req = http.request({ hostname: "127.0.0.1", port, method, path, headers: {
        ...(payload ? { "content-type": "application/json", "content-length": Buffer.byteLength(payload) } : {}),
        ...headers,
      }}, (res) => {
        const chunks: Buffer[] = [];
        res.on("data", (chunk) => chunks.push(Buffer.from(chunk)));
        res.on("end", () => resolve({
          status: res.statusCode ?? 0, headers: res.headers, text: Buffer.concat(chunks).toString(),
        }));
      });
      req.once("error", reject);
      if (payload) req.write(payload);
      req.end();
    }),
    upstream, forward, logs,
    close: () => new Promise((resolve) => server.close(() => resolve())),
  };
}

const setups: Setup[] = [];
afterEach(async () => { while (setups.length) await setups.pop()!.close(); });

describe("intercept router handler", () => {
  it("merges models without content encoding and preserves the catalog length", async () => {
    const s = await setup(); setups.push(s);
    const response = await s.request("GET", "/v1/models");
    expect(response.status).toBe(200);
    expect(response.headers["content-encoding"]).toBeUndefined();
    expect(Number(response.headers["content-length"])).toBe(Buffer.byteLength(response.text));
    expect(JSON.parse(response.text).data.map((entry: { id: string }) => entry.id)).toEqual(["gpt-5", "qwen"]);
  });

  it("fails open for an invalid GitHub catalog", async () => {
    const s = await setup({ status: 200, headers: { "content-type": "text/plain" }, body: "upstream" }); setups.push(s);
    const response = await s.request("GET", "/v1/models");
    expect(response.text).toBe("upstream");
    expect(response.headers["content-type"]).toBe("text/plain");
  });

  it("synthesizes an alias model without an upstream call", async () => {
    const s = await setup(); setups.push(s);
    const response = await s.request("GET", "/v1/models/qwen");
    expect(response.status).toBe(200);
    expect(JSON.parse(response.text).id).toBe("qwen");
    expect(s.upstream).not.toHaveBeenCalled();
  });

  it("streams a normalized custom request and rewrites its model", async () => {
    const s = await setup(); setups.push(s);
    const response = await s.request("POST", "/v1/chat/completions", {
      model: "qwen", messages: [{ role: "user", content: "hi" }], stream_options: { include_usage: true },
    });
    expect(response.status).toBe(200);
    expect(response.text).toContain('data: {"id":"local"}');
    expect(s.upstream).not.toHaveBeenCalled();
    expect(s.forward).toHaveBeenCalledWith(provider, "/v1/chat/completions",
      expect.objectContaining({ model: "Qwen/Qwen3", stream: true }), expect.any(AbortSignal), expect.any(Function));
    expect(s.forward.mock.calls[0][2]).not.toHaveProperty("stream_options");
  });

  it("converts a provider failure after SSE headers to an error event and DONE", async () => {
    const s = await setup(); setups.push(s);
    s.forward.mockImplementationOnce(async (_p: Provider, _path: string, _b: Record<string, unknown>, _signal: AbortSignal,
      onChunk: (chunk: Uint8Array) => void | Promise<void>) => {
      await onChunk(new TextEncoder().encode("data: {\"part\":true}\n\n"));
      throw new ForwardError("upstream_error", "status 502");
    });
    const response = await s.request("POST", "/v1/chat/completions", { model: "qwen" });
    expect(response.status).toBe(200);
    expect(response.text).toContain('"type":"provider_error"');
    expect(response.text).toContain("data: [DONE]");
  });

  it("relays non-stream custom JSON and returns 502 before writing", async () => {
    const s = await setup(); setups.push(s);
    const response = await s.request("POST", "/v1/chat/completions", { model: "qwen", stream: false });
    expect(response.status).toBe(200);
    expect(JSON.parse(response.text).id).toBe("local");
    s.forward.mockRejectedValueOnce(new ForwardError("upstream_error", "status 503"));
    const failed = await s.request("POST", "/v1/chat/completions", { model: "qwen", stream: false });
    expect(failed.status).toBe(502);
  });

  it("passes GitHub, non-chat, and other paths through unchanged", async () => {
    const s = await setup(); setups.push(s);
    for (const [method, path, body] of [
      ["POST", "/v1/chat/completions", { model: "gpt-5" }],
      ["POST", "/v1/responses", { model: "qwen" }],
      ["GET", "/v1/other", undefined],
    ] as const) await s.request(method, path, body);
    expect(s.upstream).toHaveBeenCalledTimes(3);
    expect(s.forward).not.toHaveBeenCalled();
  });

  it("does not log the inbound authorization value", async () => {
    const s = await setup(); setups.push(s);
    await s.request("POST", "/v1/chat/completions", { model: "qwen" }, { authorization: "Bearer secret-inbound-token" });
    expect(s.logs.join("\n")).not.toContain("secret-inbound-token");
  });
});

describe("normalizeChatCompletionsBody", () => {
  it("removes stream_options but preserves messages, tools, choice, and sampling", () => {
    const body = {
      model: "qwen", messages: [{ role: "user", content: "hi" }], tools: [{ type: "function" }],
      tool_choice: "auto", stream_options: { include_usage: true }, temperature: 0.2, top_p: 0.8,
      max_tokens: 100, stop: ["x"], n: 1, stream: true,
    };
    const normalized = normalizeChatCompletionsBody(body);
    expect(normalized).not.toHaveProperty("stream_options");
    expect(normalized).toMatchObject({ messages: body.messages, tools: body.tools, tool_choice: "auto",
      temperature: 0.2, top_p: 0.8, max_tokens: 100, stop: ["x"], n: 1, stream: true });
  });

  it("is idempotent and does not mutate the input", () => {
    const body = { model: "qwen", stream_options: { include_usage: true } };
    const once = normalizeChatCompletionsBody(body);
    expect(normalizeChatCompletionsBody(once)).toEqual(once);
    expect(body.stream_options).toBeDefined();
  });
});
