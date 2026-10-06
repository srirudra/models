import { readFile, writeFile } from "node:fs/promises";
import { loadConfig, resolveCredential } from "../config/config.js";
import type { ConfigRevision, Protocol } from "../config/types.js";
import { defaultConfigPath, readConfigFile, writeConfigFileAtomic } from "../launcher/config-file.js";
import { getSecret, listSecretNames } from "../secrets/dpapi.js";
import {
  deleteRecordingFile, isRecordingId, listRecordingFiles, readRecordingFile,
  readRecordingMode, recordingDirectory,
} from "../recording/recorder.js";

export const chrVersion = "0.1.0";

/** Aliases become catalog model ids, so keep them to URL/JSON-safe characters. */
const aliasPattern = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,63}$/;

const defaultTimeoutsMs = { connect: 10000, firstByte: 120000, streamIdle: 60000, total: 900000 };
const defaultRecordingConfig = {
  defaultMode: "off", retentionDays: 0, maxTotalMiB: 0, maxBodyMiBPerRequest: 0, onWriteFailure: "ignore",
};

export type ManageIo = { print: (value: string) => void; usage: () => void };

type RawConfig = Record<string, unknown>;
type RawRecord = Record<string, unknown>;

/** A user-facing failure that maps to exit code 1 with a plain message. */
class ManageError extends Error {}

function configPath(): string {
  return process.env.CHR_CONFIG_PATH ?? defaultConfigPath();
}

// ---------------------------------------------------------------- argv parsing

type Parsed = { positional: string[]; values: Record<string, string>; flags: Set<string> };

function parseArgs(
  args: string[],
  spec: { values?: string[]; flags?: string[] },
): Parsed | undefined {
  const values: Record<string, string> = {};
  const flags = new Set<string>();
  const positional: string[] = [];
  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index];
    if (!arg.startsWith("--")) {
      positional.push(arg);
      continue;
    }
    if (spec.values?.includes(arg)) {
      const value = args[index + 1];
      if (value === undefined || value.startsWith("--")) return undefined;
      values[arg] = value;
      index += 1;
    } else if (spec.flags?.includes(arg)) {
      flags.add(arg);
    } else {
      return undefined;
    }
  }
  return { positional, values, flags };
}

function positiveInteger(value: string, label: string): number {
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed <= 0) throw new ManageError(`${label} must be a positive integer.`);
  return parsed;
}

// ------------------------------------------------------------------ formatting

function table(headers: string[], rows: string[][]): string {
  const widths = headers.map((header, column) =>
    Math.max(header.length, ...rows.map((row) => (row[column] ?? "").length)));
  const line = (cells: string[]): string =>
    cells.map((cell, column) => (cell ?? "").padEnd(widths[column])).join("  ").trimEnd();
  return [line(headers), ...rows.map(line)].join("\n");
}

function formatErrors(errors: Array<{ path: string; code: string; message: string }>): string {
  return errors.map((error) => `  ${error.path || "(root)"} [${error.code}]: ${error.message}`).join("\n");
}

// ------------------------------------------------------------------ redaction

const secretPatterns: RegExp[] = [
  /Bearer\s+[A-Za-z0-9._~+/=-]+/g,
  /\bsk-[A-Za-z0-9_-]{8,}\b/g,
  /\bgh[opsu]_[A-Za-z0-9]{20,}\b/g,
  /\bgithub_pat_[A-Za-z0-9_]{20,}\b/g,
];

export function scrubSecrets(text: string): string {
  let result = text;
  for (const pattern of secretPatterns) result = result.replace(pattern, "REDACTED");
  return result;
}

function isSensitiveHeader(name: string): boolean {
  const lower = name.toLowerCase();
  return lower === "authorization" || lower === "proxy-authorization" || lower === "cookie" ||
    lower === "set-cookie" || lower === "x-api-key" || /token|key|secret/.test(lower);
}

/** Second sanitisation pass applied when recordings leave the machine. */
export function sanitizeForExport(value: unknown, inHeaders = false): unknown {
  if (typeof value === "string") return scrubSecrets(value);
  if (Array.isArray(value)) return value.map((item) => sanitizeForExport(item, inHeaders));
  if (value && typeof value === "object") {
    const result: Record<string, unknown> = {};
    for (const [key, item] of Object.entries(value as Record<string, unknown>)) {
      if (inHeaders && isSensitiveHeader(key)) result[key] = "REDACTED";
      else result[key] = sanitizeForExport(item, inHeaders || key.toLowerCase() === "headers");
    }
    return result;
  }
  return value;
}

// ------------------------------------------------------------- config plumbing

async function loadValidConfig(io: ManageIo): Promise<ConfigRevision | undefined> {
  const config = await readConfigFile();
  if (config.revision) return config.revision;
  io.print(config.missing
    ? `Configuration is missing at ${config.path}. Run "chr config init".`
    : `Configuration is invalid at ${config.path}:\n${formatErrors(config.errors)}`);
  return undefined;
}

/**
 * Reads the raw config (preserving unknown keys and key order), applies
 * `mutate`, validates the result, and only then rewrites the file atomically.
 */
async function updateConfig(io: ManageIo, mutate: (raw: RawConfig) => string): Promise<number> {
  const filePath = configPath();
  let text: string;
  try {
    text = await readFile(filePath, "utf8");
  } catch (error) {
    io.print((error as NodeJS.ErrnoException).code === "ENOENT"
      ? `Configuration is missing at ${filePath}. Run "chr config init".`
      : `Configuration could not be read at ${filePath}.`);
    return 1;
  }
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    io.print(`Configuration contains invalid JSON at ${filePath}.`);
    return 1;
  }
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
    io.print(`Configuration at ${filePath} is not a JSON object.`);
    return 1;
  }
  let message: string;
  try {
    message = mutate(raw as RawConfig);
  } catch (error) {
    if (error instanceof ManageError) {
      io.print(error.message);
      return 1;
    }
    throw error;
  }
  const { errors } = loadConfig(raw);
  if (errors.length > 0) {
    io.print(`Refusing to write an invalid configuration (${filePath} is unchanged):\n${formatErrors(errors)}`);
    return 1;
  }
  await writeConfigFileAtomic(filePath, `${JSON.stringify(raw, null, 2)}\n`);
  io.print(message);
  return 0;
}

function rawCompatibility(raw: RawConfig): RawRecord {
  if (!raw.compatibility || typeof raw.compatibility !== "object" || Array.isArray(raw.compatibility)) {
    raw.compatibility = {};
  }
  return raw.compatibility as RawRecord;
}

/**
 * Records a Copilot CLI version that "chr verify --accept" proved compatible.
 * Already-listed versions are a no-op so the file is never rewritten needlessly.
 */
export async function acceptTestedCliVersion(
  version: string,
  io: ManageIo,
): Promise<{ exitCode: number; accepted: boolean }> {
  const existing = await readConfigFile(configPath());
  if (existing.revision?.compatibility.testedCliVersions.includes(version)) {
    io.print(`${version} is already in compatibility.testedCliVersions in ${existing.path}.`);
    return { exitCode: 0, accepted: true };
  }
  const exitCode = await updateConfig(io, (raw) => {
    const compatibility = rawCompatibility(raw);
    const versions = Array.isArray(compatibility.testedCliVersions) ? compatibility.testedCliVersions : [];
    if (versions.includes(version)) {
      return `${version} is already in compatibility.testedCliVersions in ${configPath()}.`;
    }
    compatibility.testedCliVersions = [...versions, version];
    return `Added ${version} to compatibility.testedCliVersions in ${configPath()}.`;
  });
  return { exitCode, accepted: exitCode === 0 };
}

function rawProviders(raw: RawConfig): Record<string, RawRecord> {
  if (!raw.providers || typeof raw.providers !== "object" || Array.isArray(raw.providers)) raw.providers = {};
  return raw.providers as Record<string, RawRecord>;
}

function rawModels(raw: RawConfig): RawRecord[] {
  if (!Array.isArray(raw.models)) raw.models = [];
  return raw.models as RawRecord[];
}

// -------------------------------------------------------------------- provider

async function credentialResolves(ref: string): Promise<boolean> {
  if (ref.startsWith("env:")) return process.env[ref.slice(4)] !== undefined;
  if (ref.startsWith("windows-credential:")) {
    // Existence only: listing must never decrypt a stored secret.
    return (await listSecretNames()).includes(ref.slice("windows-credential:".length));
  }
  return false;
}

async function providerList(io: ManageIo): Promise<number> {
  const revision = await loadValidConfig(io);
  if (!revision) return 1;
  const names = Object.keys(revision.providers);
  if (names.length === 0) {
    io.print("No providers are configured.");
    return 0;
  }
  const rows: string[][] = [];
  for (const name of names) {
    const provider = revision.providers[name];
    rows.push([
      name,
      provider.protocol,
      provider.baseUrl,
      provider.credentialRef,
      (await credentialResolves(provider.credentialRef)) ? "yes" : "no",
    ]);
  }
  io.print(table(["NAME", "PROTOCOL", "BASE URL", "CREDENTIAL REF", "RESOLVES"], rows));
  return 0;
}

function assertHttpUrl(value: string): void {
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    throw new ManageError(`--base-url must be an HTTP(S) URL (got ${value}).`);
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new ManageError(`--base-url must be an HTTP(S) URL (got ${value}).`);
  }
}

async function providerAdd(io: ManageIo, args: string[]): Promise<number> {
  const parsed = parseArgs(args, {
    values: ["--base-url", "--key-env", "--key-secret", "--protocol"],
    flags: ["--force"],
  });
  if (!parsed || parsed.positional.length !== 1) {
    io.usage();
    return 2;
  }
  const name = parsed.positional[0];
  const baseUrl = parsed.values["--base-url"];
  const keyEnv = parsed.values["--key-env"];
  const keySecret = parsed.values["--key-secret"];
  const protocolFlag = parsed.values["--protocol"] ?? "chat";
  if (!baseUrl || (keyEnv === undefined) === (keySecret === undefined)) {
    io.usage();
    return 2;
  }
  if (protocolFlag !== "chat" && protocolFlag !== "responses") {
    io.usage();
    return 2;
  }
  const protocol: Protocol = protocolFlag === "responses" ? "openai-responses" : "openai-chat-completions";
  const credentialRef = keyEnv ? `env:${keyEnv}` : `windows-credential:${keySecret}`;
  const hint = keySecret && !(await listSecretNames()).includes(keySecret)
    ? `\nSecret ${keySecret} is not stored yet. Run "chr secret set ${keySecret}".`
    : "";
  return await updateConfig(io, (raw) => {
    const providers = rawProviders(raw);
    const replacing = Boolean(providers[name]);
    if (replacing && !parsed.flags.has("--force")) {
      throw new ManageError(`Provider ${name} already exists. Use --force to replace it.`);
    }
    assertHttpUrl(baseUrl);
    providers[name] = { protocol, baseUrl, credentialRef, timeoutsMs: { ...defaultTimeoutsMs } };
    return `${replacing ? "Replaced" : "Added"} provider ${name} (${protocol}) with credentialRef ${credentialRef}.${hint}`;
  });
}

async function providerRemove(io: ManageIo, args: string[]): Promise<number> {
  if (args.length !== 1 || args[0].startsWith("--")) {
    io.usage();
    return 2;
  }
  const name = args[0];
  return await updateConfig(io, (raw) => {
    const providers = rawProviders(raw);
    if (!providers[name]) throw new ManageError(`Provider ${name} was not found.`);
    const used = rawModels(raw)
      .filter((model) => model.provider === name)
      .map((model) => String(model.alias));
    if (used.length > 0) {
      throw new ManageError(`Provider ${name} is still used by: ${used.join(", ")}. Remove those models first.`);
    }
    delete providers[name];
    return `Removed provider ${name}.`;
  });
}

type Probe = { label: string; ok: boolean };

async function timedFetch(
  url: string,
  init: RequestInit,
  timeoutMs: number,
): Promise<{ status: number; ms: number; text: string } | { error: string; ms: number }> {
  const started = Date.now();
  try {
    const response = await fetch(url, { ...init, redirect: "manual", signal: AbortSignal.timeout(timeoutMs) });
    const text = await response.text().catch(() => "");
    return { status: response.status, ms: Date.now() - started, text };
  } catch (error) {
    return { error: error instanceof Error ? error.message : String(error), ms: Date.now() - started };
  }
}

function join(baseUrl: string, suffix: string): string {
  return `${baseUrl.replace(/\/+$/, "")}${suffix}`;
}

async function providerTest(io: ManageIo, args: string[]): Promise<number> {
  const parsed = parseArgs(args, { values: ["--model", "--timeout"] });
  if (!parsed || parsed.positional.length !== 1) {
    io.usage();
    return 2;
  }
  const revision = await loadValidConfig(io);
  if (!revision) return 1;
  const name = parsed.positional[0];
  const provider = revision.providers[name];
  if (!provider) {
    io.print(`Unknown provider ${name}.`);
    return 1;
  }
  let timeoutMs = 30000;
  try {
    if (parsed.values["--timeout"]) timeoutMs = positiveInteger(parsed.values["--timeout"], "--timeout");
  } catch (error) {
    io.print((error as Error).message);
    return 1;
  }

  const ref = provider.credentialRef;
  const credential = ref.startsWith("windows-credential:")
    ? await getSecret(ref.slice("windows-credential:".length))
    : resolveCredential(ref);
  if (!credential) {
    io.print(`Credential ${ref} could not be resolved.${ref.startsWith("windows-credential:")
      ? ` Run "chr secret set ${ref.slice("windows-credential:".length)}".`
      : ""}`);
    return 1;
  }
  const safe = (text: string): string => scrubSecrets(text.split(credential).join("REDACTED"));
  const headers = { authorization: `Bearer ${credential}`, "content-type": "application/json" };
  const probes: Probe[] = [];

  const modelsUrl = join(provider.baseUrl, "/models");
  const models = await timedFetch(modelsUrl, { method: "GET", headers }, timeoutMs);
  let ids: string[] = [];
  if ("error" in models) {
    io.print(`GET ${modelsUrl} -> request failed after ${models.ms} ms: ${safe(models.error)}`);
    probes.push({ label: "models", ok: false });
  } else {
    const ok = models.status >= 200 && models.status < 300;
    probes.push({ label: "models", ok });
    io.print(`GET ${modelsUrl} -> ${models.status} in ${models.ms} ms${
      models.status >= 300 && models.status < 400 ? " (redirect not followed; treated as failure)" : ""}`);
    if (ok) {
      try {
        const body = JSON.parse(models.text) as { data?: Array<{ id?: unknown }> };
        ids = (body.data ?? []).map((item) => String(item.id)).filter((id) => id !== "undefined");
      } catch {
        io.print("  /models response was not JSON.");
      }
      if (ids.length > 0) io.print(ids.slice(0, 20).map((id) => `  ${id}`).join("\n"));
      if (ids.length > 20) io.print(`  ... and ${ids.length - 20} more`);
    } else if (models.text) {
      io.print(`  ${safe(models.text).slice(0, 200)}`);
    }
  }

  const configured = revision.models.filter((model) => model.provider === name);
  const upstream = parsed.values["--model"] ??
    (configured.length === 1 ? configured[0].upstreamModel : undefined);
  for (const model of configured) {
    if (ids.length > 0 && !ids.includes(model.upstreamModel)) {
      io.print(`Warning: model ${model.alias} uses upstreamModel ${model.upstreamModel}, which the provider did not list.`);
    }
  }

  if (upstream) {
    const responses = provider.protocol === "openai-responses";
    const url = join(provider.baseUrl, responses ? "/responses" : "/chat/completions");
    const body = responses
      ? { model: upstream, input: "Reply with exactly: OK", max_output_tokens: 64 }
      : {
        model: upstream,
        messages: [{ role: "user", content: "Reply with exactly: OK" }],
        max_tokens: 64,
        stream: false,
      };
    const chat = await timedFetch(url, { method: "POST", headers, body: JSON.stringify(body) }, timeoutMs);
    if ("error" in chat) {
      io.print(`POST ${url} -> request failed after ${chat.ms} ms: ${safe(chat.error)}`);
      probes.push({ label: "completion", ok: false });
    } else {
      const ok = chat.status >= 200 && chat.status < 300;
      probes.push({ label: "completion", ok });
      io.print(`POST ${url} (${upstream}) -> ${chat.status} in ${chat.ms} ms${
        chat.status >= 300 && chat.status < 400 ? " (redirect not followed; treated as failure)" : ""}`);
      const reply = ok ? extractReply(chat.text) : { source: "raw" as const, text: chat.text };
      for (const line of formatReply(reply, safe)) io.print(line);
    }
  } else {
    io.print("No completion probe: pass --model <upstreamId> or configure exactly one model for this provider.");
  }

  const passed = probes.every((probe) => probe.ok);
  io.print(passed ? `Provider ${name}: OK` : `Provider ${name}: FAILED`);
  return passed ? 0 : 1;
}

export type ProbeReply = {
  /** "content" is real output; "reasoning" is a thinking-only reply; "raw" means unparseable. */
  source: "content" | "reasoning" | "empty" | "raw";
  text: string;
  finishReason?: string;
};

function firstText(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

/**
 * Pulls the assistant reply out of a chat-completions or responses payload.
 * Reasoning models often spend the whole token budget in `reasoning_content`
 * and return empty `content`, so that is reported explicitly rather than
 * falling back to dumping raw JSON.
 */
export function extractReply(text: string): ProbeReply {
  let body: Record<string, unknown>;
  try {
    body = JSON.parse(text) as Record<string, unknown>;
  } catch {
    return { source: "raw", text };
  }
  const choices = body.choices as Array<{
    message?: Record<string, unknown>;
    finish_reason?: unknown;
  }> | undefined;
  if (Array.isArray(choices) && choices.length > 0) {
    const choice = choices[0];
    const message = choice.message ?? {};
    const finishReason = typeof choice.finish_reason === "string" ? choice.finish_reason : undefined;
    const content = firstText(message.content);
    if (content) return { source: "content", text: content, finishReason };
    const reasoning = firstText(message.reasoning_content) ?? firstText(message.reasoning);
    if (reasoning) return { source: "reasoning", text: reasoning, finishReason };
    return { source: "empty", text: "", finishReason };
  }

  const incomplete = body.incomplete_details as { reason?: unknown } | undefined;
  const finishReason = incomplete?.reason === "max_output_tokens"
    ? "length"
    : typeof incomplete?.reason === "string"
      ? incomplete.reason
      : typeof body.status === "string"
        ? body.status
        : undefined;
  const outputText = firstText(body.output_text);
  if (outputText) return { source: "content", text: outputText, finishReason };
  const output = body.output as Array<{ content?: Array<Record<string, unknown>> }> | undefined;
  if (Array.isArray(output)) {
    const joined = output
      .flatMap((item) => item.content ?? [])
      .map((part) => firstText(part.text) ?? "")
      .join("");
    if (joined) return { source: "content", text: joined, finishReason };
    const reasoning = output
      .flatMap((item) => item.content ?? [])
      .map((part) => firstText(part.reasoning_content) ?? firstText(part.reasoning) ?? "")
      .join("");
    if (reasoning) return { source: "reasoning", text: reasoning, finishReason };
    return { source: "empty", text: "", finishReason };
  }
  return { source: "raw", text };
}

/** Renders the reply lines for `chr provider test`; `safe` redacts credentials. */
export function formatReply(reply: ProbeReply, safe: (text: string) => string): string[] {
  const suffix = reply.finishReason ? ` (finish_reason: ${reply.finishReason})` : "";
  if (reply.source === "raw") {
    return reply.text ? [`  ${safe(reply.text).slice(0, 80)}`] : [];
  }
  if (reply.source === "empty") {
    const lines = [`  reply: ""${suffix}`];
    if (reply.finishReason === "length") {
      lines.push("  reply truncated by token limit (reasoning model?) — connectivity OK");
    }
    return lines;
  }
  const label = reply.source === "reasoning" ? "reply (reasoning)" : "reply";
  return [`  ${label}: "${safe(reply.text).slice(0, 80)}"${suffix}`];
}

export async function runProviderCommand(args: string[], io: ManageIo): Promise<number> {
  const action = args[0];
  const rest = args.slice(1);
  if (action === "list" && rest.length === 0) return await providerList(io);
  if (action === "add") return await providerAdd(io, rest);
  if (action === "remove") return await providerRemove(io, rest);
  if (action === "test") return await providerTest(io, rest);
  io.usage();
  return 2;
}

// ----------------------------------------------------------------------- model

async function modelList(io: ManageIo): Promise<number> {
  const revision = await loadValidConfig(io);
  if (!revision) return 1;
  if (revision.models.length === 0) {
    io.print("No models are configured.");
    return 0;
  }
  io.print(table(
    ["ALIAS", "DISPLAY NAME", "PROVIDER", "UPSTREAM MODEL", "TOOLS", "VISION", "CONTEXT"],
    revision.models.map((model) => [
      model.alias,
      model.displayName,
      model.provider,
      model.upstreamModel,
      model.capabilities.tools ? "yes" : "no",
      model.capabilities.vision ? "yes" : "no",
      model.capabilities.contextWindowTokens === null ? "-" : String(model.capabilities.contextWindowTokens),
    ]),
  ));
  return 0;
}

async function modelAdd(io: ManageIo, args: string[]): Promise<number> {
  const parsed = parseArgs(args, {
    values: ["--provider", "--upstream", "--display-name", "--context", "--max-output"],
    flags: ["--no-tools", "--vision", "--force"],
  });
  if (!parsed || parsed.positional.length !== 1) {
    io.usage();
    return 2;
  }
  const alias = parsed.positional[0];
  const providerName = parsed.values["--provider"];
  const upstreamModel = parsed.values["--upstream"];
  if (!providerName || !upstreamModel) {
    io.usage();
    return 2;
  }
  return await updateConfig(io, (raw) => {
    if (!aliasPattern.test(alias)) {
      throw new ManageError(`Alias ${alias} must match ${aliasPattern.source}.`);
    }
    if (!rawProviders(raw)[providerName]) {
      throw new ManageError(`Unknown provider ${providerName}. Add it with "chr provider add".`);
    }
    const models = rawModels(raw);
    const existing = models.findIndex((model) => model.alias === alias);
    if (existing >= 0 && !parsed.flags.has("--force")) {
      throw new ManageError(`Model ${alias} already exists. Use --force to replace it.`);
    }
    const capabilities: Record<string, unknown> = {
      streaming: true,
      tools: !parsed.flags.has("--no-tools"),
      vision: parsed.flags.has("--vision"),
      contextWindowTokens: parsed.values["--context"]
        ? positiveInteger(parsed.values["--context"], "--context")
        : null,
    };
    if (parsed.values["--max-output"]) {
      capabilities.maxOutputTokens = positiveInteger(parsed.values["--max-output"], "--max-output");
    }
    const entry: RawRecord = {
      alias,
      displayName: parsed.values["--display-name"] ?? alias,
      provider: providerName,
      upstreamModel,
      capabilities,
    };
    if (existing >= 0) models[existing] = entry;
    else models.push(entry);
    return `${existing >= 0 ? "Replaced" : "Added"} model ${alias} -> ${providerName}/${upstreamModel}.`;
  });
}

async function modelRemove(io: ManageIo, args: string[]): Promise<number> {
  if (args.length !== 1 || args[0].startsWith("--")) {
    io.usage();
    return 2;
  }
  const alias = args[0];
  return await updateConfig(io, (raw) => {
    const models = rawModels(raw);
    const index = models.findIndex((model) => model.alias === alias);
    if (index < 0) throw new ManageError(`Model ${alias} was not found.`);
    models.splice(index, 1);
    return `Removed model ${alias}.`;
  });
}

export async function runModelCommand(args: string[], io: ManageIo): Promise<number> {
  const action = args[0];
  const rest = args.slice(1);
  if (action === "list" && rest.length === 0) return await modelList(io);
  if (action === "add") return await modelAdd(io, rest);
  if (action === "remove") return await modelRemove(io, rest);
  io.usage();
  return 2;
}

// --------------------------------------------------------------- recordings

function entryFlags(entry: RawRecord): string {
  const request = entry.request as RawRecord | undefined;
  const response = entry.response as RawRecord | undefined;
  const flags = [request || response ? "full" : "metadata"];
  if (request?.truncated === true || response?.truncated === true) flags.push("truncated");
  if (entry.canceled === true) flags.push("canceled");
  return flags.join(",");
}

async function recordList(io: ManageIo, args: string[]): Promise<number> {
  const parsed = parseArgs(args, { values: ["--limit"] });
  if (!parsed || parsed.positional.length > 0) {
    io.usage();
    return 2;
  }
  let limit = 20;
  try {
    if (parsed.values["--limit"]) limit = positiveInteger(parsed.values["--limit"], "--limit");
  } catch (error) {
    io.print((error as Error).message);
    return 1;
  }
  const files = (await listRecordingFiles()).slice(0, limit);
  if (files.length === 0) {
    io.print(`No recordings in ${recordingDirectory()}.`);
    return 0;
  }
  const rows: string[][] = [];
  for (const file of files) {
    const entry = (await readRecordingFile(file.id)) ?? {};
    rows.push([
      file.id,
      String(entry.timestamp ?? "-"),
      String(entry.requestedModel ?? "-"),
      String(entry.status ?? "-"),
      String(entry.durationMs ?? "-"),
      entryFlags(entry),
    ]);
  }
  io.print(table(["ID", "TIMESTAMP", "REQUESTED MODEL", "STATUS", "DURATION MS", "FLAGS"], rows));
  return 0;
}

async function recordShow(io: ManageIo, args: string[]): Promise<number> {
  if (args.length !== 1 || args[0].startsWith("--")) {
    io.usage();
    return 2;
  }
  const id = args[0];
  if (!isRecordingId(id)) {
    io.print(`Invalid recording id ${id}.`);
    return 1;
  }
  const entry = await readRecordingFile(id);
  if (!entry) {
    io.print(`Recording ${id} was not found.`);
    return 1;
  }
  io.print(JSON.stringify(entry, null, 2));
  return 0;
}

async function recordClear(io: ManageIo, args: string[]): Promise<number> {
  const parsed = parseArgs(args, { values: ["--older-than"] });
  if (!parsed || parsed.positional.length > 0) {
    io.usage();
    return 2;
  }
  let cutoff = Number.POSITIVE_INFINITY;
  try {
    if (parsed.values["--older-than"]) {
      cutoff = Date.now() - positiveInteger(parsed.values["--older-than"], "--older-than") * 86400000;
    }
  } catch (error) {
    io.print((error as Error).message);
    return 1;
  }
  let deleted = 0;
  for (const file of await listRecordingFiles()) {
    if (cutoff !== Number.POSITIVE_INFINITY && file.mtimeMs >= cutoff) continue;
    if (await deleteRecordingFile(file.id)) deleted += 1;
  }
  io.print(`Deleted ${deleted} recording${deleted === 1 ? "" : "s"}.`);
  return 0;
}

const exportNotes = [
  "Only locally served custom-model traffic is recorded; GitHub pass-through traffic is not captured by CHR.",
  "Credentials and auth headers are redacted.",
];

async function recordExport(io: ManageIo, args: string[]): Promise<number> {
  const parsed = parseArgs(args, { values: ["--since"] });
  if (!parsed || parsed.positional.length !== 1) {
    io.usage();
    return 2;
  }
  const outFile = parsed.positional[0];
  let since = Number.NEGATIVE_INFINITY;
  if (parsed.values["--since"]) {
    since = Date.parse(parsed.values["--since"]);
    if (Number.isNaN(since)) {
      io.print(`--since must be an ISO-8601 timestamp (got ${parsed.values["--since"]}).`);
      return 1;
    }
  }
  const config = await readConfigFile();
  const recordings: RawRecord[] = [];
  const completeness = { truncated: 0, canceled: 0, metadataOnly: 0 };
  for (const file of (await listRecordingFiles()).reverse()) {
    const entry = await readRecordingFile(file.id);
    if (!entry) continue;
    const stamp = typeof entry.timestamp === "string" ? Date.parse(entry.timestamp) : Number.NaN;
    const at = Number.isNaN(stamp) ? file.mtimeMs : stamp;
    if (at < since) continue;
    const request = entry.request as RawRecord | undefined;
    const response = entry.response as RawRecord | undefined;
    if (request?.truncated === true || response?.truncated === true) completeness.truncated += 1;
    if (entry.canceled === true) completeness.canceled += 1;
    if (!request && !response) completeness.metadataOnly += 1;
    recordings.push(sanitizeForExport({ id: file.id, ...entry }) as RawRecord);
  }
  const bundle = {
    manifest: {
      exportedAt: new Date().toISOString(),
      chrVersion,
      count: recordings.length,
      recordingMode: readRecordingMode(config.revision?.recording ?? defaultRecordingConfig),
      notes: exportNotes,
      completeness,
    },
    recordings,
  };
  await writeFile(outFile, `${JSON.stringify(bundle, null, 2)}\n`, { encoding: "utf8", mode: 0o600 });
  io.print(`Exported ${recordings.length} recording${recordings.length === 1 ? "" : "s"} to ${outFile}.`);
  return 0;
}

export async function runRecordSubcommand(args: string[], io: ManageIo): Promise<number | undefined> {
  const action = args[0];
  const rest = args.slice(1);
  if (action === "list") return await recordList(io, rest);
  if (action === "show") return await recordShow(io, rest);
  if (action === "clear") return await recordClear(io, rest);
  if (action === "export") return await recordExport(io, rest);
  return undefined;
}
