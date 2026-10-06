import { access } from "node:fs/promises";
import { constants } from "node:fs";
import { execFile } from "node:child_process";
import net from "node:net";
import { promisify } from "node:util";
import path from "node:path";
import { redactEffective } from "../config/config.js";
import { getSecret } from "../secrets/dpapi.js";
import type { ConfigRevision } from "../config/types.js";
import type { ValidationError } from "../config/types.js";

const execFileAsync = promisify(execFile);
const providerEnvPattern = /^COPILOT_PROVIDER_/;

export type DoctorOptions = {
  revision?: ConfigRevision;
  configPath?: string;
  configMissing?: boolean;
  configErrors?: ValidationError[];
  env?: NodeJS.ProcessEnv;
  executable?: string;
  locateExecutable?: (env: NodeJS.ProcessEnv) => Promise<string | undefined>;
  versionProbe?: (executable: string) => Promise<string>;
};

export type DoctorReport = {
  executable?: string;
  version?: string;
  versionDrift: boolean;
  blocking: string[];
  warnings: string[];
  effectiveConfig?: unknown;
  loopback: boolean;
  output: string;
  exitCode: number;
};

async function locateCopilot(env: NodeJS.ProcessEnv): Promise<string | undefined> {
  const pathValue = env.Path ?? env.PATH ?? "";
  const extensions = (env.PATHEXT ?? ".EXE;.CMD;.BAT").split(";");
  for (const directory of pathValue.split(";").filter(Boolean)) {
    for (const extension of ["", ...extensions]) {
      const candidate = `${directory}\\copilot${extension}`;
      try {
        await access(candidate, constants.X_OK);
        return candidate;
      } catch {
        // Continue searching PATH.
      }
    }
  }
  return undefined;
}

async function probeVersion(executable: string): Promise<string> {
  const isNodeScript = [".js", ".mjs", ".cjs"].includes(path.extname(executable).toLowerCase());
  const result = await execFileAsync(
    isNodeScript ? process.execPath : executable,
    isNodeScript ? [executable, "--version"] : ["--version"],
    { windowsHide: true },
  );
  const match = `${result.stdout}\n${result.stderr}`.match(/(\d+\.\d+\.\d+)/);
  if (!match) {
    throw new Error("could not parse Copilot CLI version");
  }
  return match[1];
}

async function probeLoopback(): Promise<boolean> {
  const server = net.createServer();
  try {
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject);
      server.listen(0, "127.0.0.1", () => resolve());
    });
    return true;
  } catch {
    return false;
  } finally {
    server.close();
  }
}

export async function runDoctor(options: DoctorOptions = {}): Promise<DoctorReport> {
  const env = options.env ?? process.env;
  const blocking: string[] = [];
  const warnings: string[] = [];
  const executable = options.executable ?? await (options.locateExecutable ?? locateCopilot)(env);
  if (!executable) {
    blocking.push("Copilot executable was not found on PATH.");
  }

  let version: string | undefined;
  if (executable) {
    try {
      version = await (options.versionProbe ?? probeVersion)(executable);
    } catch (error) {
      blocking.push(`Unable to read Copilot CLI version: ${error instanceof Error ? error.message : "probe failed"}`);
    }
  }
  const tested = options.revision?.compatibility.testedCliVersions ?? [];
  const policy = options.revision?.compatibility.untestedVersionPolicy;
  const versionDrift = Boolean(version && !tested.includes(version));
  if (versionDrift) {
    const message = `Copilot CLI ${version} is not tested (tested: ${tested.join(", ") || "none"}).`;
    if (policy === "block-custom-routing") {
      blocking.push(`${message} Custom routing is blocked; launch directly with "copilot".`);
    } else {
      warnings.push(message);
    }
  }

  const conflicts = Object.keys(env).filter((key) => providerEnvPattern.test(key));
  if (conflicts.length > 0) {
    warnings.push(`Existing Copilot provider settings will be overridden for the child: ${conflicts.join(", ")}.`);
  }
  const proxies = ["HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY"].filter((key) => env[key]);
  if (proxies.length > 0) {
    warnings.push(`Corporate proxy settings detected: ${proxies.join(", ")}.`);
  }
  const loopback = await probeLoopback();
  if (!loopback) {
    blocking.push("CHR could not bind an IPv4 loopback listener.");
  }
  let effectiveConfig: unknown;
  if (options.revision) {
    effectiveConfig = redactEffective(options.revision);
  }
  const configFinding = options.revision
    ? `Configuration: valid${options.configPath ? ` (${options.configPath})` : ""}`
    : options.configMissing
      ? `Configuration: missing at ${options.configPath ?? "the default path"}. Run "chr config init" to create it. (Required for launch; doctor can still inspect the environment.)`
      : `Configuration: invalid${options.configPath ? ` at ${options.configPath}` : ""}.\n${(options.configErrors ?? []).map((error) => `  ${error.path || "(root)"} [${error.code}]: ${error.message}`).join("\n")}`;
  const credentials = options.revision
    ? await Promise.all(Object.entries(options.revision.providers).map(async ([id, provider]) => {
      if (provider.credentialRef.startsWith("env:")) {
        const name = provider.credentialRef.slice(4);
        return `Credential ${id} (${name}): ${env[name] ? "set" : "NOT set"}`;
      }
      if (provider.credentialRef.startsWith("windows-credential:")) {
        const name = provider.credentialRef.slice("windows-credential:".length);
        let present = false;
        try {
          present = (await getSecret(name)) !== undefined;
        } catch {
          // DPAPI is unavailable off Windows; report the credential as absent.
        }
        return `Credential ${id} (${name}): ${present ? "set" : "NOT set"}`;
      }
      return `Credential ${id}: unsupported reference`;
    }))
    : [];
  const output = [
    `Copilot: ${executable ?? "not found"}`,
    `Version: ${version ?? "unknown"}`,
    `Loopback: ${loopback ? "available (CHR binds 127.0.0.1)" : "unavailable"}`,
    ...warnings.map((warning) => `Warning: ${warning}`),
    ...blocking.map((problem) => `Error: ${problem}`),
    configFinding,
    ...credentials,
    effectiveConfig ? `Effective configuration (redacted):\n${JSON.stringify(effectiveConfig, null, 2)}` : "",
  ].filter(Boolean).join("\n");
  return { executable, version, versionDrift, blocking, warnings, effectiveConfig, loopback, output, exitCode: blocking.length ? 1 : 0 };
}
