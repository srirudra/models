import { EventEmitter } from "node:events";
import http from "node:http";
import { describe, expect, it, vi } from "vitest";
import { launchIntercept } from "../src/launcher/intercept-launch.js";
import type { InterceptHandler } from "../src/intercept/router-handler.js";
import type { ConfigRevision, Provider } from "../src/config/types.js";

const provider: Provider = {
  protocol: "openai-chat-completions",
  baseUrl: "http://provider.test/v1",
  credentialRef: "env:CHR_TEST_KEY",
  timeoutsMs: { connect: 50, firstByte: 50, streamIdle: 50, total: 200 },
};

function revision(untestedVersionPolicy: string): ConfigRevision {
  return {
    revisionId: 1,
    schemaVersion: 1,
    compatibility: { testedCliVersions: ["1.0.88"], untestedVersionPolicy },
    providers: { local: provider },
    models: [{
      alias: "qwen", displayName: "Qwen", provider: "local", upstreamModel: "Qwen/Qwen3",
      capabilities: { streaming: true, tools: true, vision: false, contextWindowTokens: 32768 },
    }],
    routing: {
      unmatchedGitHubModel: "preserve-original-route", unknownCustomModel: "error",
      crossProviderFallback: "disabled", auxiliary: { policy: "block" }, githubLeg: { mode: "fail-closed" },
    },
    recording: {
      defaultMode: "off", retentionDays: 1, maxTotalMiB: 1, maxBodyMiBPerRequest: 1, onWriteFailure: "ignore",
    },
  } as ConfigRevision;
}

type Session = {
  banner: string;
  spawned: number;
  request: (method: string, path: string, body?: unknown) => Promise<{ status: number; text: string }>;
  upstream: ReturnType<typeof vi.fn>;
  forward: ReturnType<typeof vi.fn>;
  finish: () => Promise<number>;
};

async function launch(
  policy: string,
  detectVersion: () => Promise<string | undefined>,
): Promise<Session> {
  const child = new EventEmitter();
  const upstream = vi.fn().mockResolvedValue({
    status: 200,
    headers: { "content-type": "application/json" },
    body: Buffer.from(JSON.stringify({ data: [{ id: "gpt-5" }] })),
  });
  const forward = vi.fn().mockImplementation(async (
    _provider: Provider, _path: string, _body: unknown, _signal: AbortSignal,
    onChunk: (chunk: Uint8Array) => void | Promise<void>,
  ) => onChunk(new TextEncoder().encode('data: {"id":"local"}\n\ndata: [DONE]\n\n')));
  let banner = "";
  let spawned = 0;
  let handler: InterceptHandler | undefined;
  let announceSpawn = (): void => undefined;
  const spawnedPromise = new Promise<void>((resolve) => { announceSpawn = resolve; });
  const done = launchIntercept({
    revision: revision(policy),
    env: { CHR_TEST_KEY: "key", CHR_CONFIG_PATH: "C:\\chr-test\\config.json" },
    watch: false,
    childCommand: "copilot-test",
    detectVersion,
    handlerOverrides: { upstream, forward },
    startProxy: async ({ handleRequest }) => {
      handler = handleRequest;
      return { port: 45678, close: async () => undefined };
    },
    writeCa: async () => "C:\\chr-test\\ca.pem",
    removeCa: async () => undefined,
    spawnChild: () => { spawned += 1; announceSpawn(); return child; },
    writeStderr: (message) => { banner += message; },
  });
  await spawnedPromise;
  const server = http.createServer((req, res) => handler!(req, res, { host: "api.githubcopilot.com" }));
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const port = (server.address() as { port: number }).port;
  return {
    get banner() { return banner; },
    get spawned() { return spawned; },
    upstream,
    forward,
    request: (method, path, body) => new Promise((resolve, reject) => {
      const payload = body === undefined ? undefined : JSON.stringify(body);
      const req = http.request({
        hostname: "127.0.0.1", port, method, path,
        headers: payload ? { "content-type": "application/json", "content-length": Buffer.byteLength(payload) } : {},
      }, (res) => {
        const chunks: Buffer[] = [];
        res.on("data", (chunk) => chunks.push(Buffer.from(chunk)));
        res.on("end", () => resolve({ status: res.statusCode ?? 0, text: Buffer.concat(chunks).toString() }));
      });
      req.once("error", reject);
      if (payload) req.write(payload);
      req.end();
    }),
    finish: async () => {
      child.emit("exit", 0);
      const code = await done;
      await new Promise<void>((resolve) => server.close(() => resolve()));
      return code;
    },
  };
}

describe("AT-16 intercept CLI version gate", () => {
  it("injects custom models for a tested Copilot CLI version", async () => {
    const session = await launch("block-custom-routing", async () => "1.0.88");
    const catalog = await session.request("GET", "/v1/models");
    expect(JSON.parse(catalog.text).data.map((entry: { id: string }) => entry.id)).toEqual(["gpt-5", "qwen"]);
    const chat = await session.request("POST", "/v1/chat/completions", { model: "qwen", messages: [] });
    expect(chat.status).toBe(200);
    expect(session.forward).toHaveBeenCalledTimes(1);
    expect(session.banner).toContain("injected 1 custom model (qwen)");
    expect(session.banner).not.toContain("DISABLED");
    expect(session.spawned).toBe(1);
    expect(await session.finish()).toBe(0);
  });

  it("falls back to passthrough-only for an untested version under block-custom-routing", async () => {
    const session = await launch("block-custom-routing", async () => "9.9.9");
    const catalog = await session.request("GET", "/v1/models");
    expect(JSON.parse(catalog.text)).toEqual({ data: [{ id: "gpt-5" }] });
    const chat = await session.request("POST", "/v1/chat/completions", { model: "qwen", messages: [] });
    expect(chat.status).toBe(200);
    expect(session.forward).not.toHaveBeenCalled();
    expect(session.upstream).toHaveBeenCalledTimes(2);
    expect(session.banner).toContain(
      'CHR intercept: Copilot CLI 9.9.9 is untested; custom models DISABLED (GitHub models only). Run "chr verify", then add "9.9.9" to compatibility.testedCliVersions in C:\\chr-test\\config.json to enable.',
    );
    expect(session.banner).toContain("injected 0 custom models (none)");
    expect(session.spawned).toBe(1);
    expect(await session.finish()).toBe(0);
  });

  it("treats an undetectable version as untested", async () => {
    const session = await launch("block-custom-routing", async () => undefined);
    const catalog = await session.request("GET", "/v1/models");
    expect(JSON.parse(catalog.text)).toEqual({ data: [{ id: "gpt-5" }] });
    expect(session.banner).toContain("Copilot CLI unknown is untested; custom models DISABLED");
    expect(session.spawned).toBe(1);
    expect(await session.finish()).toBe(0);
  });

  it("warns but keeps custom models for a warn policy", async () => {
    const session = await launch("warn", async () => "9.9.9");
    const catalog = await session.request("GET", "/v1/models");
    expect(JSON.parse(catalog.text).data.map((entry: { id: string }) => entry.id)).toEqual(["gpt-5", "qwen"]);
    const chat = await session.request("POST", "/v1/chat/completions", { model: "qwen", messages: [] });
    expect(chat.status).toBe(200);
    expect(session.forward).toHaveBeenCalledTimes(1);
    expect(session.banner).toContain("warning - Copilot CLI 9.9.9 is untested; custom models stay enabled");
    expect(session.spawned).toBe(1);
    expect(await session.finish()).toBe(0);
  });
});
