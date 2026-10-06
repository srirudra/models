import { spawn, execFile } from "node:child_process";
import { randomBytes } from "node:crypto";
import path from "node:path";
import { promisify } from "node:util";
import { createEphemeralCa, removeCaFile, writeCaFile } from "../intercept/ca.js";
import { startInterceptProxy } from "../intercept/proxy.js";
import type { InterceptProxy } from "../intercept/proxy.js";
import type { ConfigRevision } from "../config/types.js";
import { createInterceptHandler, eligibleModels } from "../intercept/router-handler.js";
import type { InterceptHandler } from "../intercept/router-handler.js";
import type { Forward, Upstream } from "../intercept/router-handler.js";
import { primeCredentials } from "../config/config.js";
import { watchConfig } from "../config/watch.js";
import type { ConfigWatcher } from "../config/watch.js";
import { defaultConfigPath } from "./config-file.js";
import { createRecorder } from "../recording/recorder.js";

const execFileAsync = promisify(execFile);

type Child = {
  once(event: string, listener: (...args: any[]) => void): unknown;
};

export type InterceptLaunchOptions = {
  revision: ConfigRevision;
  args?: string[];
  childCommand?: string;
  env?: NodeJS.ProcessEnv;
  spawnChild?: (command: string, args: string[], options: {
    env: NodeJS.ProcessEnv;
    stdio: "inherit";
    windowsHide: false;
  }) => Child;
  startProxy?: (options: {
    ca: ReturnType<typeof createEphemeralCa>;
    handleRequest: InterceptHandler;
    proxyAuthToken?: string;
  }) => Promise<InterceptProxy>;
  writeCa?: (pem: string) => Promise<string>;
  removeCa?: (path: string) => Promise<void>;
  /** Test seam; production writes the banner to process.stderr. */
  writeStderr?: (message: string) => void;
  /** AT-16 test seam; production probes "<command> --version". */
  detectVersion?: (command: string) => Promise<string | undefined>;
  /** AT-14 hot reload: configuration file to watch (defaults to the active config). */
  configPath?: string;
  /** AT-14 hot reload: set false to skip watching (tests, one-shot runs). */
  watch?: boolean;
  /** AT-14 test seam. */
  watchConfigFile?: typeof watchConfig;
  /** Test seam; lets tests drive the handler without real network access. */
  handlerOverrides?: { upstream?: Upstream; forward?: Forward };
};

function waitForChild(child: Child): Promise<number> {
  return new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", (code) => resolve(typeof code === "number" ? code : 1));
  });
}

/**
 * Best-effort Copilot CLI version probe.  Mirrors doctor's probe (which is not
 * exported) and resolves undefined instead of throwing: an undetectable
 * version is treated as untested, never as a reason to block the session.
 */
export async function detectCopilotVersion(command: string): Promise<string | undefined> {
  const extension = path.extname(command).toLowerCase();
  const isNodeScript = [".js", ".mjs", ".cjs"].includes(extension);
  // Windows shims ("copilot" or copilot.cmd from PATH) need a shell.
  const useShell = !isNodeScript && process.platform === "win32" && extension !== ".exe";
  try {
    const file = isNodeScript
      ? process.execPath
      : useShell && /\s/.test(command) ? `"${command}"` : command;
    const result = await execFileAsync(
      file,
      isNodeScript ? [command, "--version"] : ["--version"],
      { windowsHide: true, timeout: 15_000, shell: useShell },
    );
    return `${result.stdout}\n${result.stderr}`.match(/(\d+\.\d+\.\d+)/)?.[1];
  } catch {
    return undefined;
  }
}

function describeModels(models: ConfigRevision["models"]): string {
  return `${models.length} custom model${models.length === 1 ? "" : "s"} (${models.map((model) => model.alias).join(", ") || "none"})`;
}

export async function launchIntercept(options: InterceptLaunchOptions): Promise<number> {
  const ca = createEphemeralCa();
  const revision = Array.isArray(options.revision.models)
    ? options.revision
    : { ...options.revision, models: [], providers: {} } as ConfigRevision;
  await primeCredentials(revision, options.env ?? process.env);
  const write = options.writeStderr ?? ((message: string) => process.stderr.write(message));
  const command = options.childCommand ?? "copilot";
  const environment = options.env ?? process.env;
  const configPath = options.configPath ?? environment.CHR_CONFIG_PATH ?? defaultConfigPath(environment);

  // AT-16: gate custom routing on a tested Copilot CLI version.  The Copilot
  // session always launches; only custom model injection is withheld.
  const tested = options.revision.compatibility?.testedCliVersions;
  let passthroughOnly = false;
  if (Array.isArray(tested) && tested.length > 0) {
    const version = await (options.detectVersion ?? detectCopilotVersion)(command);
    if (!version || !tested.includes(version)) {
      const shown = version ?? "unknown";
      if (revision.compatibility.untestedVersionPolicy === "block-custom-routing") {
        passthroughOnly = true;
        write(`CHR intercept: Copilot CLI ${shown} is untested; custom models DISABLED (GitHub models only). Run "chr verify", then add "${shown}" to compatibility.testedCliVersions in ${configPath} to enable.\n`);
      } else {
        write(`CHR intercept: warning - Copilot CLI ${shown} is untested; custom models stay enabled. Run "chr verify", then add "${shown}" to compatibility.testedCliVersions in ${configPath}.\n`);
      }
    }
  }

  const supportedModels = passthroughOnly ? [] : eligibleModels(revision);
  const recorder = createRecorder(revision.recording ?? {
    defaultMode: "off", retentionDays: 0, maxTotalMiB: 0, maxBodyMiBPerRequest: 0, onWriteFailure: "ignore",
  }, {
    logger: (message) => process.stderr.write(`CHR recording: ${message}\n`),
  });
  const handleRequest = createInterceptHandler(revision, {
    logger: (message) => process.stderr.write(`CHR intercept: ${message}\n`),
    recorder,
    passthroughOnly,
    ...options.handlerOverrides,
  });
  const proxyAuthToken = randomBytes(24).toString("base64url");
  const proxy = await (options.startProxy ?? ((proxyOptions) => startInterceptProxy(proxyOptions)) )({
    ca,
    handleRequest,
    proxyAuthToken,
  });
  const caFile = await (options.writeCa ?? writeCaFile)(ca.caPem);
  const inherited = { ...environment };
  for (const key of Object.keys(inherited)) {
    if (key.startsWith("COPILOT_PROVIDER_") || key === "COPILOT_MODEL") delete inherited[key];
  }
  const proxyUrl = `http://chr:${proxyAuthToken}@127.0.0.1:${proxy.port}`;
  const env: NodeJS.ProcessEnv = {
    ...inherited,
    HTTPS_PROXY: proxyUrl,
    HTTP_PROXY: proxyUrl,
    NODE_EXTRA_CA_CERTS: caFile,
  };
  write(
    `CHR intercept: proxying copilot via 127.0.0.1:${proxy.port}; injected ${describeModels(supportedModels)}. GitHub models pass through; recording: ${recorder.currentMode()}\n`,
  );

  // AT-14: swap validated revisions in without restarting the session.
  let watcher: ConfigWatcher | undefined;
  if (options.watch !== false) {
    try {
      watcher = (options.watchConfigFile ?? watchConfig)(
        configPath,
        (next) => {
          handleRequest.setRevision(next);
          write(`CHR intercept: configuration reloaded (revision ${next.revisionId}): ${describeModels(passthroughOnly ? [] : eligibleModels(next))}\n`);
        },
        (message) => {
          write(`CHR intercept: configuration reload rejected, keeping revision ${handleRequest.currentRevision().revisionId}: ${message}\n`);
        },
        { env: environment },
      );
    } catch {
      // Watching is a convenience; never block the session on it.
    }
  }

  const spawnChild = options.spawnChild ?? ((file, args, spawnOptions) => spawn(file, args, spawnOptions));
  try {
    return await waitForChild(spawnChild(command, options.args ?? [], {
      env,
      stdio: "inherit",
      windowsHide: false,
    }));
  } finally {
    watcher?.close();
    await proxy.close();
    await (options.removeCa ?? removeCaFile)(caFile);
  }
}
