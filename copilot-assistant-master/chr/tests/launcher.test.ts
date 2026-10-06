import { EventEmitter } from "node:events";
import { describe, expect, it } from "vitest";
import { loadConfig } from "../src/config/config.js";
import { launchCopilot } from "../src/launcher/launch.js";

const revision = loadConfig({
  schemaVersion: 1,
  compatibility: { testedCliVersions: ["1.0.88", "1.0.89"], untestedVersionPolicy: "block-custom-routing" },
  providers: {
    local: {
      protocol: "openai-chat-completions",
      baseUrl: "http://127.0.0.1:1/v1",
      credentialRef: "env:CHR_TEST_KEY",
      timeoutsMs: { connect: 100, firstByte: 100, streamIdle: 100, total: 1000 },
    },
  },
  models: [{
    alias: "test",
    displayName: "Test",
    provider: "local",
    upstreamModel: "test-upstream",
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
    maxTotalMiB: 1,
    maxBodyMiBPerRequest: 1,
    onWriteFailure: "continue",
  },
}).revision!;

function fakeChild(code: number): EventEmitter {
  const child = new EventEmitter();
  queueMicrotask(() => child.emit("exit", code));
  return child;
}

describe("CHR launcher", () => {
  it("scopes the ephemeral endpoint and session token, and propagates exit", async () => {
    const original = process.env.COPILOT_PROVIDER_BASE_URL;
    const environments: NodeJS.ProcessEnv[] = [];
    let firstUrl = "";
    const run = async (code: number) => launchCopilot({
      revision,
      env: { ...process.env, CHR_TEST_KEY: "test-secret", COPILOT_PROVIDER_BASE_URL: "old-value" },
      tokenFactory: () => code === 7 ? "token-one" : "token-two",
      spawnChild: (_command, _args, options) => {
        environments.push(options.env);
        firstUrl = options.env.COPILOT_PROVIDER_BASE_URL ?? "";
        expect(firstUrl).toMatch(/^http:\/\/127\.0\.0\.1:\d+\/v1$/);
        expect(options.env.COPILOT_PROVIDER_HEADERS).toMatch(/^X-CHR-Session: token-/);
        return fakeChild(code);
      },
    });
    expect(await run(7)).toBe(7);
    expect(await run(0)).toBe(0);
    expect(environments[0].COPILOT_PROVIDER_HEADERS).not.toBe(environments[1].COPILOT_PROVIDER_HEADERS);
    expect(process.env.COPILOT_PROVIDER_BASE_URL).toBe(original);
    await expect(fetch(firstUrl)).rejects.toThrow();
  });

  it("does not spawn when an untested version is blocked", async () => {
    let spawned = false;
    await expect(launchCopilot({
      revision,
      version: "9.9.9",
      spawnChild: () => {
        spawned = true;
        return fakeChild(0);
      },
    })).rejects.toThrow(/directly with "copilot"/);
    expect(spawned).toBe(false);
  });

  it("supports a child command path containing spaces and Unicode", async () => {
    let command = "";
    const code = await launchCopilot({
      revision,
      childCommand: "C:\\Program Files\\模型\\fake copilot.cmd",
      env: { ...process.env, CHR_TEST_KEY: "test-secret" },
      spawnChild: (value) => {
        command = value;
        return fakeChild(0);
      },
    });
    expect(code).toBe(0);
    expect(command).toContain("模型");
  });
});
