import { EventEmitter } from "node:events";
import { afterEach, describe, expect, it, vi } from "vitest";
import { loadConfig, validateConfig } from "../src/config/config.js";
import { launchCopilot } from "../src/launcher/launch.js";

const base = (models: unknown[] = [{
  alias: "qwen",
  displayName: "Qwen",
  provider: "runpod",
  upstreamModel: "Qwen/Qwen3.8-27B-FP8",
  capabilities: { streaming: true, tools: true, vision: false, contextWindowTokens: 131072 },
}], providerOverrides: Record<string, unknown> = {}) => ({
  schemaVersion: 1,
  compatibility: { testedCliVersions: ["1.0.88"], untestedVersionPolicy: "allow" },
  providers: {
    runpod: {
      protocol: "openai-chat-completions",
      baseUrl: "http://127.0.0.1:1/v1",
      credentialRef: "env:QWEN_KEY",
      timeoutsMs: { connect: 100, firstByte: 100, streamIdle: 100, total: 1000 },
      ...providerOverrides,
    },
  },
  models,
  routing: {
    unmatchedGitHubModel: "preserve-original-route", unknownCustomModel: "error",
    crossProviderFallback: "disabled", auxiliary: { policy: "block" }, githubLeg: { mode: "fail-closed" },
  },
  recording: { defaultMode: "off", retentionDays: 7, maxTotalMiB: 1, maxBodyMiBPerRequest: 1, onWriteFailure: "continue" },
});

function revision(models?: unknown[], provider?: Record<string, unknown>) {
  return loadConfig(base(models, provider)).revision!;
}

function defaultModel(): any {
  return (base().models as any)[0];
}

function child() {
  const value = new EventEmitter();
  queueMicrotask(() => value.emit("exit", 0));
  return value;
}

async function capturedEnv(r: ReturnType<typeof revision>, modelAlias?: string, env = { QWEN_KEY: "secret" }) {
  let captured: NodeJS.ProcessEnv | undefined;
  await launchCopilot({
    revision: r, modelAlias, env,
    spawnChild: (_command, _args, options) => { captured = options.env; return child(); },
  });
  return captured!;
}

describe("model selection and child environment", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("does not print the effective configuration to stdout while launching", async () => {
    const write = vi.spyOn(process.stdout, "write");
    await launchCopilot({ revision: revision(), env: { QWEN_KEY: "secret" }, spawnChild: () => child() });
    expect(write.mock.calls.flat().join("")).not.toContain("revisionId");
  });

  it("accepts a valid model alias and rejects an invalid alias without spawning", async () => {
    let spawned = false;
    await expect(launchCopilot({
      revision: revision(), modelAlias: "missing", env: { QWEN_KEY: "secret" },
      spawnChild: () => { spawned = true; return child(); },
    })).rejects.toThrow(/missing.*qwen/);
    expect(spawned).toBe(false);
  });

  it("auto-selects the only configured model", async () => {
    expect((await capturedEnv(revision())).COPILOT_MODEL).toBe("qwen");
  });

  it("passes the selected model as the first Copilot argument", async () => {
    let args: string[] = [];
    await launchCopilot({
      revision: revision(), env: { QWEN_KEY: "secret" }, args: ["--help"],
      spawnChild: (_command, childArgs) => { args = childArgs; return child(); },
    });
    expect(args).toEqual(["--model", "qwen", "--help"]);
  });

  it.each([["--model", "qwen"], ["--model=qwen"]])(
    "does not duplicate a matching passthrough %s argument",
    async (...args: string[]) => {
      let childArgs: string[] = [];
      await launchCopilot({
        revision: revision(), env: { QWEN_KEY: "secret" }, args,
        spawnChild: (_command, value) => { childArgs = value; return child(); },
      });
      expect(childArgs).toEqual(args);
    },
  );

  it.each([["--model", "other"], ["--model=other"]])(
    "rejects a conflicting passthrough %s argument before spawning",
    async (...args: string[]) => {
      let spawned = false;
      await expect(launchCopilot({
        revision: revision(), env: { QWEN_KEY: "secret" }, args,
        spawnChild: () => { spawned = true; return child(); },
      })).rejects.toThrow(/use "chr launch --model qwen" instead/);
      expect(spawned).toBe(false);
    },
  );

  it("prints a secret-free startup banner", async () => {
    const write = vi.spyOn(process.stderr, "write");
    await launchCopilot({
      revision: revision(), env: { QWEN_KEY: "secret" },
      tokenFactory: () => "session-secret",
      spawnChild: () => child(),
    });
    const banner = write.mock.calls.flat().join("");
    expect(banner).toContain("model qwen");
    expect(banner).toContain("runpod-qwen");
    expect(banner).toContain("Qwen/Qwen3.8-27B-FP8");
    expect(banner).toMatch(/http:\/\/127\.0\.0\.1:\d+\/v1/);
    expect(banner).not.toContain("secret");
    expect(banner).not.toContain("session-secret");
  });

  it("requires an explicit model when several models are configured", async () => {
    const models = [defaultModel(), { ...defaultModel(), alias: "other" }];
    let spawned = false;
    await expect(launchCopilot({
      revision: revision(models), env: { QWEN_KEY: "secret" },
      spawnChild: () => { spawned = true; return child(); },
    })).rejects.toThrow(/--model.*qwen, other/);
    expect(spawned).toBe(false);
  });

  it("rejects configurations with zero models before spawning", async () => {
    await expect(launchCopilot({
      revision: revision([]), spawnChild: () => child(),
    })).rejects.toThrow(/aliases: none/);
  });

  it.each([
    ["openai-chat-completions", "completions"],
    ["openai-responses", "responses"],
  ] as const)("maps %s to the %s wire API", async (protocol, wireApi) => {
    const env = await capturedEnv(revision(undefined, { protocol }));
    expect(env).toMatchObject({
      COPILOT_MODEL: "qwen",
      COPILOT_PROVIDER_TYPE: "openai",
      COPILOT_PROVIDER_WIRE_API: wireApi,
    });
  });

  it("sets prompt and output token limits, including the default output limit", async () => {
    const explicit = await capturedEnv(revision());
    expect(explicit).toMatchObject({
      COPILOT_PROVIDER_MAX_PROMPT_TOKENS: "114688",
      COPILOT_PROVIDER_MAX_OUTPUT_TOKENS: "16384",
    });
    const custom = await capturedEnv(revision([{
      ...defaultModel(),
      capabilities: { ...defaultModel().capabilities, maxOutputTokens: 1000 },
    }]));
    expect(custom.COPILOT_PROVIDER_MAX_PROMPT_TOKENS).toBe("130072");
    expect(custom.COPILOT_PROVIDER_MAX_OUTPUT_TOKENS).toBe("1000");
  });

  it("does not set token limits when the context window is null", async () => {
    const env = await capturedEnv(revision([{
      ...defaultModel(),
      capabilities: { ...defaultModel().capabilities, contextWindowTokens: null },
    }]));
    expect(env.COPILOT_PROVIDER_MAX_PROMPT_TOKENS).toBeUndefined();
    expect(env.COPILOT_PROVIDER_MAX_OUTPUT_TOKENS).toBeUndefined();
  });

  it("strips inherited provider settings while preserving unrelated variables", async () => {
    const env = await capturedEnv(revision(), undefined, {
      QWEN_KEY: "secret", PATH: "preserved", COPILOT_MODEL: "user-model",
      COPILOT_PROVIDER_API_KEY: "user-key", COPILOT_PROVIDER_BEARER_TOKEN: "user-token",
      COPILOT_PROVIDER_API_KEY_COMMAND: "user-command", COPILOT_PROVIDER_WIRE_MODEL: "user-wire",
      COPILOT_PROVIDER_HEADERS: "user-headers",
    });
    expect(env.PATH).toBe("preserved");
    expect(env.COPILOT_PROVIDER_BASE_URL).toMatch(/^http:\/\/127\.0\.0\.1:\d+\/v1$/);
    expect(env.COPILOT_PROVIDER_HEADERS).toMatch(/^X-CHR-Session:/);
    expect(env.COPILOT_MODEL).toBe("qwen");
    expect(env.COPILOT_PROVIDER_API_KEY).toBeUndefined();
    expect(env.COPILOT_PROVIDER_BEARER_TOKEN).toBeUndefined();
    expect(env.COPILOT_PROVIDER_API_KEY_COMMAND).toBeUndefined();
    expect(env.COPILOT_PROVIDER_WIRE_MODEL).toBeUndefined();
  });

  it("fails before spawn when the selected credential variable is missing", async () => {
    let spawned = false;
    await expect(launchCopilot({
      revision: revision(), env: {},
      spawnChild: () => { spawned = true; return child(); },
    })).rejects.toThrow(/QWEN_KEY/);
    expect(spawned).toBe(false);
  });
});

describe("capability validation", () => {
  it.each([0, -1, 131072, 200000])("rejects maxOutputTokens=%s for a 131072 context", (maxOutputTokens) => {
    const errors = validateConfig(base([{
      ...defaultModel(),
      capabilities: { ...defaultModel().capabilities, maxOutputTokens },
    }]));
    expect(errors.some((error) => error.path.endsWith("maxOutputTokens"))).toBe(true);
  });
});
