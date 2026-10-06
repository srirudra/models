import { mkdtemp, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { readFile } from "node:fs/promises";
import { afterEach, describe, expect, it, vi } from "vitest";
import { loadConfig } from "../src/config/config.js";
import { runCli } from "../src/cli/main.js";
import { runVerify } from "../src/launcher/verify.js";
import { run, seedConfigFile, withEnv } from "./manage-support.js";

const revision = loadConfig({
  schemaVersion: 1,
  compatibility: { testedCliVersions: ["1.0.88", "1.0.89"], untestedVersionPolicy: "block-custom-routing" },
  providers: {},
  models: [],
  routing: {
    unmatchedGitHubModel: "preserve-original-route",
    unknownCustomModel: "error",
    crossProviderFallback: "disabled",
    auxiliary: { policy: "block" },
    githubLeg: { mode: "fail-closed" },
  },
  recording: {
    defaultMode: "off", retentionDays: 1, maxTotalMiB: 1, maxBodyMiBPerRequest: 1,
    onWriteFailure: "continue",
  },
}).revision!;

const fakeCopilot = `import http from "node:http";
const mode = process.env.FAKE_COPILOT_MODE || "ok";
if (process.argv.includes("--version")) { console.log("Copilot 1.0.89"); process.exit(0); }
if (process.env.COPILOT_PROVIDER_API_KEY) process.exit(9);
if (mode === "hang") await new Promise(() => {});
const url = process.env.COPILOT_PROVIDER_BASE_URL + (process.env.COPILOT_PROVIDER_WIRE_API === "responses" ? "/responses" : "/chat/completions");
let body = "";
const request = await new Promise((resolve, reject) => {
  const req = http.request(url, { method: "POST", headers: { "content-type": "application/json" } }, resolve);
  req.on("error", reject);
  req.end(JSON.stringify({ model: mode === "wrong-model" ? "wrong" : process.env.COPILOT_MODEL, stream: true, messages: process.env.COPILOT_PROVIDER_WIRE_API === "responses" ? undefined : [], input: process.env.COPILOT_PROVIDER_WIRE_API === "responses" ? [] : undefined, tools: process.env.COPILOT_PROVIDER_WIRE_API === "responses" ? [{ type: "function", name: "fake" }] : [{ type: "function", function: { name: "fake" } }] }));
});
request.on("data", chunk => body += chunk);
await new Promise(resolve => request.on("end", resolve));
process.stdout.write("CHR verify reply");
`;

const directories: string[] = [];
async function executable(): Promise<string> {
  const directory = await mkdtemp(path.join(os.tmpdir(), "chr-verify-"));
  directories.push(directory);
  const file = path.join(directory, "fake-copilot.mjs");
  await writeFile(file, fakeCopilot);
  return file;
}

afterEach(async () => {
  while (directories.length) await rm(directories.pop()!, { recursive: true, force: true });
});

describe("CHR verify", () => {
  it("passes both probes and strips inherited provider settings", async () => {
    const result = await runVerify({
      revision, copilot: await executable(), timeoutMs: 1000,
      env: { ...process.env, COPILOT_PROVIDER_API_KEY: "must-not-leak", COPILOT_PROVIDER_BASE_URL: "parent" },
    });
    expect(result.exitCode).toBe(0);
    expect(result.checks.map((check) => check.passed)).toEqual([true, true]);
  });

  it("reports a wrong model as a probe failure", async () => {
    const result = await runVerify({
      revision, copilot: await executable(), timeoutMs: 1000,
      env: { ...process.env, FAKE_COPILOT_MODE: "wrong-model" },
    });
    expect(result.exitCode).toBe(1);
    expect(result.output).toContain("body.model is not the probe model");
  });

  it("reports and kills a hanging child quickly", async () => {
    const started = Date.now();
    const result = await runVerify({
      revision, copilot: await executable(), timeoutMs: 50,
      env: { ...process.env, FAKE_COPILOT_MODE: "hang" },
    });
    expect(Date.now() - started).toBeLessThan(2000);
    expect(result.exitCode).toBe(1);
    expect(result.output).toContain("exceeded 50ms timeout");
  });

  it("reports a missing config separately from an untested loaded config", async () => {
    const result = await runVerify({ copilot: await executable(), timeoutMs: 1000 });
    expect(result.configStatus).toBe("missing");
    expect(result.tested).toBe(false);
    expect(result.output).toContain("Version: 1.0.89 (no CHR config; starter template lists it as tested)");
    expect(result.output).toContain('Run "chr config init" to create a config.');
    expect(result.output).not.toContain("Add 1.0.89 to testedCliVersions");
  });

  it("prints actual per-probe results as JSON through the CLI", async () => {
    const copilot = await executable();
    const configPath = path.join(path.dirname(copilot), "missing-config.json");
    const originalConfigPath = process.env.CHR_CONFIG_PATH;
    process.env.CHR_CONFIG_PATH = configPath;
    let stdout = "";
    const write = vi.spyOn(process.stdout, "write").mockImplementation((chunk) => {
      stdout += chunk.toString();
      return true;
    });
    try {
      const exitCode = await runCli(["verify", "--json", "--copilot", copilot, "--timeout", "1000"]);
      expect(exitCode).toBe(0);
    } finally {
      write.mockRestore();
      if (originalConfigPath === undefined) delete process.env.CHR_CONFIG_PATH;
      else process.env.CHR_CONFIG_PATH = originalConfigPath;
    }
    const result = JSON.parse(stdout) as {
      configStatus: string;
      checks: Array<{ wireApi: string; passed: boolean }>;
      exitCode: number;
    };
    expect(result.configStatus).toBe("missing");
    expect(result.exitCode).toBe(0);
    expect(result.checks).toEqual([
      expect.objectContaining({ wireApi: "completions", passed: true }),
      expect.objectContaining({ wireApi: "responses", passed: true }),
    ]);
  });
});

describe("CHR verify --accept", () => {
  const restores: Array<() => void> = [];

  afterEach(() => {
    while (restores.length) restores.pop()!();
  });

  async function setup(): Promise<{ copilot: string; configPath: string }> {
    const copilot = await executable();
    const configPath = await seedConfigFile(path.dirname(copilot));
    restores.push(withEnv("CHR_CONFIG_PATH", configPath));
    return { copilot, configPath };
  }

  async function readConfig(configPath: string): Promise<Record<string, any>> {
    return JSON.parse(await readFile(configPath, "utf8")) as Record<string, any>;
  }

  it("adds the verified version and preserves the rest of the config", async () => {
    const { copilot, configPath } = await setup();
    const result = await run(["verify", "--accept", "--copilot", copilot, "--timeout", "1000"]);
    expect(result.code).toBe(0);
    expect(result.out).toContain(`Added 1.0.89 to compatibility.testedCliVersions in ${configPath}.`);
    const config = await readConfig(configPath);
    expect(config.compatibility.testedCliVersions).toEqual(["1.0.88", "1.0.89"]);
    expect(config.compatibility.untestedVersionPolicy).toBe("block-custom-routing");
    expect(config._comment).toBe("unknown keys must survive CHR writes");
    expect(config.models).toHaveLength(1);
    expect(config.providers.local.baseUrl).toBe("https://example.test/v1");
  });

  it("is a no-op when the version is already tested", async () => {
    const { copilot, configPath } = await setup();
    expect((await run(["verify", "--accept", "--copilot", copilot, "--timeout", "1000"])).code).toBe(0);
    const second = await run(["verify", "--accept", "--copilot", copilot, "--timeout", "1000"]);
    expect(second.code).toBe(0);
    expect(second.out).toContain("1.0.89 is already in compatibility.testedCliVersions");
    expect((await readConfig(configPath)).compatibility.testedCliVersions).toEqual(["1.0.88", "1.0.89"]);
  });

  it("leaves the config untouched when a probe fails", async () => {
    const { copilot, configPath } = await setup();
    restores.push(withEnv("FAKE_COPILOT_MODE", "wrong-model"));
    const before = await readFile(configPath, "utf8");
    const result = await run(["verify", "--accept", "--copilot", copilot, "--timeout", "1000"]);
    expect(result.code).toBe(1);
    expect(result.out).not.toContain("Added 1.0.89");
    expect(await readFile(configPath, "utf8")).toBe(before);
  });

  it("reports a missing config and exits non-zero", async () => {
    const copilot = await executable();
    const configPath = path.join(path.dirname(copilot), "absent-config.json");
    restores.push(withEnv("CHR_CONFIG_PATH", configPath));
    const result = await run(["verify", "--accept", "--copilot", copilot, "--timeout", "1000"]);
    expect(result.code).toBe(1);
    expect(result.out).toContain(`Configuration is missing at ${configPath}. Run "chr config init".`);
  });

  it("reports acceptance in the JSON output", async () => {
    const { copilot } = await setup();
    const accepted = await run(["verify", "--accept", "--json", "--copilot", copilot, "--timeout", "1000"]);
    expect(accepted.code).toBe(0);
    expect((JSON.parse(accepted.out) as { accepted: boolean }).accepted).toBe(true);
    const plain = await run(["verify", "--json", "--copilot", copilot, "--timeout", "1000"]);
    expect((JSON.parse(plain.out) as { accepted: boolean }).accepted).toBe(false);
  });

  it("points at --accept in the success hint when the flag is absent", async () => {
    const { copilot } = await setup();
    const result = await run(["verify", "--copilot", copilot, "--timeout", "1000"]);
    expect(result.code).toBe(0);
    expect(result.out).toContain('Run "chr verify --accept" to add 1.0.89 to testedCliVersions');
  });
});
