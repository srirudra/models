import { watch as fsWatch } from "node:fs";
import { stat } from "node:fs/promises";
import path from "node:path";
import { readConfigFile } from "../launcher/config-file.js";
import { primeCredentials } from "./config.js";
import type { ConfigRevision } from "./types.js";

export type ConfigWatcher = { close(): void };

export type WatchConfigOptions = {
  /** Coalescing window for editor save bursts (default 250 ms). */
  debounceMs?: number;
  env?: NodeJS.ProcessEnv;
  /** Test seams. */
  read?: typeof readConfigFile;
  prime?: (revision: ConfigRevision, env: NodeJS.ProcessEnv) => Promise<void>;
};

function describeErrors(result: Awaited<ReturnType<typeof readConfigFile>>): string {
  if (result.missing) return "configuration file is missing";
  if (result.parseError) return `configuration contains invalid JSON`;
  return result.errors
    .map((error) => `${error.path || "(root)"} [${error.code}]: ${error.message}`)
    .join("; ") || "configuration is invalid";
}

/**
 * Watch a configuration file and publish validated, credential-primed
 * revisions.  The directory is watched (not the file) because editors and
 * `writeFile` replace files atomically, which detaches file watches.
 */
export function watchConfig(
  filePath: string,
  onRevision: (revision: ConfigRevision) => void,
  onError: (message: string) => void,
  options: WatchConfigOptions = {},
): ConfigWatcher {
  const debounceMs = options.debounceMs ?? 250;
  const env = options.env ?? process.env;
  const read = options.read ?? readConfigFile;
  const prime = options.prime ?? primeCredentials;
  const directory = path.dirname(path.resolve(filePath));
  const name = path.basename(filePath);

  let signature = "";
  let lastRevisionId = 0;
  let timer: NodeJS.Timeout | undefined;
  let running = false;
  let pending = false;
  let closed = false;

  void stat(filePath).then(
    (stats) => { signature = `${stats.mtimeMs}:${stats.size}`; },
    () => { signature = ""; },
  );

  const reload = async (): Promise<void> => {
    if (closed) return;
    if (running) { pending = true; return; }
    running = true;
    try {
      // Ignore spurious directory events (attribute touches, sibling writes).
      let next = "";
      try {
        const stats = await stat(filePath);
        next = `${stats.mtimeMs}:${stats.size}`;
      } catch {
        next = "";
      }
      if (next !== "" && next === signature) return;
      signature = next;
      const result = await read(filePath);
      if (!result.revision) {
        onError(describeErrors(result));
        return;
      }
      let revision = result.revision;
      try {
        await prime(revision, env);
      } catch (error) {
        onError(error instanceof Error ? error.message : "credentials could not be resolved");
        return;
      }
      // Revision ids must advance for every accepted reload.
      if (revision.revisionId <= lastRevisionId) {
        revision = Object.freeze({ ...revision, revisionId: lastRevisionId + 1 }) as ConfigRevision;
      }
      lastRevisionId = revision.revisionId;
      onRevision(revision);
    } catch (error) {
      onError(error instanceof Error ? error.message : "configuration reload failed");
    } finally {
      running = false;
      if (pending && !closed) {
        pending = false;
        setTimeout(() => { void reload(); }, debounceMs).unref?.();
      }
    }
  };

  const schedule = (): void => {
    if (closed) return;
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => {
      timer = undefined;
      void reload();
    }, debounceMs);
    timer.unref?.();
  };

  const watcher = fsWatch(directory, { persistent: false }, (_event, changed) => {
    try {
      if (changed && path.basename(changed.toString()) !== name) return;
      schedule();
    } catch {
      // A watcher callback must never throw into Node's event loop.
    }
  });
  watcher.on("error", (error) => {
    onError(`configuration watcher failed: ${error instanceof Error ? error.message : "unknown error"}`);
  });

  return {
    close(): void {
      closed = true;
      if (timer) clearTimeout(timer);
      timer = undefined;
      watcher.close();
    },
  };
}
