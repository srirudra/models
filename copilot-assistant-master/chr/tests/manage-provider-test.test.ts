import http from "node:http";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { removeAll, run, seedConfig, seedConfigFile, tempDirectory, withEnv } from "./manage-support.js";

type Handler = (req: http.IncomingMessage, res: http.ServerResponse) => void;

const servers: http.Server[] = [];
const directories: string[] = [];
const restore: Array<() => void> = [];
let directory = "";

async function listen(handler: Handler): Promise<string> {
  const server = http.createServer(handler);
  servers.push(server);
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  return `http://127.0.0.1:${(server.address() as { port: number }).port}/v1`;
}

async function configureProvider(baseUrl: string, protocol = "openai-chat-completions"): Promise<void> {
  const filePath = await seedConfigFile(directory, {
    providers: {
      local: {
        protocol,
        baseUrl,
        credentialRef: "env:CHR_MANAGE_PROBE_KEY",
        timeoutsMs: seedConfig.providers.local.timeoutsMs,
      },
    },
  });
  restore.push(withEnv("CHR_CONFIG_PATH", filePath));
}

beforeEach(async () => {
  directory = await tempDirectory("chr-probe-");
  directories.push(directory);
  restore.push(withEnv("CHR_MANAGE_PROBE_KEY", "sk-probe-credential-value"));
});

afterEach(async () => {
  while (restore.length > 0) restore.pop()!();
  for (const server of servers.splice(0)) await new Promise<void>((resolve) => server.close(() => resolve()));
  await removeAll(directories);
});

describe("chr provider test", () => {
  it("passes when /models and the completion probe both return 2xx", async () => {
    const seen: string[] = [];
    let posted: Record<string, unknown> = {};
    const baseUrl = await listen((req, res) => {
      seen.push(`${req.method} ${req.url}`);
      if (req.url === "/v1/models") {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({ object: "list", data: [{ id: "gpt-test" }, { id: "other" }] }));
        return;
      }
      const chunks: Buffer[] = [];
      req.on("data", (chunk) => chunks.push(Buffer.from(chunk)));
      req.on("end", () => {
        posted = JSON.parse(Buffer.concat(chunks).toString("utf8")) as Record<string, unknown>;
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({
          choices: [{ message: { role: "assistant", content: "OK" }, finish_reason: "stop" }],
        }));
      });
    });
    await configureProvider(baseUrl);

    const result = await run(["provider", "test", "local"]);
    expect(result.code).toBe(0);
    expect(seen).toEqual(["GET /v1/models", "POST /v1/chat/completions"]);
    expect(posted.max_tokens).toBe(64);
    expect(result.out).toContain("gpt-test");
    expect(result.out).toContain('reply: "OK" (finish_reason: stop)');
    expect(result.out).not.toContain('"object"');
    expect(result.out).toContain("Provider local: OK");
    expect(result.out).not.toContain("sk-probe-credential-value");
  });

  it("reports a reasoning-only reply and a token-limited empty reply without failing", async () => {
    let payload = "";
    const baseUrl = await listen((req, res) => {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(req.url === "/v1/models" ? JSON.stringify({ data: [{ id: "gpt-test" }] }) : payload);
    });
    await configureProvider(baseUrl);

    payload = JSON.stringify({
      id: "chatcmpl-1",
      object: "chat.completion",
      choices: [{ message: { content: "", reasoning_content: "thinking about OK" }, finish_reason: "length" }],
    });
    const reasoning = await run(["provider", "test", "local"]);
    expect(reasoning.code).toBe(0);
    expect(reasoning.out).toContain('reply (reasoning): "thinking about OK" (finish_reason: length)');
    expect(reasoning.out).not.toContain("chatcmpl-1");

    payload = JSON.stringify({
      id: "chatcmpl-2",
      object: "chat.completion",
      choices: [{ message: { content: "" }, finish_reason: "length" }],
    });
    const truncated = await run(["provider", "test", "local"]);
    expect(truncated.code).toBe(0);
    expect(truncated.out).toContain('reply: "" (finish_reason: length)');
    expect(truncated.out).toContain("reply truncated by token limit (reasoning model?) — connectivity OK");
    expect(truncated.out).toContain("Provider local: OK");
  });

  it("warns when the configured upstream model is missing from /models", async () => {
    const baseUrl = await listen((req, res) => {
      if (req.url === "/v1/models") {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({ data: [{ id: "someone-else" }] }));
        return;
      }
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ choices: [{ message: { content: "OK" } }] }));
    });
    await configureProvider(baseUrl);

    const result = await run(["provider", "test", "local"]);
    expect(result.code).toBe(0);
    expect(result.out).toContain("Warning: model alpha uses upstreamModel gpt-test");
  });

  it("uses the responses endpoint for openai-responses providers", async () => {
    const seen: string[] = [];
    let posted: Record<string, unknown> = {};
    const baseUrl = await listen((req, res) => {
      seen.push(`${req.method} ${req.url}`);
      if (req.url === "/v1/models") {
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({ data: [{ id: "gpt-test" }] }));
        return;
      }
      const chunks: Buffer[] = [];
      req.on("data", (chunk) => chunks.push(Buffer.from(chunk)));
      req.on("end", () => {
        posted = JSON.parse(Buffer.concat(chunks).toString("utf8")) as Record<string, unknown>;
        res.writeHead(200, { "content-type": "application/json" });
        res.end(JSON.stringify({
          status: "completed",
          output: [{ content: [{ type: "output_text", text: "O" }, { type: "output_text", text: "K" }] }],
        }));
      });
    });
    await configureProvider(baseUrl, "openai-responses");

    const result = await run(["provider", "test", "local", "--model", "gpt-test"]);
    expect(result.code).toBe(0);
    expect(seen).toEqual(["GET /v1/models", "POST /v1/responses"]);
    expect(posted.max_output_tokens).toBe(64);
    expect(result.out).toContain('reply: "OK" (finish_reason: completed)');
  });

  it("maps an incomplete responses payload to a token-limit note", async () => {
    const baseUrl = await listen((req, res) => {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(req.url === "/v1/models"
        ? JSON.stringify({ data: [{ id: "gpt-test" }] })
        : JSON.stringify({
          status: "incomplete",
          incomplete_details: { reason: "max_output_tokens" },
          output: [{ content: [] }],
        }));
    });
    await configureProvider(baseUrl, "openai-responses");

    const result = await run(["provider", "test", "local", "--model", "gpt-test"]);
    expect(result.code).toBe(0);
    expect(result.out).toContain('reply: "" (finish_reason: length)');
    expect(result.out).toContain("reply truncated by token limit (reasoning model?) — connectivity OK");
  });

  it("fails on 401 without echoing the credential", async () => {
    const baseUrl = await listen((_req, res) => {
      res.writeHead(401, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: { message: "invalid key sk-probe-credential-value" } }));
    });
    await configureProvider(baseUrl);

    const result = await run(["provider", "test", "local"]);
    expect(result.code).toBe(1);
    expect(result.out).toContain("401");
    expect(result.out).toContain("REDACTED");
    expect(result.out).not.toContain("sk-probe-credential-value");
    expect(result.out).toContain("Provider local: FAILED");
  });

  it("treats a 3xx as failure and never contacts the redirect target", async () => {
    let contacted = 0;
    const target = await listen((_req, res) => {
      contacted += 1;
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ data: [] }));
    });
    const baseUrl = await listen((_req, res) => {
      res.writeHead(302, { location: `${target}/models` });
      res.end();
    });
    await configureProvider(baseUrl);

    const result = await run(["provider", "test", "local"]);
    expect(result.code).toBe(1);
    expect(contacted).toBe(0);
    expect(result.out).toContain("302");
    expect(result.out).toContain("redirect not followed");
  });

  it("reports an unresolvable credential", async () => {
    const baseUrl = await listen((_req, res) => { res.writeHead(200); res.end("{}"); });
    await configureProvider(baseUrl);
    restore.push(withEnv("CHR_MANAGE_PROBE_KEY", undefined));

    const result = await run(["provider", "test", "local"]);
    expect(result.code).toBe(1);
    expect(result.out).toContain("could not be resolved");
  });

  it("rejects an unknown provider name", async () => {
    await configureProvider("https://example.test/v1");
    const result = await run(["provider", "test", "nope"]);
    expect(result.code).toBe(1);
    expect(result.out).toContain("Unknown provider nope");
  });
});
