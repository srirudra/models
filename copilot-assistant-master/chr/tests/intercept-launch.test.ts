import { EventEmitter } from "node:events";
import { access, readFileSync } from "node:fs";
import { access, rm } from "node:fs/promises";
import { describe, expect, it } from "vitest";
import { launchIntercept } from "../src/launcher/intercept-launch.js";
import type { InterceptProxy } from "../src/intercept/proxy.js";

describe("intercept launcher", () => {
  it("sanitizes the child environment and cleans up after exit", async () => {
    const child = new EventEmitter();
    let childEnv: NodeJS.ProcessEnv | undefined;
    let caPath = "";
    let caContents = "";
    let closed = false;
    let banner = "";
    const fakeProxy: InterceptProxy = { port: 45678, close: async () => { closed = true; } };
    const launch = launchIntercept({
      revision: {} as never,
      args: ["--arg", "value"],
      env: {
        COPILOT_PROVIDER_API_KEY: "secret",
        COPILOT_PROVIDER_BASE_URL: "https://provider.invalid",
        COPILOT_PROVIDER_WIRE_API: "1",
        COPILOT_MODEL: "model",
        UNRELATED: "keep",
      },
      startProxy: async () => fakeProxy,
      writeCa: async (pem) => {
        const { mkdtemp, writeFile } = await import("node:fs/promises");
        const { tmpdir } = await import("node:os");
        const { join } = await import("node:path");
        const dir = await mkdtemp(join(tmpdir(), "chr-launch-test-"));
        caPath = join(dir, "ca.pem");
        await writeFile(caPath, pem);
        return caPath;
      },
      spawnChild: (_command, args, options) => {
        expect(args).toEqual(["--arg", "value"]);
        childEnv = options.env;
        caContents = readFileSync(options.env.NODE_EXTRA_CA_CERTS ?? "", "utf8");
        return child;
      },
      writeStderr: (message) => { banner += message; },
    });
    const exitTimer = setInterval(() => {
      if (childEnv) {
        clearInterval(exitTimer);
        child.emit("exit", 7);
      }
    }, 0);
    const code = await launch;
    expect(childEnv?.HTTPS_PROXY).toMatch(/^http:\/\/chr:[A-Za-z0-9_-]+@127\.0\.0\.1:45678$/);
    expect(childEnv?.HTTP_PROXY).toBe(childEnv?.HTTPS_PROXY);
    expect(banner).not.toContain(childEnv?.HTTPS_PROXY.split("@")[0].slice(11));
    expect(childEnv?.UNRELATED).toBe("keep");
    expect(childEnv).not.toHaveProperty("COPILOT_PROVIDER_API_KEY");
    expect(childEnv).not.toHaveProperty("COPILOT_PROVIDER_BASE_URL");
    expect(childEnv).not.toHaveProperty("COPILOT_PROVIDER_WIRE_API");
    expect(childEnv).not.toHaveProperty("COPILOT_MODEL");
    expect(childEnv?.NODE_EXTRA_CA_CERTS).toBe(caPath);
    expect(caContents).toContain("CERTIFICATE");
    expect(banner).toContain("127.0.0.1:45678");
    expect(code).toBe(7);
    expect(closed).toBe(true);
    await expect(access(caPath)).rejects.toMatchObject({ code: "ENOENT" });
    await rm(caPath.replace(/\\[^\\]+$/, ""), { recursive: true, force: true });
  });
});
