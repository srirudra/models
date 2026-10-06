import { describe, expect, it } from "vitest";
import { loadConfig } from "../src/config/config.js";
import { runDoctor } from "../src/launcher/doctor.js";

const revision = loadConfig({
  schemaVersion: 1,
  compatibility: { testedCliVersions: ["1.0.88", "1.0.89"], untestedVersionPolicy: "block-custom-routing" },
  providers: {
    local: {
      protocol: "openai-chat-completions",
      baseUrl: "https://provider.test/v1",
      credentialRef: "env:SECRET_CANARY",
      timeoutsMs: { connect: 1, firstByte: 1, streamIdle: 1, total: 1 },
    },
  },
  models: [],
  routing: {
    unmatchedGitHubModel: "preserve-original-route",
    unknownCustomModel: "error",
    crossProviderFallback: "disabled",
    auxiliary: { policy: "block" },
    githubLeg: { mode: "fail-closed" },
  },
  recording: {
    defaultMode: "off",
    retentionDays: 1,
    maxTotalMiB: 1,
    maxBodyMiBPerRequest: 1,
    onWriteFailure: "continue",
  },
}).revision!;

describe("CHR doctor", () => {
  it("reports credential presence without exposing its value", async () => {
    const report = await runDoctor({
      revision,
      env: { PATH: "", SECRET_CANARY: "super-secret" },
      locateExecutable: async () => "copilot",
      versionProbe: async () => "1.0.88",
    });
    expect(report.output).toContain("Credential local (SECRET_CANARY): set");
    expect(report.output).not.toContain("super-secret");

    const missing = await runDoctor({
      revision,
      env: { PATH: "" },
      locateExecutable: async () => "copilot",
      versionProbe: async () => "1.0.88",
    });
    expect(missing.output).toContain("Credential local (SECRET_CANARY): NOT set");
  });

  it("redacts credentials and reports drift/conflicts without prompting", async () => {
    const report = await runDoctor({
      revision,
      env: {
        PATH: "C:\\fake",
        COPILOT_PROVIDER_API_KEY: "SECRET_CANARY_VALUE",
        HTTPS_PROXY: "http://proxy.test",
      },
      locateExecutable: async () => "C:\\fake\\copilot.exe",
      versionProbe: async () => "9.9.9",
    });
    expect(report.exitCode).toBe(1);
    expect(report.output).not.toContain("SECRET_CANARY_VALUE");
    expect(report.output).toContain("Version");
    expect(report.output).toContain("proxy");
  });
});
