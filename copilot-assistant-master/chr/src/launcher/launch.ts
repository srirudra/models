import { randomBytes } from "node:crypto";
import { spawn } from "node:child_process";
import type http from "node:http";
import { createServer, listen } from "../server/server.js";
import type { ConfigRevision } from "../config/types.js";
import { primeCredentials } from "../config/config.js";

export type SpawnedChild = {
  once(event: string, listener: (...args: any[]) => void): unknown;
};

export type LaunchOptions = {
  revision: ConfigRevision;
  modelAlias?: string;
  args?: string[];
  childCommand?: string;
  env?: NodeJS.ProcessEnv;
  spawnChild?: (
    command: string,
    args: string[],
    options: { env: NodeJS.ProcessEnv; stdio: "inherit"; windowsHide: false },
  ) => SpawnedChild;
  tokenFactory?: () => string;
  version?: string;
  testedVersions?: string[];
  untestedVersionPolicy?: string;
};

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : "operation failed";
}

function closeServer(server: http.Server): Promise<void> {
  return new Promise((resolve, reject) => {
    server.close((error) => error ? reject(error) : resolve());
  });
}

function childExit(child: SpawnedChild): Promise<number> {
  return new Promise((resolve, reject) => {
    child.once("error", (error) => reject(error));
    child.once("exit", (code) => resolve(typeof code === "number" ? code : 1));
  });
}

function modelArgument(args: string[], selectedAlias: string): string[] {
  let hasMatchingModel = false;
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index];
    let value: string | undefined;
    if (argument === "--model") {
      value = args[index + 1];
      if (!value || value.startsWith("--")) {
        throw new Error(`Copilot argument "--model" conflicts with CHR; use "chr launch --model ${selectedAlias}" instead.`);
      }
      index += 1;
    } else if (argument.startsWith("--model=")) {
      value = argument.slice("--model=".length);
      if (!value) {
        throw new Error(`Copilot argument "--model=" conflicts with CHR; use "chr launch --model ${selectedAlias}" instead.`);
      }
    }
    if (value !== undefined && value !== selectedAlias) {
      throw new Error(
        `Copilot argument selects model "${value}", but CHR selected "${selectedAlias}"; use "chr launch --model ${selectedAlias}" instead.`,
      );
    }
    if (value !== undefined) hasMatchingModel = true;
  }
  return hasMatchingModel ? args : [`--model`, selectedAlias, ...args];
}

export async function launchCopilot(options: LaunchOptions): Promise<number> {
  const aliases = options.revision.models.map((model) => model.alias);
  const selectedAlias = options.modelAlias ??
    (aliases.length === 1 ? aliases[0] : undefined);
  if (!selectedAlias) {
    throw new Error(`select a model with --model; configured aliases: ${aliases.join(", ") || "none"}`);
  }
  const selected = options.revision.models.find((model) => model.alias === selectedAlias);
  if (!selected) {
    throw new Error(`unknown model alias "${selectedAlias}"; configured aliases: ${aliases.join(", ") || "none"}`);
  }
  const childArgs = modelArgument(options.args ?? [], selected.alias);
  const tested = options.testedVersions ?? options.revision.compatibility.testedCliVersions;
  if (
    options.version &&
    !tested.includes(options.version) &&
    (options.untestedVersionPolicy ?? options.revision.compatibility.untestedVersionPolicy) ===
      "block-custom-routing"
  ) {
    throw new Error(
      `Copilot CLI ${options.version} is untested. Custom routing is blocked; launch Copilot directly with "copilot".`,
    );
  }

  const provider = options.revision.providers[selected.provider];
  await primeCredentials(options.revision, options.env ?? process.env);
  const server = createServer(options.revision);
  let port: number;
  try {
    port = await listen(server, 0);
  } catch (error) {
    server.close();
    throw new Error(`CHR proxy unavailable: failed to bind loopback listener (${errorMessage(error)}).`);
  }

  const token = (options.tokenFactory ?? (() => randomBytes(32).toString("hex")))();
  const inherited = { ...(options.env ?? process.env) };
  for (const key of Object.keys(inherited)) {
    if (key.startsWith("COPILOT_PROVIDER_") || key === "COPILOT_MODEL") delete inherited[key];
  }
  const env: NodeJS.ProcessEnv = {
    ...inherited,
    COPILOT_PROVIDER_BASE_URL: `http://127.0.0.1:${port}/v1`,
    COPILOT_PROVIDER_HEADERS: `X-CHR-Session: ${token}`,
  };
  env.COPILOT_MODEL = selected.alias;
  env.COPILOT_PROVIDER_TYPE = "openai";
  env.COPILOT_PROVIDER_WIRE_API = provider.protocol === "openai-responses" ? "responses" : "completions";
  if (selected.capabilities.contextWindowTokens !== null) {
    env.COPILOT_PROVIDER_MAX_PROMPT_TOKENS = String(
      selected.capabilities.contextWindowTokens - (selected.capabilities.maxOutputTokens ?? 16384),
    );
    env.COPILOT_PROVIDER_MAX_OUTPUT_TOKENS = String(selected.capabilities.maxOutputTokens ?? 16384);
  }
  process.stderr.write(
    `CHR: model ${selected.alias} -> ${selected.provider}-${selected.alias} (${selected.upstreamModel}) via ${env.COPILOT_PROVIDER_BASE_URL}\n`,
  );
  const command = options.childCommand ?? "copilot";
  const spawnChild = options.spawnChild ?? ((file, args, spawnOptions) => spawn(file, args, spawnOptions));
  let code: number;
  try {
    const child = spawnChild(command, childArgs, {
      env,
      stdio: "inherit",
      windowsHide: false,
    });
    code = await childExit(child);
  } catch (error) {
    throw new Error(`failed to launch Copilot through CHR (${errorMessage(error)}).`);
  } finally {
    await closeServer(server);
  }
  return code;
}
