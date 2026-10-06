import { afterAll, beforeAll, describe, expect, it } from "vitest";
import http from "node:http";
import { loadConfig, redactEffective, validateConfig } from "../src/config/config.js";
import { classifyRequest } from "../src/routing/classifier.js";
import { resolveRoute } from "../src/routing/resolve.js";
import { SSEParser } from "../src/providers/sse.js";
import { createServer, listen } from "../src/server/server.js";

const config = (extra: Record<string, unknown> = {}) => ({
  schemaVersion: 1,
  compatibility: {
    testedCliVersions: ["1.0.88", "1.0.89"],
    untestedVersionPolicy: "block-custom-routing",
  },
  providers: {
    p: {
      protocol: "openai-chat-completions",
      baseUrl: "http://127.0.0.1:1/v1",
      credentialRef: "env:SECRET",
      timeoutsMs: { connect: 100, firstByte: 100, streamIdle: 100, total: 1000 },
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
  recording: {
    defaultMode: "off",
    retentionDays: 7,
    maxTotalMiB: 1,
    maxBodyMiBPerRequest: 1,
    onWriteFailure: "continue",
  },
  ...extra,
});

describe("config and routes", () => {
  it("validates duplicates, refs and SEC-15", () => {
    const bad = config({
      models: [...(config() as any).models, ...(config() as any).models],
      routing: {
        ...(config() as any).routing,
        githubLeg: { mode: "forward" },
      },
    });
    const e = validateConfig(bad);
    expect(e.some(x => x.code === "duplicate")).toBe(true);
    expect(e.some(x => x.code === "SEC-15")).toBe(true);
  });

  it("freezes revisions and redacts credentials", () => {
    const r = loadConfig(config()).revision!;
    expect(Object.isFrozen(r)).toBe(true);
    expect(redactEffective(r).providers.p.credentialRef).toBe("redacted");
  });

  it("resolves registered and unknown aliases", () => {
    const r = loadConfig(config()).revision!;
    expect(resolveRoute("custom/a", r)).toMatchObject({
      kind: "custom",
      upstreamModel: "real-a",
    });
    expect(resolveRoute("custom/nope", r).kind).toBe("error");
  });
});

describe("classifier", () => {
  it("labels the captured nano classifier as auxiliary without requiring registration", () => {
    const x = classifyRequest({
      model: "gpt-5.4-nano",
      messages: [{
        role: "system",
        content: "Detect whether the CURRENT MESSAGE expresses frustration",
      }],
    }, "custom/a");
    expect(x.kind).toBe("auxiliary");
    expect(resolveRoute(
      "gpt-5.4-nano",
      loadConfig(config()).revision!,
      {
        model: "gpt-5.4-nano",
        messages: [{ role: "system", content: "classify frustration" }],
      },
      "custom/a",
    )).toMatchObject({ kind: "auxiliary", policy: "block" });
  });
});

describe("SSE", () => {
  it("handles split UTF-8, multiple frames and DONE", () => {
    const p = new SSEParser();
    const text = "data: {\"x\":\"€\"}\n\ndata: [DONE]\n\n";
    const b = new TextEncoder().encode(text);
    const a = p.feed(b.slice(0, 15));
    const c = p.feed(b.slice(15), true);
    expect([...a, ...c].map(x => x.data)).toEqual(['{"x":"€"}', "[DONE]"]);
  });

  it("rejects malformed terminal data", () => {
    expect(() => new SSEParser().feed(new TextEncoder().encode("bad"), true)).toThrow();
  });
});

describe("server integration", () => {
  let upstream: http.Server;
  let chr: http.Server;
  let upstreamHits = 0;
  let chrPort = 0;
  let receivedAuthorization: string | undefined;

  beforeAll(async () => {
    upstream = http.createServer((req, res) => {
      upstreamHits++;
      receivedAuthorization = req.headers.authorization;
      res.writeHead(200, { "content-type": "text/event-stream" });
      res.flushHeaders();
      res.write(
        'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"run","arguments":"{\\"a\\":"}}]}}]}\n\n',
      );
      setTimeout(() => {
        res.write(
          'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}]}\n\n',
        );
        res.end("data: [DONE]\n\n");
      }, 10);
    });
    await new Promise<void>(r => upstream.listen(0, "127.0.0.1", () => r()));
    const port = (upstream.address() as { port: number }).port;
    const r = loadConfig(config({
      providers: {
        p: {
          ...(config() as any).providers.p,
          baseUrl: `http://127.0.0.1:${port}/v1`,
        },
      },
    })).revision!;
    chr = createServer(r);
    chrPort = await listen(chr);
  });

  afterAll(() => {
    upstream.close();
    chr.close();
  });

  it("streams fragments and preserves the upstream request boundary", async () => {
    const response = await fetch(`http://127.0.0.1:${chrPort}/v1/chat/completions`, {
      method: "POST",
      body: JSON.stringify({ model: "custom/a", messages: [], stream: true }),
      headers: { authorization: "******", "content-type": "application/json" },
    });
    const reader = response.body!.getReader();
    const chunks: Uint8Array[] = [];
    while (true) {
      const x = await reader.read();
      if (x.done) break;
      chunks.push(x.value);
    }
    expect(upstreamHits).toBe(1);
    expect(receivedAuthorization).toBeUndefined();
    expect(chunks.length).toBeGreaterThan(1);
    const text = new TextDecoder().decode(Buffer.concat(chunks));
    expect(text.indexOf('\\"a\\":') < text.indexOf('1}')).toBe(true);
  });

  it("rejects an unknown alias without contacting upstream", async () => {
    upstreamHits = 0;
    const response = await fetch(`http://127.0.0.1:${chrPort}/v1/chat/completions`, {
      method: "POST",
      body: JSON.stringify({ model: "custom/nope", messages: [], stream: true }),
    });
    expect(response.status).toBe(400);
    expect(upstreamHits).toBe(0);
  });
});
