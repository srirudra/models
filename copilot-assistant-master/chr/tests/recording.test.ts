import { mkdtemp, readFile, readdir, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import {
  createRecorder, readRecordingMode, recordingDirectory, resetRecordingTestSeams,
  setRecordingTestSeams, writeRecordingMode,
} from "../src/recording/recorder.js";

const config = {
  defaultMode: "metadata", retentionDays: 30, maxTotalMiB: 2, maxBodyMiBPerRequest: 1, onWriteFailure: "warn",
};
const dirs: string[] = [];
afterEach(async () => {
  resetRecordingTestSeams();
  for (const dir of dirs.splice(0)) await rm(dir, { recursive: true, force: true });
});

describe("recording", () => {
  it("resolves state modes and falls back to configuration", async () => {
    const dir = await mkdtemp(path.join(os.tmpdir(), "chr-recording-")); dirs.push(dir);
    setRecordingTestSeams({ directory: dir });
    expect(readRecordingMode(config)).toBe("metadata");
    await writeRecordingMode("full");
    expect(readRecordingMode(config)).toBe("full");
    await writeRecordingMode("off");
    expect(readRecordingMode(config)).toBe("off");
  });

  it("redacts credentials while retaining ordinary content", async () => {
    const dir = await mkdtemp(path.join(os.tmpdir(), "chr-recording-")); dirs.push(dir);
    setRecordingTestSeams({ directory: dir });
    await writeRecordingMode("full");
    const recorder = createRecorder(config);
    await recorder.record({
      timestamp: new Date().toISOString(), request: {
        headers: { authorization: "Bearer secret", cookie: "session", "x-request-token": "token", "x-content": "hello" },
        body: { api_key: "secret", authorization: "also-secret", prompt: "keep this",
        },
      }, response: { bodyText: "answer", truncated: false },
    });
    const file = (await readdir(recordingDirectory())).find((name) => name.endsWith(".json"))!;
    const text = await readFile(path.join(recordingDirectory(), file), "utf8");
    expect(text).toContain("REDACTED");
    expect(text).toContain("keep this");
    expect(text).not.toContain("Bearer secret");
  });

  it("never throws when writing is disabled by a size limit", async () => {
    const dir = await mkdtemp(path.join(os.tmpdir(), "chr-recording-")); dirs.push(dir);
    setRecordingTestSeams({ directory: dir });
    await writeRecordingMode("metadata");
    const recorder = createRecorder({ ...config, maxTotalMiB: 0 }, { logger: () => undefined });
    await expect(recorder.record({ timestamp: "now" })).resolves.toBeUndefined();
  });
});
