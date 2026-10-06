import http from "node:http";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { loadConfig } from "../src/config/config.js";
import { createServer, listen } from "../src/server/server.js";

function closeServer(server: http.Server): Promise<void> {
  return new Promise((resolve, reject) => {
    server.close(error => error ? reject(error) : resolve());
  });
}

describe("client cancellation", () => {
  let upstream: http.Server;
  let chr: http.Server;
  let chrPort: number;
  let upstreamClosed: Promise<void>;
  let resolveUpstreamClosed: () => void;

  beforeAll(async () => {
    upstreamClosed = new Promise(resolve => {
      resolveUpstreamClosed = resolve;
    });
    upstream = http.createServer((req, res) => {
      req.once("aborted", resolveUpstreamClosed);
      res.once("close", resolveUpstreamClosed);
      res.writeHead(200, { "content-type": "text/event-stream" });
      res.flushHeaders();
      res.write('data: {"choices":[{"delta":{"content":"first"}}]}\n\n');
      res.write('data: {"choices":[{"delta":{"content":"second"}}]}\n\n');
    });
    await new Promise<void>(resolve => upstream.listen(0, "127.0.0.1", resolve));
    const upstreamPort = (upstream.address() as { port: number }).port;
    const revision = loadConfig({
      schemaVersion: 1,
      providers: {
        p: {
          protocol: "openai-chat-completions",
          baseUrl: `http://127.0.0.1:${upstreamPort}/v1`,
          credentialRef: "env:SECRET",
          timeoutsMs: { connect: 500, firstByte: 500, streamIdle: 5000, total: 10000 },
        },
      },
      models: [{
        alias: "custom/a",
        displayName: "A",
        provider: "p",
        upstreamModel: "real-a",
        capabilities: {
          streaming: true,
          tools: true,
          vision: false,
          contextWindowTokens: null,
        },
      }],
      routing: {
        unmatchedGitHubModel: "preserve-original-route",
        unknownCustomModel: "error",
        crossProviderFallback: "disabled",
        auxiliary: { policy: "block" },
        githubLeg: { mode: "fail-closed" },
      },
    }).revision!;
    chr = createServer(revision);
    chrPort = await listen(chr);
  });

  afterAll(async () => {
    chr.closeAllConnections();
    upstream.closeAllConnections();
    await closeServer(chr);
    await closeServer(upstream);
  });

  it("propagates an aborted client request to the upstream", async () => {
    const controller = new AbortController();
    const response = await fetch(`http://127.0.0.1:${chrPort}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: "custom/a", messages: [], stream: true }),
      signal: controller.signal,
    });
    const reader = response.body!.getReader();
    const firstChunk = await reader.read();
    expect(firstChunk.done).toBe(false);
    controller.abort();
    await reader.cancel().catch(() => undefined);

    const observed = await Promise.race([
      upstreamClosed.then(() => true),
      new Promise<boolean>(resolve => setTimeout(() => resolve(false), 2000)),
    ]);
    expect(observed).toBe(true);
  });
});
