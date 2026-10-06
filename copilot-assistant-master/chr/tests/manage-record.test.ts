import { mkdir, readFile, utimes, writeFile } from "node:fs/promises";
import path from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
  recordingDirectory, resetRecordingTestSeams, setRecordingTestSeams, writeRecordingMode,
} from "../src/recording/recorder.js";
import { removeAll, run, seedConfigFile, tempDirectory, withEnv } from "./manage-support.js";

const directories: string[] = [];
const restore: Array<() => void> = [];
let directory = "";

async function writeRecording(
  id: string,
  entry: Record<string, unknown>,
  ageDays = 0,
): Promise<string> {
  await mkdir(recordingDirectory(), { recursive: true });
  const file = path.join(recordingDirectory(), `${id}.json`);
  await writeFile(file, JSON.stringify(entry), "utf8");
  if (ageDays > 0) {
    const when = new Date(Date.now() - ageDays * 86400000);
    await utimes(file, when, when);
  }
  return file;
}

const fullEntry = {
  timestamp: "2026-01-02T03:04:05.000Z",
  requestedModel: "alpha",
  upstreamModel: "gpt-test",
  provider: "local",
  status: 200,
  durationMs: 1234,
  streaming: true,
  canceled: false,
  request: {
    headers: { authorization: "Bearer xyz", "x-trace": "keep-me" },
    body: { prompt: "my key is sk-abcdefghijkl" },
  },
  response: { bodyText: "answer with Bearer xyz and sk-abcdefghijkl", truncated: true },
};

const metadataEntry = {
  timestamp: "2026-01-01T00:00:00.000Z",
  requestedModel: "beta",
  upstreamModel: "gpt-beta",
  provider: "local",
  status: 499,
  durationMs: 10,
  streaming: true,
  canceled: true,
};

beforeEach(async () => {
  directory = await tempDirectory("chr-record-");
  directories.push(directory);
  setRecordingTestSeams({ directory });
  restore.push(withEnv("CHR_CONFIG_PATH", await seedConfigFile(directory)));
});

afterEach(async () => {
  resetRecordingTestSeams();
  while (restore.length > 0) restore.pop()!();
  await removeAll(directories);
});

describe("chr record list/show/clear", () => {
  it("lists recordings newest first with flags", async () => {
    await writeRecording("20260101-old", metadataEntry, 5);
    await writeRecording("20260102-new", fullEntry);

    const result = await run(["record", "list"]);
    expect(result.code).toBe(0);
    const lines = result.out.trim().split("\n");
    expect(lines[0]).toContain("ID");
    expect(lines[1]).toContain("20260102-new");
    expect(lines[1]).toContain("alpha");
    expect(lines[1]).toContain("1234");
    expect(lines[1]).toContain("full,truncated");
    expect(lines[2]).toContain("metadata,canceled");
  });

  it("honours --limit", async () => {
    await writeRecording("20260101-old", metadataEntry, 5);
    await writeRecording("20260102-new", fullEntry);
    const result = await run(["record", "list", "--limit", "1"]);
    expect(result.out).toContain("20260102-new");
    expect(result.out).not.toContain("20260101-old");
  });

  it("shows a recording and rejects path traversal ids", async () => {
    await writeRecording("20260102-new", fullEntry);
    const shown = await run(["record", "show", "20260102-new"]);
    expect(shown.code).toBe(0);
    expect(JSON.parse(shown.out).requestedModel).toBe("alpha");

    const traversal = await run(["record", "show", "..\\..\\config"]);
    expect(traversal.code).toBe(1);
    expect(traversal.out).toContain("Invalid recording id");

    const missing = await run(["record", "show", "nosuchid"]);
    expect(missing.code).toBe(1);
    expect(missing.out).toContain("was not found");
  });

  it("clears all recordings or only those older than N days", async () => {
    await writeRecording("20260101-old", metadataEntry, 5);
    await writeRecording("20260102-new", fullEntry);

    const partial = await run(["record", "clear", "--older-than", "2"]);
    expect(partial.code).toBe(0);
    expect(partial.out).toContain("Deleted 1 recording.");
    expect((await run(["record", "list"])).out).toContain("20260102-new");

    const all = await run(["record", "clear"]);
    expect(all.out).toContain("Deleted 1 recording.");
    expect((await run(["record", "list"])).out).toContain("No recordings in");
  });

  it("prints usage for unknown record flags", async () => {
    const result = await run(["record", "list", "--bogus"]);
    expect(result.code).toBe(2);
    expect(result.out).toContain("Usage:");
  });
});

describe("chr record export", () => {
  it("writes a redacted bundle with manifest completeness counts", async () => {
    await writeRecordingMode("full");
    await writeRecording("20260101-old", metadataEntry, 5);
    await writeRecording("20260102-new", fullEntry);
    const outFile = path.join(directory, "bundle.json");

    const result = await run(["record", "export", outFile]);
    expect(result.code).toBe(0);
    expect(result.out).toContain("Exported 2 recordings");

    const text = await readFile(outFile, "utf8");
    expect(text).not.toContain("sk-abcdefghijkl");
    expect(text).not.toContain("Bearer xyz");
    expect(text).toContain("REDACTED");
    expect(text).toContain("keep-me");

    const bundle = JSON.parse(text) as {
      manifest: Record<string, unknown>;
      recordings: Array<Record<string, unknown>>;
    };
    expect(bundle.manifest.count).toBe(2);
    expect(bundle.manifest.chrVersion).toBe("0.1.0");
    expect(bundle.manifest.recordingMode).toBe("full");
    expect(bundle.manifest.completeness).toEqual({ truncated: 1, canceled: 1, metadataOnly: 1 });
    expect(bundle.manifest.notes).toEqual([
      "Only locally served custom-model traffic is recorded; GitHub pass-through traffic is not captured by CHR.",
      "Credentials and auth headers are redacted.",
    ]);
    expect(String(bundle.manifest.exportedAt)).toMatch(/^\d{4}-\d{2}-\d{2}T/);
    const exported = bundle.recordings.find((entry) => entry.id === "20260102-new")!;
    const request = exported.request as { headers: Record<string, string> };
    expect(request.headers.authorization).toBe("REDACTED");
    expect(request.headers["x-trace"]).toBe("keep-me");
  });

  it("filters by --since and rejects a malformed timestamp", async () => {
    await writeRecording("20260101-old", metadataEntry, 5);
    await writeRecording("20260102-new", fullEntry);
    const outFile = path.join(directory, "since.json");

    const result = await run(["record", "export", outFile, "--since", "2026-01-02T00:00:00.000Z"]);
    expect(result.code).toBe(0);
    const bundle = JSON.parse(await readFile(outFile, "utf8")) as { recordings: Array<{ id: string }> };
    expect(bundle.recordings.map((entry) => entry.id)).toEqual(["20260102-new"]);

    const bad = await run(["record", "export", outFile, "--since", "not-a-date"]);
    expect(bad.code).toBe(1);
    expect(bad.out).toContain("ISO-8601");
  });
});
