import { randomBytes } from "node:crypto";
import { readFileSync, statSync } from "node:fs";
import { mkdir, readdir, readFile, stat, unlink, writeFile } from "node:fs/promises";
import path from "node:path";
import type { ConfigRevision } from "../config/types.js";

export type RecordingMode = "off" | "metadata" | "full";
type RecordingConfig = ConfigRevision["recording"];
type RecordingEntry = Record<string, unknown>;

let directoryOverride: string | undefined;
export function setRecordingTestSeams(options: { directory?: string; directoryOverride?: string }): void {
  directoryOverride = options.directoryOverride ?? options.directory;
}
export function resetRecordingTestSeams(): void { directoryOverride = undefined; }
export function recordingDirectory(): string {
  return path.join(directoryOverride ?? path.join(
    process.env.LOCALAPPDATA ?? path.join(process.env.USERPROFILE ?? process.cwd(), "AppData", "Local"), "CHR",
  ), "recordings");
}
export function recordingStatePath(): string {
  return path.join(directoryOverride ?? path.join(
    process.env.LOCALAPPDATA ?? path.join(process.env.USERPROFILE ?? process.cwd(), "AppData", "Local"), "CHR",
  ), "recording-state.json");
}

const modes: readonly RecordingMode[] = ["off", "metadata", "full"];
const valid = (value: unknown): value is RecordingMode => typeof value === "string" && modes.includes(value as RecordingMode);
function configured(config: RecordingConfig): RecordingMode {
  return valid(config.defaultMode) ? config.defaultMode : "off";
}
export function readRecordingMode(config: RecordingConfig): RecordingMode {
  try {
    const value = JSON.parse(readFileSync(recordingStatePath(), "utf8")) as { mode?: unknown };
    return valid(value.mode) ? value.mode : configured(config);
  } catch { return configured(config); }
}
export async function writeRecordingMode(mode: RecordingMode): Promise<void> {
  await mkdir(path.dirname(recordingStatePath()), { recursive: true });
  await writeFile(recordingStatePath(), JSON.stringify({ mode, updatedAt: new Date().toISOString() }, null, 2), { mode: 0o600 });
}

function redactHeaders(headers: unknown): Record<string, unknown> {
  const result: Record<string, unknown> = {};
  for (const [name, value] of Object.entries((headers ?? {}) as Record<string, unknown>)) {
    const lower = name.toLowerCase();
    result[name] = lower === "authorization" || lower === "proxy-authorization" ||
      lower === "cookie" || lower === "set-cookie" || lower === "x-api-key" ||
      /token|key|secret/.test(lower) ? "REDACTED" : value;
  }
  return result;
}
function redactBody(body: unknown): unknown {
  if (!body || typeof body !== "object" || Array.isArray(body)) return body;
  const result = { ...(body as Record<string, unknown>) };
  for (const key of ["api_key", "authorization"]) if (key in result) result[key] = "REDACTED";
  return result;
}
function compactTimestamp(date: Date): string {
  return date.toISOString().replace(/[-:.TZ]/g, "").slice(0, 17);
}

export type RecordingFileInfo = { id: string; path: string; mtimeMs: number; size: number };

const recordingIdPattern = /^[0-9A-Za-z-]+$/;

/** Recording ids are filenames; reject anything that could escape the directory. */
export function isRecordingId(id: string): boolean {
  return recordingIdPattern.test(id);
}

/** Read-only listing of stored recordings, newest first. */
export async function listRecordingFiles(): Promise<RecordingFileInfo[]> {
  let names: string[];
  try {
    names = (await readdir(recordingDirectory(), { withFileTypes: true }))
      .filter((entry) => entry.isFile() && entry.name.endsWith(".json"))
      .map((entry) => entry.name);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return [];
    throw error;
  }
  const files = await Promise.all(names.map(async (name) => {
    const full = path.join(recordingDirectory(), name);
    const info = await stat(full);
    return { id: name.slice(0, -".json".length), path: full, mtimeMs: info.mtimeMs, size: info.size };
  }));
  return files.sort((a, b) => b.mtimeMs - a.mtimeMs || b.id.localeCompare(a.id));
}

/** Read-only load of a single recording by id. */
export async function readRecordingFile(id: string): Promise<Record<string, unknown> | undefined> {
  if (!isRecordingId(id)) throw new Error(`invalid recording id ${id}`);
  try {
    const text = await readFile(path.join(recordingDirectory(), `${id}.json`), "utf8");
    return JSON.parse(text) as Record<string, unknown>;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return undefined;
    throw error;
  }
}

export async function deleteRecordingFile(id: string): Promise<boolean> {
  if (!isRecordingId(id)) throw new Error(`invalid recording id ${id}`);
  try {
    await unlink(path.join(recordingDirectory(), `${id}.json`));
    return true;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return false;
    throw error;
  }
}

export function createRecorder(
  config: RecordingConfig,
  deps: { logger?: (message: string) => void } = {},
): { currentMode(): RecordingMode; record(entry: RecordingEntry): Promise<void>; maxBodyBytes: number } {
  const logger = deps.logger ?? (() => undefined);
  const cap = Math.max(0, (Number(config.maxBodyMiBPerRequest) || 0) * 1024 * 1024);
  let cachedMtime = -1;
  let cachedSize = -1;
  let lastStateRead = 0;
  let cachedMode: RecordingMode = configured(config);
  let disabled = false;
  const failure = (error: unknown): void => {
    const message = error instanceof Error ? error.message : String(error);
    if (config.onWriteFailure === "ignore") return;
    logger(`recording write failed: ${message}`);
    if (config.onWriteFailure === "disable") disabled = true;
  };
  const prune = async (): Promise<void> => {
    const files = (await readdir(recordingDirectory(), { withFileTypes: true })).filter((x) => x.isFile() && x.name.endsWith(".json"));
    const now = Date.now();
    const retention = Math.max(0, Number(config.retentionDays) || 0) * 86400000;
    for (const file of files) {
      const full = path.join(recordingDirectory(), file.name);
      if (now - (await stat(full)).mtimeMs > retention) await unlink(full);
    }
  };
  const currentMode = (): RecordingMode => {
    if (disabled) return "off";
    try {
      const state = statSync(recordingStatePath());
      const mtime = state.mtimeMs;
      // Windows can retain a coarse mtime (and all modes have similarly sized
      // state files).  Re-read briefly after a change so a mode switch during
      // an in-flight request is observed before its recording is written.
      if (mtime !== cachedMtime || state.size !== cachedSize || Date.now() - lastStateRead < 2000) {
        cachedMtime = mtime;
        cachedSize = state.size;
        lastStateRead = Date.now();
        const value = JSON.parse(readFileSync(recordingStatePath(), "utf8")) as { mode?: unknown };
        cachedMode = valid(value.mode) ? value.mode : configured(config);
      }
    } catch {
      if (cachedMtime !== -2) { cachedMtime = -2; cachedSize = -1; cachedMode = configured(config); }
    }
    return cachedMode;
  };
  const ready = (async () => {
    try { await mkdir(recordingDirectory(), { recursive: true }); await prune(); }
    catch (error) { failure(error); }
  })();
  return {
    currentMode,
    maxBodyBytes: cap,
    async record(entry): Promise<void> {
      await ready;
      if (disabled || currentMode() === "off") return;
      try {
        // The request may have lasted long enough for recording mode to be
        // changed after it was captured.  Apply the mode at write time.
        const modeAtWrite = currentMode();
        if (modeAtWrite === "off") return;
        await mkdir(recordingDirectory(), { recursive: true });
        await prune();
        const value: RecordingEntry = { ...entry };
        if (modeAtWrite === "metadata") {
          delete value.request;
          delete value.response;
        }
        if (value.request && typeof value.request === "object") {
          const request = value.request as Record<string, unknown>;
          const requestValue: Record<string, unknown> = {
            ...request, headers: redactHeaders(request.headers), body: redactBody(request.body),
          };
          const requestText = JSON.stringify(requestValue.body);
          if (Buffer.byteLength(requestText) > cap) {
            requestValue.body = Buffer.from(requestText).subarray(0, cap).toString("utf8");
            requestValue.truncated = true;
          }
          value.request = requestValue;
        }
        if (value.response && typeof value.response === "object") {
          const response = { ...(value.response as Record<string, unknown>) };
          let text = typeof response.bodyText === "string" ? response.bodyText : "";
          response.truncated = Boolean(response.truncated);
          if (Buffer.byteLength(text) > cap) { text = Buffer.from(text).subarray(0, cap).toString("utf8"); response.truncated = true; }
          response.bodyText = text;
          value.response = response;
        }
        const output = Buffer.from(JSON.stringify(value));
        const files = (await readdir(recordingDirectory(), { withFileTypes: true })).filter((x) => x.isFile() && x.name.endsWith(".json"));
        let total = 0;
        const sized = await Promise.all(files.map(async (file) => ({ file, size: (await stat(path.join(recordingDirectory(), file.name))).size })));
        total = sized.reduce((sum, item) => sum + item.size, 0);
        const limit = Math.max(0, Number(config.maxTotalMiB) || 0) * 1024 * 1024;
        for (const item of sized.sort((a, b) => statSync(path.join(recordingDirectory(), a.file.name)).mtimeMs - statSync(path.join(recordingDirectory(), b.file.name)).mtimeMs)) {
          if (total + output.length <= limit) break;
          await unlink(path.join(recordingDirectory(), item.file.name)); total -= item.size;
        }
        if (total + output.length > limit) throw new Error("recording size limit reached");
        await writeFile(path.join(recordingDirectory(), `${compactTimestamp(new Date())}-${randomBytes(3).toString("hex")}.json`), output, { mode: 0o600 });
      } catch (error) { failure(error); }
    },
  };
}
