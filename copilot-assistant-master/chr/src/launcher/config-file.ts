import { mkdir, readFile, rename, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { loadConfig } from "../config/config.js";
import type { ConfigRevision } from "../config/types.js";
import type { ValidationError } from "../config/types.js";

export function defaultConfigPath(env: NodeJS.ProcessEnv = process.env): string {
  const localAppData = env.LOCALAPPDATA ?? path.join(os.homedir(), "AppData", "Local");
  return path.join(localAppData, "CHR", "config.json");
}

export const starterConfig = `{
  "_comment": "Replace the credentialRef placeholder with your own environment variable. Do not put secrets in this file.",
  "schemaVersion": 1,
  "compatibility": {
    "testedCliVersions": ["1.0.88", "1.0.89", "1.0.90"],
    "untestedVersionPolicy": "block-custom-routing"
  },
  "providers": {
    "local": {
      "protocol": "openai-chat-completions",
      "baseUrl": "https://example.test/v1",
      "credentialRef": "env:CHR_CUSTOM_API_KEY",
      "timeoutsMs": {
        "connect": 10000,
        "firstByte": 120000,
        "streamIdle": 60000,
        "total": 900000
      }
    }
  },
  "models": [],
  "routing": {
    "unmatchedGitHubModel": "preserve-original-route",
    "unknownCustomModel": "error",
    "crossProviderFallback": "disabled",
    "auxiliary": { "policy": "block" },
    "githubLeg": { "mode": "fail-closed" }
  },
  "recording": {
    "defaultMode": "off",
    "retentionDays": 7,
    "maxTotalMiB": 1024,
    "maxBodyMiBPerRequest": 16,
    "onWriteFailure": "continue-inference-and-alert"
  }
}
`;

export function starterTestedCliVersions(): string[] {
  const parsed = JSON.parse(starterConfig) as {
    compatibility?: { testedCliVersions?: unknown };
  };
  return Array.isArray(parsed.compatibility?.testedCliVersions)
    ? parsed.compatibility.testedCliVersions.filter((version): version is string => typeof version === "string")
    : [];
}

export type ConfigFileResult = {
  path: string;
  revision?: ConfigRevision;
  errors: ValidationError[];
  missing: boolean;
  parseError?: string;
};

export async function readConfigFile(
  filePath = process.env.CHR_CONFIG_PATH ?? defaultConfigPath(),
): Promise<ConfigFileResult> {
  let text: string;
  try {
    text = await readFile(filePath, "utf8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return { path: filePath, errors: [], missing: true };
    }
    return {
      path: filePath,
      errors: [{ path: "", code: "read", message: "configuration could not be read" }],
      missing: false,
    };
  }
  let input: unknown;
  try {
    input = JSON.parse(text);
  } catch {
    return {
      path: filePath,
      errors: [{ path: "", code: "json", message: "configuration contains invalid JSON" }],
      missing: false,
      parseError: "invalid JSON",
    };
  }
  try {
    const result = loadConfig(input);
    return { path: filePath, ...result, missing: false };
  } catch {
    return {
      path: filePath,
      errors: [{
        path: "",
        code: "schema",
        message: "configuration has an invalid structure",
      }],
      missing: false,
    };
  }
}

export async function loadConfigFile(
  filePath = process.env.CHR_CONFIG_PATH ?? defaultConfigPath(),
): Promise<ConfigRevision> {
  const result = await readConfigFile(filePath);
  if (result.missing) {
    throw new Error(`CHR configuration was not found at ${filePath}. Run "chr config init".`);
  }
  if (!result.revision) {
    throw new Error(
      `invalid CHR configuration at ${filePath}:\n${result.errors.map((error) => `${error.path || "(root)"} [${error.code}]: ${error.message}`).join("\n")}`,
    );
  }
  return result.revision;
}

/**
 * Writes configuration text atomically: the content lands in `<file>.tmp` and
 * is renamed over the target so a crash can never leave a half-written config.
 */
export async function writeConfigFileAtomic(filePath: string, text: string): Promise<void> {
  const temporary = `${filePath}.tmp`;
  await mkdir(path.dirname(filePath), { recursive: true });
  try {
    await writeFile(temporary, text, { encoding: "utf8", mode: 0o600 });
    await rename(temporary, filePath);
  } catch (error) {
    await rm(temporary, { force: true }).catch(() => undefined);
    throw error;
  }
}

export async function initConfigFile(
  filePath = process.env.CHR_CONFIG_PATH ?? defaultConfigPath(),
  force = false,
): Promise<void> {
  try {
    if (!force) {
      await readFile(filePath);
      throw new Error(`configuration already exists at ${filePath}; use --force to overwrite it`);
    }
  } catch (error) {
    if (error instanceof Error && !("code" in error)) throw error;
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") {
      throw new Error(`configuration could not be checked at ${filePath}`);
    }
  }
  await mkdir(path.dirname(filePath), { recursive: true });
  await writeFile(filePath, starterConfig, { encoding: "utf8" });
}
