import { mkdtemp, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { vi } from "vitest";
import { runCli } from "../src/cli/main.js";

/** Shared fixtures for the management-command tests. */

export const seedConfig = {
  _comment: "unknown keys must survive CHR writes",
  schemaVersion: 1,
  compatibility: { testedCliVersions: ["1.0.88"], untestedVersionPolicy: "block-custom-routing" },
  providers: {
    local: {
      protocol: "openai-chat-completions",
      baseUrl: "https://example.test/v1",
      credentialRef: "env:CHR_MANAGE_TEST_KEY",
      timeoutsMs: { connect: 10000, firstByte: 120000, streamIdle: 60000, total: 900000 },
    },
  },
  models: [{
    alias: "alpha",
    displayName: "Alpha",
    provider: "local",
    upstreamModel: "gpt-test",
    capabilities: { streaming: true, tools: true, vision: false, contextWindowTokens: null },
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
    maxTotalMiB: 1024,
    maxBodyMiBPerRequest: 16,
    onWriteFailure: "continue-inference-and-alert",
  },
};

export async function tempDirectory(prefix = "chr-manage-"): Promise<string> {
  return await mkdtemp(path.join(os.tmpdir(), prefix));
}

export async function removeAll(directories: string[]): Promise<void> {
  for (const directory of directories.splice(0)) await rm(directory, { recursive: true, force: true });
}

export async function seedConfigFile(
  directory: string,
  overrides: Record<string, unknown> = {},
): Promise<string> {
  const filePath = path.join(directory, "config.json");
  await writeFile(filePath, `${JSON.stringify({ ...seedConfig, ...overrides }, null, 2)}\n`, "utf8");
  return filePath;
}

/** Runs the CLI while capturing everything written to stdout. */
export async function run(args: string[]): Promise<{ code: number; out: string }> {
  let out = "";
  const spy = vi.spyOn(process.stdout, "write").mockImplementation(((chunk: unknown) => {
    out += String(chunk);
    return true;
  }) as typeof process.stdout.write);
  try {
    const code = await runCli(args);
    return { code, out };
  } finally {
    spy.mockRestore();
  }
}

export function withEnv(name: string, value: string | undefined): () => void {
  const original = process.env[name];
  if (value === undefined) delete process.env[name];
  else process.env[name] = value;
  return () => {
    if (original === undefined) delete process.env[name];
    else process.env[name] = original;
  };
}
