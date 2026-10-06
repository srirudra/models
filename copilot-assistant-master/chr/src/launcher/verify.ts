import { spawn } from "node:child_process";
import http from "node:http";
import path from "node:path";
import { runDoctor } from "./doctor.js";
import { starterTestedCliVersions } from "./config-file.js";
import type { ConfigRevision } from "../config/types.js";

const probeModel = "chr-verify-probe";
const replyText = "CHR verify reply";

type CapturedRequest = {
  method: string;
  path: string;
  body: Record<string, unknown>;
};

export type VerifyOptions = {
  revision?: ConfigRevision;
  copilot?: string;
  timeoutMs?: number;
  env?: NodeJS.ProcessEnv;
  doctor?: typeof runDoctor;
};

export type VerifyResult = {
  version?: string;
  configStatus: "loaded" | "missing";
  tested: boolean;
  checks: Array<{ wireApi: string; passed: boolean; details: string[] }>;
  output: string;
  exitCode: number;
};

function readRequest(req: http.IncomingMessage): Promise<Record<string, unknown>> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    req.on("data", (chunk) => chunks.push(Buffer.from(chunk)));
    req.on("end", () => {
      try {
        const parsed = JSON.parse(Buffer.concat(chunks).toString("utf8")) as Record<string, unknown>;
        const body = parsed.body;
        resolve(body && typeof body === "object" && !Array.isArray(body)
          ? body as Record<string, unknown>
          : parsed);
      } catch (error) {
        reject(error);
      }
    });
    req.on("error", reject);
  });
}

function sendSse(res: http.ServerResponse, wireApi: string): void {
  res.writeHead(200, {
    "content-type": "text/event-stream",
    "cache-control": "no-cache",
    connection: "keep-alive",
  });
  if (wireApi === "completions") {
    res.write(`data: ${JSON.stringify({
      id: "chr-verify",
      object: "chat.completion.chunk",
      choices: [{ index: 0, delta: { role: "assistant", content: replyText }, finish_reason: null }],
    })}\n\n`);
    res.write(`data: ${JSON.stringify({
      id: "chr-verify",
      object: "chat.completion.chunk",
      choices: [{ index: 0, delta: {}, finish_reason: "stop" }],
    })}\n\n`);
  } else {
    res.write(`data: ${JSON.stringify({
      type: "response.created",
      response: { id: "chr-verify", object: "response", status: "in_progress" },
    })}\n\n`);
    res.write(`data: ${JSON.stringify({
      type: "response.output_text.delta",
      item_id: "msg-verify",
      output_index: 0,
      content_index: 0,
      delta: replyText,
    })}\n\n`);
    res.write(`data: ${JSON.stringify({
      type: "response.completed",
      response: {
        id: "chr-verify",
        object: "response",
        status: "completed",
        output: [{
          type: "message",
          role: "assistant",
          content: [{ type: "output_text", text: replyText }],
        }],
      },
    })}\n\n`);
  }
  res.end("data: [DONE]\n\n");
}

function close(server: http.Server): Promise<void> {
  return new Promise((resolve) => server.close(() => resolve()));
}

function childResult(
  executable: string,
  args: string[],
  env: NodeJS.ProcessEnv,
  timeoutMs: number,
): Promise<{ code: number; output: string; timedOut: boolean }> {
  return new Promise((resolve, reject) => {
    const command = [".js", ".mjs", ".cjs"].includes(path.extname(executable).toLowerCase())
      ? process.execPath
      : executable;
    const child = spawn(command, command === executable ? args : [executable, ...args], { env, windowsHide: true });
    let output = "";
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill();
    }, timeoutMs);
    child.stdout?.on("data", (chunk) => { output += chunk.toString(); });
    child.stderr?.on("data", (chunk) => { output += chunk.toString(); });
    child.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
    child.once("close", (code) => {
      clearTimeout(timer);
      resolve({ code: typeof code === "number" ? code : 1, output, timedOut });
    });
  });
}

function validTools(value: unknown, wireApi: "completions" | "responses"): boolean {
  if (value === undefined) return true;
  return Array.isArray(value) && value.every((tool) => {
    if (!tool || typeof tool !== "object") return false;
    const item = tool as Record<string, unknown>;
    if (wireApi === "completions") {
      const fn = item.function;
      return item.type === "function" && !!fn && typeof fn === "object" &&
        typeof (fn as Record<string, unknown>).name === "string";
    }
    return item.type === "function" && typeof item.name === "string";
  });
}

async function probe(
  executable: string,
  wireApi: "completions" | "responses",
  env: NodeJS.ProcessEnv,
  timeoutMs: number,
): Promise<{ passed: boolean; details: string[] }> {
  const requests: CapturedRequest[] = [];
  const server = http.createServer(async (req, res) => {
    const path = new URL(req.url ?? "/", "http://127.0.0.1").pathname;
    if (req.method === "GET" && path === "/v1/models") {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ object: "list", data: [{ id: probeModel }] }));
      return;
    }
    if (req.method === "POST" && (path === "/v1/chat/completions" || path === "/v1/responses")) {
      try {
        requests.push({ method: req.method, path, body: await readRequest(req) });
        sendSse(res, wireApi);
      } catch {
        res.writeHead(400);
        res.end();
      }
      return;
    }
    res.writeHead(404);
    res.end();
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const port = (server.address() as { port: number }).port;
  const childEnv: NodeJS.ProcessEnv = { ...env };
  for (const key of Object.keys(childEnv)) {
    if (key.startsWith("COPILOT_PROVIDER_")) delete childEnv[key];
  }
  Object.assign(childEnv, {
    COPILOT_PROVIDER_BASE_URL: `http://127.0.0.1:${port}/v1`,
    COPILOT_PROVIDER_TYPE: "openai",
    COPILOT_PROVIDER_WIRE_API: wireApi,
    COPILOT_MODEL: probeModel,
  });
  let result: { code: number; output: string; timedOut: boolean };
  try {
    result = await childResult(executable, ["-p", "Reply with the verification text.", "--allow-all-tools"], childEnv, timeoutMs);
  } finally {
    await close(server);
  }

  const request = requests.find((item) => item.path === `/v1/${wireApi === "completions" ? "chat/completions" : "responses"}`);
  const body = request?.body;
  const details: string[] = [];
  if (!request) details.push("expected endpoint was not called");
  if (body?.model !== probeModel) details.push("body.model is not the probe model");
  if (body?.stream !== true) details.push("body.stream is not true");
  if (wireApi === "completions" && !Array.isArray(body?.messages)) details.push("messages is missing");
  if (wireApi === "responses" && !Array.isArray(body?.input)) details.push("input is missing");
  if (!validTools(body?.tools, wireApi)) details.push("tools has an invalid shape");
  if (result.code !== 0) details.push(`Copilot exited with code ${result.code}`);
  if (result.timedOut) details.push(`Copilot exceeded ${timeoutMs}ms timeout`);
  if (!result.output.includes(replyText)) details.push("Copilot output did not contain the mock reply");
  return { passed: details.length === 0, details };
}

export async function runVerify(options: VerifyOptions = {}): Promise<VerifyResult> {
  const doctor = await (options.doctor ?? runDoctor)({
    revision: options.revision,
    env: options.env,
    executable: options.copilot,
  });
  const checks: VerifyResult["checks"] = [];
  if (!doctor.executable || !doctor.version) {
    return {
      version: doctor.version,
      configStatus: options.revision ? "loaded" : "missing",
      tested: false,
      checks,
      output: `Unable to verify Copilot: ${doctor.blocking.join(" ") || "executable not found."}`,
      exitCode: 1,
    };
  }
  for (const wireApi of ["completions", "responses"] as const) {
    const check = await probe(doctor.executable, wireApi, options.env ?? process.env, options.timeoutMs ?? 30_000);
    checks.push({ wireApi, ...check, details: check.passed ? ["all invariants passed"] : check.details });
  }
  const tested = Boolean(options.revision?.compatibility.testedCliVersions.includes(doctor.version));
  const configStatus = options.revision ? "loaded" : "missing";
  const inStarterTemplate = starterTestedCliVersions().includes(doctor.version);
  const passed = checks.every((check) => check.passed);
  const versionStatus = configStatus === "loaded"
    ? (tested ? "tested" : "untested")
    : inStarterTemplate
      ? "no CHR config; starter template lists it as tested"
      : "no CHR config; not in starter template";
  const lines = [
    `Copilot: ${doctor.executable}`,
    `Version: ${doctor.version} (${versionStatus})`,
    ...checks.map((check) => `${check.wireApi}: ${check.passed ? "PASS" : "FAIL"}${check.details.length ? ` — ${check.details.join("; ")}` : ""}`),
    configStatus === "missing" ? 'Run "chr config init" to create a config.' : "",
    configStatus === "missing" && !inStarterTemplate
      ? `Add ${doctor.version} to testedCliVersions before enabling custom routing.`
      : "",
    passed && configStatus === "loaded" && !tested
      ? `All probes passed. Run "chr verify --accept" to add ${doctor.version} to testedCliVersions before enabling custom routing.`
      : "",
  ].filter(Boolean);
  return { version: doctor.version, configStatus, tested, checks, output: lines.join("\n"), exitCode: passed ? 0 : 1 };
}
