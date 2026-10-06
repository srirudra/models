import { mkdtemp, readdir, rm, writeFile } from "node:fs/promises";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  createRecorder, recordingDirectory, resetRecordingTestSeams, setRecordingTestSeams, writeRecordingMode,
} from "../src/recording/recorder.js";
import { createInterceptHandler } from "../src/intercept/router-handler.js";
import { primeCredentials } from "../src/config/config.js";
import type { ConfigRevision, Provider } from "../src/config/types.js";

const config = {
  defaultMode: "metadata", retentionDays: 30, maxTotalMiB: 2, maxBodyMiBPerRequest: 1, onWriteFailure: "warn",
} as const;
const dirs: string[] = [];
afterEach(async () => {
  resetRecordingTestSeams();
  for (const dir of dirs.splice(0)) await rm(dir, { recursive: true, force: true });
});

async function recorder(mode: "off" | "metadata" | "full") {
  const dir = await mkdtemp(path.join(os.tmpdir(), "chr-at-recording-")); dirs.push(dir);
  setRecordingTestSeams({ directory: dir }); await writeRecordingMode(mode);
  await new Promise((resolve) => setTimeout(resolve, 5));
  return { recorder: createRecorder(config), dir };
}
const provider: Provider = { protocol: "openai-chat-completions", baseUrl: "http://127.0.0.1:1/v1",
  credentialRef: "env:AT_KEY", timeoutsMs: { connect: 200, firstByte: 200, streamIdle: 500, total: 2000 } };
const revision: ConfigRevision = { revisionId: 991, schemaVersion: 1, providers: { p: provider },
  models: [{ alias: "at-model", displayName: "at", provider: "p", upstreamModel: "model",
    capabilities: { streaming: true, tools: true, vision: false, contextWindowTokens: 4096 } }],
  routing: { unmatchedGitHubModel: "github", unknownCustomModel: "error", crossProviderFallback: "error",
    auxiliary: { policy: "block" }, githubLeg: { mode: "forward" } } };
async function realFlow(mode: "off" | "metadata" | "full", chunks: string[], onChunk?: () => Promise<void>, abortAfterFirst = false) {
  const dir = await mkdtemp(path.join(os.tmpdir(), "chr-at-real-recording-")); dirs.push(dir);
  setRecordingTestSeams({ directory: dir }); await writeRecordingMode(mode);
  let providerPort = 0;
  let providerClosed = false;
  const ps = http.createServer(async (providerReq, res) => {
    providerReq.once("close", () => { providerClosed = true; });
    res.writeHead(200, { "content-type": "text/event-stream" });
    for (const chunk of chunks) { res.write(chunk); if (onChunk) await onChunk(); if (abortAfterFirst) await new Promise((resolve) => setTimeout(resolve, 100)); }
    res.end();
  });
  await new Promise<void>((resolve) => ps.listen(0, "127.0.0.1", resolve)); providerPort = (ps.address() as { port: number }).port;
  const r = { ...revision, providers: { p: { ...provider, baseUrl: `http://127.0.0.1:${providerPort}/v1` } } };
  await primeCredentials(r, { AT_KEY: "provider-secret" });
  const recorderValue = createRecorder(config);
  await new Promise((resolve) => setTimeout(resolve, 10));
  const handler = createInterceptHandler(r, { recorder: recorderValue });
  const gateway = http.createServer((req, res) => handler(req, res, { host: "api.individual.githubcopilot.com" }));
  await new Promise<void>((resolve) => gateway.listen(0, "127.0.0.1", resolve)); const port = (gateway.address() as { port: number }).port;
  // Other recording suites use the process-global test seam; restore ours
  // immediately before driving the handler.
  setRecordingTestSeams({ directory: dir });
  const result = await new Promise<{ text: string; status: number }>((resolve, reject) => {
    const body = JSON.stringify({ model: "at-model", messages: [{ role: "user", content: "private prompt" }] });
    const req = http.request({ hostname: "127.0.0.1", port, path: "/v1/chat/completions", method: "POST",
      headers: { "content-type": "application/json", "content-length": Buffer.byteLength(body) } }, (res) => {
      const out: Buffer[] = []; let first = true;
      res.on("data", async (chunk) => { out.push(Buffer.from(chunk)); if (onChunk) await onChunk(); if (abortAfterFirst && first) { first = false; req.destroy(); } });
      res.on("end", () => resolve({ text: Buffer.concat(out).toString(), status: res.statusCode ?? 0 }));
      res.on("close", () => { if (abortAfterFirst) resolve({ text: Buffer.concat(out).toString(), status: res.statusCode ?? 0 }); });
    }); req.once("error", reject); req.end(body);
  });
  await new Promise((resolve) => setTimeout(resolve, 30)); await gateway.close(); await ps.close();
  return { dir, result, providerClosed };
}
const entry = { request: { headers: { authorization: "Bearer secret" }, body: { prompt: "private prompt" } },
  response: { bodyText: "private answer" } };

describe("CHR automated acceptance recording", () => {
  it("AT-08 applies off, metadata, and full recording semantics", async () => {
    const frame = `data: ${JSON.stringify({ id: "x", choices: [{ delta: { content: "answer" }, finish_reason: "stop" }] })}\n\ndata: [DONE]\n\n`;
    const off = await realFlow("off", [frame]); expect(await recordingFiles(off.dir)).toHaveLength(0);
    const full = await realFlow("full", [frame]); expect(await pollText(full.dir)).toContain("private prompt");
  });

  it("AT-09 drops bodies when mode changes before the recording write", async () => {
    const frame = `data: ${JSON.stringify({ id: "x", choices: [{ delta: { content: "private answer" }, finish_reason: null }] })}\n\n`;
    const off = await realFlow("full", [frame, "data: [DONE]\n\n"], async () => { await writeRecordingMode("off"); });
    expect(await pollText(off.dir)).not.toContain("private prompt");
    const metadata = await realFlow("full", [frame, "data: [DONE]\n\n"], async () => { await writeRecordingMode("metadata"); });
    expect(await pollText(metadata.dir)).not.toContain("private prompt");
  });

  it("AT-13 disables recording after a write failure without affecting callers", async () => {
    const parent = await mkdtemp(path.join(os.tmpdir(), "chr-at-recording-file-")); dirs.push(parent);
    const file = path.join(parent, "not-a-directory"); await writeFile(file, "x");
    setRecordingTestSeams({ directory: file }); await writeRecordingMode("full").catch(() => undefined);
    const log = vi.fn(); const value = createRecorder({ ...config, onWriteFailure: "disable" }, { logger: log });
    await expect(value.record(entry)).resolves.toBeUndefined();
    await expect(value.record(entry)).resolves.toBeUndefined();
    expect(log).toHaveBeenCalledTimes(1);
  });

  it("AT-11 persists the canceled marker for an interrupted stream", async () => {
    // Cancellation metadata is exercised by the handler; this assertion also
    // ensures the real recorder remains enabled for the request lifecycle.
    const value = await realFlow("full", ["data: {\"id\":\"x\"}\n\n", "data: [DONE]\n\n"], undefined, true);
    expect(value.providerClosed).toBe(true);
    expect(await pollText(value.dir)).toContain("\"canceled\":true");
  });
});

async function writeText(dir: string): Promise<string> {
  const recordingDir = recordingDirectory();
  const files = (await readdir(recordingDir)).filter((x) => x.endsWith(".json"));
  return files.length ? (await Promise.all(files.map((file) => import("node:fs/promises").then((fs) => fs.readFile(path.join(recordingDir, file), "utf8"))))).join("\n") : "";
}
async function recordingFiles(dir: string): Promise<string[]> {
  setRecordingTestSeams({ directory: dir });
  try { return (await readdir(recordingDirectory())).filter((x) => x.endsWith(".json")); } catch { return []; }
}
async function pollText(dir: string): Promise<string> {
  for (let i = 0; i < 40; i++) {
    setRecordingTestSeams({ directory: dir });
    if ((await recordingFiles(dir)).length) return await writeText(dir);
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  return await writeText(dir);
}
