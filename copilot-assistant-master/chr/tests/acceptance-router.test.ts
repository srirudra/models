import http from "node:http";
import { readFileSync } from "node:fs";
import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import type { ConfigRevision, Provider } from "../src/config/types.js";
import { primeCredentials } from "../src/config/config.js";
import { createInterceptHandler } from "../src/intercept/router-handler.js";

const base: Provider = {
  protocol: "openai-chat-completions", baseUrl: "http://127.0.0.1:1/v1", credentialRef: "env:AT_KEY",
  timeoutsMs: { connect: 200, firstByte: 200, streamIdle: 200, total: 300 },
};
const revision = (p: Provider): ConfigRevision => ({
  revisionId: Date.now(), schemaVersion: 1, providers: { custom: p },
  models: [{ alias: "at-model", displayName: "AT model", provider: "custom", upstreamModel: "real-model",
    capabilities: { streaming: true, tools: true, vision: false, contextWindowTokens: 8192 } }],
  routing: { unmatchedGitHubModel: "github", unknownCustomModel: "error", crossProviderFallback: "error",
    auxiliary: { policy: "block" }, githubLeg: { mode: "forward" } },
});
type ProviderServer = { port: number; requests: http.IncomingMessage[]; server: http.Server; close(): Promise<void> };
async function providerServer(handler: (req: http.IncomingMessage, res: http.ServerResponse) => void): Promise<ProviderServer> {
  const requests: http.IncomingMessage[] = [];
  const server = http.createServer((req, res) => { requests.push(req); handler(req, res); });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  return { port: (server.address() as { port: number }).port, requests, server,
    close: () => new Promise((resolve) => server.close(() => resolve())) };
}
async function gateway(r: ConfigRevision, upstream = vi.fn().mockResolvedValue({
  status: 200, headers: { "content-type": "application/json" }, body: Buffer.from(JSON.stringify({ data: [{ id: "github" }] })),
})) {
  const handler = createInterceptHandler(r, { upstream });
  const server = http.createServer((req, res) => handler(req, res, { host: "api.individual.githubcopilot.com" }));
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = (server.address() as { port: number }).port;
  const request = (body: unknown, headers: Record<string, string> = {}) => new Promise<{ status: number; text: string }>((resolve, reject) => {
    const data = JSON.stringify(body);
    const req = http.request({ hostname: "127.0.0.1", port, path: "/v1/chat/completions", method: "POST",
      headers: { "content-type": "application/json", "content-length": Buffer.byteLength(data), ...headers } }, (res) => {
      const chunks: Buffer[] = []; res.on("data", (x) => chunks.push(Buffer.from(x)));
      res.on("end", () => resolve({ status: res.statusCode ?? 0, text: Buffer.concat(chunks).toString() }));
    });
    req.once("error", reject); req.end(data);
  });
  const models = () => new Promise<string>((resolve, reject) => {
    const req = http.request({ hostname: "127.0.0.1", port, path: "/v1/models" }, (res) => {
      const chunks: Buffer[] = []; res.on("data", (x) => chunks.push(Buffer.from(x)));
      res.on("end", () => resolve(Buffer.concat(chunks).toString()));
    }); req.once("error", reject); req.end();
  });
  return { port, request, models, upstream, close: () => new Promise<void>((resolve) => server.close(() => resolve())) };
}
const sse = (text = "ok") => `data: ${JSON.stringify({ id: "x", object: "chat.completion.chunk", choices: [{ index: 0, delta: { content: text }, finish_reason: null }] })}\n\n` +
  `data: ${JSON.stringify({ id: "x", object: "chat.completion.chunk", choices: [{ index: 0, delta: {}, finish_reason: "stop" }] })}\n\ndata: [DONE]\n\n`;

beforeEach(async () => { await primeCredentials(revision(base), { AT_KEY: "provider-secret" }); });

describe("CHR real-provider acceptance", () => {
  it("AT-05 routes an alias to custom and an unconfigured model to GitHub", async () => {
    const p = await providerServer((_q, res) => { res.writeHead(200, { "content-type": "text/event-stream" }); res.end(sse()); });
    const r = revision({ ...base, baseUrl: `http://127.0.0.1:${p.port}/v1` }); await primeCredentials(r, { AT_KEY: "provider-secret" });
    const g = await gateway(r); try {
      expect((await g.request({ model: "other", stream: false })).status).toBe(200);
      expect((await g.request({ model: "at-model" })).status).toBe(200);
      expect(p.requests).toHaveLength(1); expect(g.upstream).toHaveBeenCalledTimes(1);
    } finally { await g.close(); await p.close(); }
  });

  it("AT-07 forwards only the configured provider credential and no GitHub identity headers", async () => {
    let received = ""; const p = await providerServer((req, res) => {
      received = JSON.stringify(req.headers); res.writeHead(200, { "content-type": "text/event-stream" }); res.end(sse());
    });
    const r = revision({ ...base, baseUrl: `http://127.0.0.1:${p.port}/v1` }); await primeCredentials(r, { AT_KEY: "provider-secret" });
    const g = await gateway(r); try {
      await g.request({ model: "at-model" }, { authorization: "Bearer GITHUB-TOKEN", cookie: "cookie",
        "x-github-api-version": "version", "copilot-integration-id": "integration", "editor-version": "editor", "x-request-id": "request" });
      const headers = JSON.parse(received) as Record<string, string>;
      expect(headers.authorization).toBe("Bearer provider-secret");
      for (const key of ["cookie", "x-github-api-version", "copilot-integration-id", "editor-version", "x-request-id"]) expect(headers[key]).toBeUndefined();
      expect(received).not.toContain("GITHUB-TOKEN");
    } finally { await g.close(); await p.close(); }
  });

  it("AT-06 reports 401, 429, and timeout failures without GitHub fallback", async () => {
    for (const mode of [401, 429, "timeout"] as const) {
      const p = await providerServer((_q, res) => {
        if (mode === "timeout") return;
        res.writeHead(mode, { "content-type": "application/json" }); res.end('{"error":{"message":"failure"}}');
      });
      const r = revision({ ...base, baseUrl: `http://127.0.0.1:${p.port}/v1`,
        timeoutsMs: { connect: 200, firstByte: 80, streamIdle: 80, total: 120 } }); await primeCredentials(r, { AT_KEY: "provider-secret" });
      const g = await gateway(r); try {
        const result = await g.request({ model: "at-model", stream: false });
        expect(result.status).not.toBe(200); expect(g.upstream).not.toHaveBeenCalled(); expect(p.requests).toHaveLength(1);
      } finally { await g.close(); await p.close(); }
    }
  });

  it("AT-06 rejects malformed/truncated provider SSE with a visible error", async () => {
    const p = await providerServer((_q, res) => { res.writeHead(200, { "content-type": "text/event-stream" }); res.end('data: {"choices":['); });
    const r = revision({ ...base, baseUrl: `http://127.0.0.1:${p.port}/v1` }); await primeCredentials(r, { AT_KEY: "provider-secret" });
    const g = await gateway(r); try {
      const result = await g.request({ model: "at-model" }); expect(result.text).toContain("provider_error");
    } finally { await g.close(); await p.close(); }
  });

  it("AT-23 validates streaming and non-streaming OpenAI chat completion shapes", async () => {
    const p = await providerServer((_q, res) => { res.writeHead(200, { "content-type": "text/event-stream" }); res.end(sse()); });
    const r = revision({ ...base, baseUrl: `http://127.0.0.1:${p.port}/v1` }); await primeCredentials(r, { AT_KEY: "provider-secret" });
    const g = await gateway(r); try {
      const stream = await g.request({ model: "at-model" }); const frame = JSON.parse(stream.text.split("\n")[0].slice(6));
      expect(frame).toMatchObject({ id: expect.any(String), object: "chat.completion.chunk", choices: [{ delta: expect.any(Object), finish_reason: null }] });
      expect(stream.text).toContain("data: [DONE]");
      const complete = await g.request({ model: "at-model", stream: false }); const value = JSON.parse(complete.text);
      expect(value).toMatchObject({ id: expect.any(String), object: "chat.completion", choices: [{ message: expect.any(Object), finish_reason: "stop" }] });
    } finally { await g.close(); await p.close(); }
  });

  it("AT-22 merges the GitHub catalog and keeps provider secrets out", async () => {
    const p = await providerServer((_q, res) => { res.writeHead(500); res.end(); });
    const r = revision({ ...base, baseUrl: `http://127.0.0.1:${p.port}/v1` }); await primeCredentials(r, { AT_KEY: "provider-secret" });
    const g = await gateway(r); try {
      const value = JSON.parse(await g.models()) as { data: Array<Record<string, unknown>> };
      expect(value.data.map((x) => x.id)).toEqual(["github", "at-model"]);
      expect(JSON.stringify(value)).not.toContain("provider-secret");
      expect(JSON.stringify(value)).not.toContain("127.0.0.1");
    } finally { await g.close(); await p.close(); }
  });

  it("AT-12 handles byte-fragmented UTF-8 chat and Responses fixture streams", async () => {
    const chatBytes = Buffer.from(sse("héllo 😀 世界") +
      `data: ${JSON.stringify({ id: "x", object: "chat.completion.chunk", choices: [{ index: 0, delta: { tool_calls: [
        { index: 0, id: "a", type: "function", function: { name: "one", arguments: "{" } },
        { index: 1, id: "b", type: "function", function: { name: "two", arguments: "[]" } },
      ] }, finish_reason: null }] })}\n\n` + "data: [DONE]\n\n");
    let splitInsideUtf8 = false;
    const cp = await providerServer((_q, res) => {
      res.writeHead(200, { "content-type": "text/event-stream" });
      let offset = 0; let n = 0; while (offset < chatBytes.length) {
        const size = [1, 2, 7, 3, 5, 4, 6][n++ % 7];
        const end = Math.min(chatBytes.length, offset + size);
        if (end < chatBytes.length && ((chatBytes[end - 1]! >= 0xc0 && chatBytes[end]! >= 0x80) ||
          (chatBytes[end - 1]! >= 0x80 && chatBytes[end]! >= 0x80))) splitInsideUtf8 = true;
        res.write(chatBytes.subarray(offset, end)); offset = end;
      }
      res.end(chatBytes.subarray(offset));
    });
    const r = revision({ ...base, baseUrl: `http://127.0.0.1:${cp.port}/v1` }); await primeCredentials(r, { AT_KEY: "provider-secret" });
    const g = await gateway(r); try {
      const result = await g.request({ model: "at-model" });
      expect(splitInsideUtf8).toBe(true); expect(result.text).toContain("héllo 😀 世界");
      expect(result.text).toContain("\"index\":1");
    } finally { await g.close(); await cp.close(); }

    for (const fixture of ["responses-text-stream.txt", "responses-toolcall-stream.txt"]) {
      const bytes = readFileSync(`C:\\Users\\visha_f65p61m\\repos\\copilot-assistant\\spike5\\${fixture}`);
      const rp = await providerServer(async (_q, res) => {
        res.writeHead(200, { "content-type": "text/event-stream" });
        for (let i = 0; i < bytes.length; i += 1) { res.write(bytes.subarray(i, i + 1)); await new Promise((resolve) => setTimeout(resolve, 0)); }
        res.end();
      });
      const rr = revision({ ...base, protocol: "openai-responses", baseUrl: `http://127.0.0.1:${rp.port}/v1` }); await primeCredentials(rr, { AT_KEY: "provider-secret" });
      const gg = await gateway(rr); try {
        const result = await gg.request({ model: "at-model" });
        expect(result.status).toBe(200); expect(result.text).toContain("data: [DONE]");
      } finally { await gg.close(); await rp.close(); }
    }
  });
});
