import { describe, expect, it } from "vitest";
import { loadConfig, validateConfig } from "../src/config/config.js";
import { classifyRequest } from "../src/routing/classifier.js";
import { resolveRoute } from "../src/routing/resolve.js";

const makeConfig = (overrides: Record<string, unknown> = {}) => ({
  schemaVersion: 1,
  compatibility: {
    testedCliVersions: ["1.0.88", "1.0.89"],
    untestedVersionPolicy: "block-custom-routing",
  },
  providers: {
    local: {
      protocol: "openai-chat-completions",
      baseUrl: "http://127.0.0.1:1234/v1",
      credentialRef: "env:KEY",
      timeoutsMs: {
        connect: 100,
        firstByte: 100,
        streamIdle: 100,
        total: 1000,
      },
    },
  },
  models: [
    {
      alias: "custom/coding",
      displayName: "Coding",
      provider: "local",
      upstreamModel: "coding",
      capabilities: {
        streaming: true,
        tools: true,
        vision: false,
        contextWindowTokens: null,
      },
    },
  ],
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
  ...overrides,
});

describe("configuration", () => {
  it("rejects malformed URLs, unknown providers, and capability mismatches", () => {
    const config = makeConfig({
      providers: {
        local: {
          ...makeConfig().providers.local,
          baseUrl: "not a URL",
        },
      },
      models: [{
        ...makeConfig().models[0],
        capabilities: { ...makeConfig().models[0].capabilities, streaming: false },
      }, {
        ...makeConfig().models[0],
        alias: "custom/missing",
        provider: "missing",
      }],
    });
    const errors = validateConfig(config);

    expect(errors.map((error) => error.code)).toEqual(
      expect.arrayContaining(["url", "reference", "capability"]),
    );
  });

  it("creates independent, incrementing revisions", () => {
    const first = loadConfig(makeConfig()).revision!;
    const second = loadConfig(makeConfig({
      models: [{ ...makeConfig().models[0], upstreamModel: "new-model" }],
    })).revision!;

    expect(second.revisionId).toBeGreaterThan(first.revisionId);
    expect(first.models[0].upstreamModel).toBe("coding");
  });
});

describe("routing and classification", () => {
  it("keeps a normal primary request primary", () => {
    expect(classifyRequest({
      model: "custom/coding",
      messages: [{ role: "user", content: "implement this feature" }],
      tools: [{ type: "function" }],
    }).kind).toBe("primary");
  });

  it.each(["route-to", "allow"] as const)("supports auxiliary policy %s", (policy) => {
    const revision = loadConfig(makeConfig({
      routing: {
        ...makeConfig().routing,
        auxiliary: policy === "route-to"
          ? { policy, provider: "local" }
          : { policy },
      },
    })).revision!;
    const route = resolveRoute(
      "unregistered/auxiliary",
      revision,
      { model: "unregistered/auxiliary", messages: [], max_tokens: 32 },
      "custom/coding",
    );

    expect(route).toMatchObject({ kind: "auxiliary", policy });
  });
});
