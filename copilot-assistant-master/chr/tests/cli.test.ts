import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { runCli } from "../src/cli/main.js";
import { readConfigFile } from "../src/launcher/config-file.js";
import { runDoctor } from "../src/launcher/doctor.js";

const tempDirectories: string[] = [];

afterEach(async () => {
  while (tempDirectories.length > 0) {
    await rm(tempDirectories.pop()!, { recursive: true, force: true });
  }
});

async function tempConfig(contents?: string): Promise<string> {
  const directory = await mkdtemp(path.join(os.tmpdir(), "chr-cli-"));
  tempDirectories.push(directory);
  const filePath = path.join(directory, "config.json");
  if (contents !== undefined) await writeFile(filePath, contents);
  return filePath;
}

describe("CHR CLI configuration handling", () => {
  it("doctor reports a missing config while continuing environment checks", async () => {
    const filePath = await tempConfig();
    const config = await readConfigFile(filePath);
    const report = await runDoctor({
      revision: config.revision,
      configPath: config.path,
      configMissing: config.missing,
      configErrors: config.errors,
      env: { PATH: "", HTTPS_PROXY: "http://proxy.test" },
      locateExecutable: async () => "copilot",
      versionProbe: async () => "1.0.88",
    });
    expect(report.output).toContain(filePath);
    expect(report.output).toContain("Version: 1.0.88");
    expect(report.output).toContain("Loopback:");
    expect(report.output).toContain("proxy");
    expect(report.output).not.toContain("ENOENT");
  });

  it("doctor reports malformed JSON as a structured finding", async () => {
    const filePath = await tempConfig("{ malformed");
    const config = await readConfigFile(filePath);
    const report = await runDoctor({
      revision: config.revision,
      configPath: config.path,
      configMissing: config.missing,
      configErrors: config.errors,
      env: { PATH: "" },
      locateExecutable: async () => "copilot",
      versionProbe: async () => "1.0.88",
    });
    expect(report.output).toContain("Configuration: invalid");
    expect(report.output).toContain("[json]");
    expect(report.output).not.toContain("Unexpected token");
    expect(report.output).not.toContain("SyntaxError");
  });

  it("config init creates, protects, and force-overwrites the starter", async () => {
    const filePath = await tempConfig();
    const original = process.env.CHR_CONFIG_PATH;
    process.env.CHR_CONFIG_PATH = filePath;
    try {
      expect(await runCli(["config", "init"])).toBe(0);
      const first = await readFile(filePath, "utf8");
      expect(first).toContain("env:CHR_CUSTOM_API_KEY");
      expect(first).toContain('"testedCliVersions": ["1.0.88", "1.0.89", "1.0.90"]');
      expect(first).not.toContain("SECRET");
      expect(await runCli(["config", "init"])).toBe(1);
      expect(await runCli(["config", "init", "--force"])).toBe(0);
      expect(await readFile(filePath, "utf8")).toBe(first);
    } finally {
      if (original === undefined) delete process.env.CHR_CONFIG_PATH;
      else process.env.CHR_CONFIG_PATH = original;
    }
  });
});
