import fs from "node:fs";
import path from "node:path";
import http from "node:http";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { loadConfig } from "../src/config/config.js";
import { createServer, listen } from "../src/server/server.js";

type Envelope = {
  path: string;
  body: Record<string, unknown>;
};

const fixture = (directory: string, name: string): Envelope => {
  const file = path.resolve(process.cwd(), "..", directory, "fixtures", name);
  return JSON.parse(fs.readFileSync(file, "utf8")) as Envelope;
};

describe("captured CLI fixtures", () => {
  let upstream: http.Server;
  let chr: http.Server;
  let port: number;
  const received: Array<Record<string, unknown>> = [];

  beforeAll(async () => {
    upstream = http.createServer((req, res) => {
      const chunks: Buffer[] = [];
      req.on("data", (chunk) => chunks.push(Buffer.from(chunk)));
      req.on("end", () => {
        received.push(JSON.parse(Buffer.concat(chunks).toString("utf8")));
        res.writeHead(200, { "content-type": "text/event-stream" });
        res.end("data: [DONE]\n\n");
      });
    });
    await new Promise<void>((resolve) => upstream.listen(0, "127.0.0.1", resolve));
    const upstreamPort = (upstream.address() as { port: number }).port;
    const config = {
      schemaVersion: 1,
      compatibility: {
        testedCliVersions: ["1.0.88", "1.0.89"],
        untestedVersionPolicy: "block-custom-routing",
      },
      providers: {
        chat: {
          protocol: "openai-chat-completions",
          baseUrl: `http://127.0.0.1:${upstreamPort}/v1`,
          credentialRef: "env:KEY",
          timeoutsMs: { connect: 500, firstByte: 500, streamIdle: 500, total: 2000 },
        },
        responses: {
          protocol: "openai-responses",
          baseUrl: `http://127.0.0.1:${upstreamPort}/v1`,
          credentialRef: "env:KEY",
          timeoutsMs: { connect: 500, firstByte: 500, streamIdle: 500, total: 2000 },
        },
      },
      models: [
        {
          alias: "custom/chat",
          displayName: "Chat",
          provider: "chat",
          upstreamModel: "chat-upstream",
          capabilities: { streaming: true, tools: true, vision: false, contextWindowTokens: null },
        },
        {
          alias: "custom/responses",
          displayName: "Responses",
          provider: "responses",
          upstreamModel: "responses-upstream",
          capabilities: { streaming: true, tools: true, vision: false, contextWindowTokens: null },
        },
      ],
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
    };
    chr = createServer(loadConfig(config).revision!);
    port = await listen(chr);
  });

  afterAll(() => {
    upstream.close();
    chr.close();
  });

  it("serves the deterministic registered model catalog", async () => {
    const response = await fetch(`http://127.0.0.1:${port}/v1/models`);
    const body = await response.json() as { data: Array<{ id: string }> };

    expect(response.status).toBe(200);
    expect(body.data.map((model) => model.id)).toEqual(["custom/chat", "custom/responses"]);
  });

  it.each(["spike", "spike3"])("accepts %s captured bodies", async (directory) => {
    for (const [name, alias] of [
      ["completions-request.json", "custom/chat"],
      ["responses-request.json", "custom/responses"],
    ] as const) {
    const envelope = fixture(directory, name);
    const body = { ...envelope.body, model: alias };
    const response = await fetch(`http://127.0.0.1:${port}${envelope.path}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });

    expect(response.status).toBe(200);
    await response.text();
    expect(received.at(-1)?.model).toBe(
      alias === "custom/chat" ? "chat-upstream" : "responses-upstream",
    );
    }
  });
});
