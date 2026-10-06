import { EventEmitter } from "node:events";
import http from "node:http";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { launchIntercept } from "../src/launcher/intercept-launch.js";
import { readConfigFile } from "../src/launcher/config-file.js";
import type { InterceptHandler } from "../src/intercept/router-handler.js";

type MockProvider = { port: number; hits: () => number; release: () => void; close: () => Promise<void> };

/** Minimal OpenAI chat-completions SSE provider; can hold a stream open. */
async function startProvider(name: string, options: { slow?: boolean } = {}): Promise<MockProvider> {
  let release = (): void => undefined;
  let hits = 0;
  const released = new Promise<void>((resolve) => { release = () => resolve(); });
  const server = http.createServer((req, res) => {
    hits += 1;
    res.writeHead(200, { "content-type": "text/event-stream" });
    res.write(`data: ${JSON.stringify({ id: name, choices: [{ index: 0, delta: { content: name } }] })}\n\n`);
    const finish = () => { res.write("data: [DONE]\n\n"); res.end(); };
    if (options.slow) void released.then(finish);
    else finish();
    void req;
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  return {
    port: (server.address() as { port: number }).port,
    hits: () => hits,
    release,
    close: () => new Promise((resolve) => { release(); server.close(() => resolve()); }),
  };
}

function configFor(providerPort: number, aliases: string[]): string {
  return JSON.stringify({
    schemaVersion: 1,
    compatibility: { testedCliVersions: [], untestedVersionPolicy: "warn" },
    providers: {
      a: {
        protocol: "openai-chat-completions",
        baseUrl: `http://127.0.0.1:${providerPort}/v1`,
        credentialRef: "env:CHR_TEST_KEY",
        timeoutsMs: { connect: 2000, firstByte: 5000, streamIdle: 5000, total: 10000 },
      },
    },
    models: aliases.map((alias) => ({
      alias, displayName: alias, provider: "a", upstreamModel: `upstream-${alias}`,
      capabilities: { streaming: true, tools: false, vision: false, contextWindowTokens: 32768 },
    })),
    routing: {
      unmatchedGitHubModel: "preserve-original-route", unknownCustomModel: "error",
      crossProviderFallback: "disabled", auxiliary: { policy: "block" }, githubLeg: { mode: "fail-closed" },
    },
    recording: {
      defaultMode: "off", retentionDays: 1, maxTotalMiB: 1, maxBodyMiBPerRequest: 1, onWriteFailure: "ignore",
    },
  }, null, 2);
}

async function waitFor(predicate: () => boolean, timeoutMs = 3000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error("condition was not met within the polling window");
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
}

const cleanups: Array<() => Promise<void>> = [];
afterEach(async () => { while (cleanups.length) await cleanups.pop()!(); });

describe("AT-14 configuration hot reload", () => {
  it("swaps validated revisions, rejects invalid ones, and keeps in-flight requests on their revision", async () => {
    const providerA = await startProvider("PROVIDER-A", { slow: true });
    const providerB = await startProvider("PROVIDER-B");
    cleanups.push(() => providerA.close(), () => providerB.close());
    const directory = await mkdtemp(path.join(tmpdir(), "chr-hot-reload-"));
    cleanups.push(() => rm(directory, { recursive: true, force: true }));
    const configPath = path.join(directory, "config.json");
    await writeFile(configPath, configFor(providerA.port, ["a1"]), "utf8");
    const loaded = await readConfigFile(configPath);
    expect(loaded.revision).toBeDefined();

    const child = new EventEmitter();
    const upstream = vi.fn().mockResolvedValue({
      status: 200,
      headers: { "content-type": "application/json" },
      body: Buffer.from(JSON.stringify({ data: [{ id: "gpt-5" }] })),
    });
    let banner = "";
    let handler: InterceptHandler | undefined;
    let announceSpawn = (): void => undefined;
    const spawned = new Promise<void>((resolve) => { announceSpawn = resolve; });
    const done = launchIntercept({
      revision: loaded.revision!,
      env: { CHR_TEST_KEY: "key" },
      configPath,
      childCommand: "copilot-test",
      detectVersion: async () => "1.0.88",
      handlerOverrides: { upstream },
      startProxy: async ({ handleRequest }) => {
        handler = handleRequest;
        return { port: 45678, close: async () => undefined };
      },
      writeCa: async () => path.join(directory, "ca.pem"),
      removeCa: async () => undefined,
      spawnChild: () => { announceSpawn(); return child; },
      writeStderr: (message) => { banner += message; },
    });
    await spawned;
    const server = http.createServer((req, res) => handler!(req, res, { host: "api.githubcopilot.com" }));
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const port = (server.address() as { port: number }).port;
    cleanups.push(async () => {
      child.emit("exit", 0);
      await done;
      await new Promise<void>((resolve) => server.close(() => resolve()));
    });

    const request = (method: string, urlPath: string, body?: unknown) => new Promise<{
      status: number; text: string;
    }>((resolve, reject) => {
      const payload = body === undefined ? undefined : JSON.stringify(body);
      const req = http.request({
        hostname: "127.0.0.1", port, method, path: urlPath,
        headers: payload ? { "content-type": "application/json", "content-length": Buffer.byteLength(payload) } : {},
      }, (res) => {
        const chunks: Buffer[] = [];
        res.on("data", (chunk) => chunks.push(Buffer.from(chunk)));
        res.on("end", () => resolve({ status: res.statusCode ?? 0, text: Buffer.concat(chunks).toString() }));
      });
      req.once("error", reject);
      if (payload) req.write(payload);
      req.end();
    });

    const revisionA = handler!.currentRevision().revisionId;

    // An invalid configuration is rejected and the old revision stays live.
    await writeFile(configPath, "{ not json", "utf8");
    await waitFor(() => banner.includes("configuration reload rejected"));
    expect(banner).toContain(`keeping revision ${revisionA}`);
    expect(handler!.currentRevision().revisionId).toBe(revisionA);
    const catalogDuringRejection = await request("GET", "/v1/models");
    expect(JSON.parse(catalogDuringRejection.text).data.map((entry: { id: string }) => entry.id))
      .toEqual(["gpt-5", "a1"]);

    // Start a slow streaming request while revision A is still current.
    const inflight = request("POST", "/v1/chat/completions", { model: "a1", messages: [], stream: true });
    await waitFor(() => providerA.hits() > 0);

    // A valid configuration is adopted; new requests see the new revision.
    await writeFile(configPath, configFor(providerB.port, ["a1", "b1"]), "utf8");
    await waitFor(() => banner.includes("configuration reloaded"));
    const revisionB = handler!.currentRevision().revisionId;
    expect(revisionB).toBeGreaterThan(revisionA);
    expect(banner).toContain(`configuration reloaded (revision ${revisionB}): 2 custom models (a1, b1)`);

    const catalogAfterReload = await request("GET", "/v1/models");
    expect(JSON.parse(catalogAfterReload.text).data.map((entry: { id: string }) => entry.id))
      .toEqual(["gpt-5", "a1", "b1"]);

    // A request started after the reload uses revision B's provider...
    const afterReload = await request("POST", "/v1/chat/completions", { model: "b1", messages: [], stream: true });
    expect(afterReload.text).toContain("PROVIDER-B");

    // ...while the in-flight request finishes against revision A's provider.
    providerA.release();
    const inflightResult = await inflight;
    expect(inflightResult.text).toContain("PROVIDER-A");
    expect(inflightResult.text).not.toContain("PROVIDER-B");
  }, 20_000);
});
