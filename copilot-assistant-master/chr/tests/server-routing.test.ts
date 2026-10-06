import http from "node:http";
import { afterEach, describe, expect, it } from "vitest";
import { loadConfig } from "../src/config/config.js";
import { createServer, listen } from "../src/server/server.js";

const config = (baseUrl: string) => loadConfig({
  schemaVersion: 1,
  compatibility: { testedCliVersions: ["1.0.88"], untestedVersionPolicy: "allow" },
  providers: {
    local: {
      protocol: "openai-chat-completions", baseUrl, credentialRef: "env:SERVER_TEST_KEY",
      timeoutsMs: { connect: 100, firstByte: 100, streamIdle: 100, total: 1000 },
    },
  },
  models: [{
    alias: "qwen", displayName: "Qwen", provider: "local", upstreamModel: "qwen-upstream",
    capabilities: { streaming: true, tools: true, vision: false, contextWindowTokens: null },
  }],
  routing: {
    unmatchedGitHubModel: "preserve-original-route", unknownCustomModel: "error",
    crossProviderFallback: "disabled", auxiliary: { policy: "block" }, githubLeg: { mode: "fail-closed" },
  },
  recording: { defaultMode: "off", retentionDays: 7, maxTotalMiB: 1, maxBodyMiBPerRequest: 1, onWriteFailure: "continue" },
}).revision!;

describe("CHR server routing", () => {
  let upstream: http.Server | undefined;
  let chr: http.Server | undefined;

  afterEach(() => {
    upstream?.close();
    chr?.close();
    delete process.env.SERVER_TEST_KEY;
  });

  it("rewrites the alias and injects only the configured credential", async () => {
    let received: Record<string, unknown> | undefined;
    let authorization: string | undefined;
    upstream = http.createServer((req, res) => {
      const chunks: Buffer[] = [];
      req.on("data", (chunk) => chunks.push(chunk));
      req.on("end", () => {
        received = JSON.parse(Buffer.concat(chunks).toString("utf8"));
        authorization = req.headers.authorization;
        res.writeHead(200, { "content-type": "text/event-stream" });
        res.end("data: [DONE]\n\n");
      });
    });
    await new Promise<void>((resolve) => upstream!.listen(0, "127.0.0.1", resolve));
    const upstreamPort = (upstream.address() as { port: number }).port;
    process.env.SERVER_TEST_KEY = "loopback-secret";
    chr = createServer(config(`http://127.0.0.1:${upstreamPort}/v1`));
    const chrPort = await listen(chr);

    const response = await fetch(`http://127.0.0.1:${chrPort}/v1/chat/completions`, {
      method: "POST",
      headers: { authorization: "Bearer inbound-secret", "content-type": "application/json" },
      body: JSON.stringify({ model: "qwen", messages: [], stream: true }),
    });
    await response.text();
    expect(response.status).toBe(200);
    expect(received?.model).toBe("qwen-upstream");
    expect(authorization).toBe("Bearer loopback-secret");
  });
});
