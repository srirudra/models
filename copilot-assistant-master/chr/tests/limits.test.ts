import http from "node:http";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { loadConfig } from "../src/config/config.js";
import { createServer, listen } from "../src/server/server.js";

function closeServer(server: http.Server): Promise<void> {
  return new Promise((resolve, reject) => {
    server.close(error => error ? reject(error) : resolve());
  });
}

describe("server resource limits", () => {
  let upstream: http.Server;
  let chr: http.Server;
  let chrPort: number;
  let upstreamHits = 0;
  const heldResponses: http.ServerResponse[] = [];
  let allHeld: Promise<void>;
  let resolveAllHeld: () => void;

  beforeAll(async () => {
    allHeld = new Promise(resolve => {
      resolveAllHeld = resolve;
    });
    upstream = http.createServer((req, res) => {
      upstreamHits++;
      heldResponses.push(res);
      res.writeHead(200, { "content-type": "text/event-stream" });
      res.flushHeaders();
      res.write('data: {"choices":[{"delta":{"content":"held"}}]}\n\n');
      if (heldResponses.length === 32) {
        resolveAllHeld();
      }
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
          timeoutsMs: { connect: 500, firstByte: 500, streamIdle: 10000, total: 20000 },
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
    for (const response of heldResponses) {
      if (!response.writableEnded) {
        response.end("data: [DONE]\n\n");
      }
    }
    chr.closeAllConnections();
    upstream.closeAllConnections();
    await closeServer(chr);
    await closeServer(upstream);
  });

  it("rejects an oversized body before contacting upstream", async () => {
    upstreamHits = 0;
    const oversizedBody = JSON.stringify({
      model: "custom/a",
      messages: [],
      padding: "x".repeat(8 * 1024 * 1024),
    });
    const response = await fetch(`http://127.0.0.1:${chrPort}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: oversizedBody,
    });

    expect(response.status).toBe(413);
    expect(upstreamHits).toBe(0);
  });

  it("rejects requests beyond the fixed concurrency limit", async () => {
    const requests = Array.from({ length: 32 }, () => fetch(
      `http://127.0.0.1:${chrPort}/v1/chat/completions`,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ model: "custom/a", messages: [], stream: true }),
      },
    ));
    await allHeld;
    const extra = await fetch(`http://127.0.0.1:${chrPort}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: "custom/a", messages: [], stream: true }),
    });

    expect(upstreamHits).toBe(32);
    expect(extra.status).toBe(429);
    expect(await extra.json()).toMatchObject({
      error: {
        message: "concurrency limit exceeded",
        type: "rate_limit_error",
      },
    });

    for (const response of heldResponses) {
      if (!response.writableEnded) {
        response.end("data: [DONE]\n\n");
      }
    }
    await Promise.all(requests);
  });
});
