import { pathToFileURL } from "node:url";
import { defaultConfigPath, initConfigFile, loadConfigFile, readConfigFile } from "../launcher/config-file.js";
import { runDoctor } from "../launcher/doctor.js";
import { launchCopilot } from "../launcher/launch.js";
import { launchIntercept } from "../launcher/intercept-launch.js";
import { runVerify } from "../launcher/verify.js";
import { redactEffective } from "../config/config.js";
import { getSecret, listSecretNames, removeSecret, setSecret } from "../secrets/dpapi.js";
import { createInterface } from "node:readline/promises";
import { readRecordingMode, recordingDirectory, recordingStatePath, writeRecordingMode } from "../recording/recorder.js";
import {
  acceptTestedCliVersion, chrVersion, runModelCommand, runProviderCommand, runRecordSubcommand,
} from "./manage.js";

const usage = `Usage:
  chr doctor
  chr verify [--copilot <path>] [--json] [--timeout <ms>] [--accept]
  chr config validate
  chr config show
  chr config init [--force]
  chr secret set <name> [--from-env <ENVVAR>]
  chr secret list
  chr secret remove <name>
  chr provider list
  chr provider add <name> --base-url <url> (--key-env <VAR> | --key-secret <NAME>) [--protocol chat|responses] [--force]
  chr provider remove <name>
  chr provider test <name> [--model <upstreamId>] [--timeout <ms>]
  chr model list
  chr model add <alias> --provider <name> --upstream <id> [--display-name <s>] [--context <n>] [--max-output <n>] [--no-tools] [--vision] [--force]
  chr model remove <alias>
  chr launch [--model <alias>] [-- <copilot arguments>]
  chr intercept [-- <copilot args>]
  chr record on|off|status [--full]
  chr record list [--limit <n>]
  chr record show <id>
  chr record clear [--older-than <days>]
  chr record export <outFile> [--since <ISO>]
  chr version`;

function print(value: string): void {
  process.stdout.write(`${value}\n`);
}

const io = { print, usage: () => print(usage) };

export async function runCli(argv: string[] = process.argv.slice(2)): Promise<number> {
  const command = argv[0];
  try {
    if (command === "record") {
      const action = argv[1];
      if ((action === "on" && (argv.length === 2 || (argv.length === 3 && argv[2] === "--full"))) ||
          (action === "off" && argv.length === 2)) {
        await writeRecordingMode(action === "off" ? "off" : argv[2] === "--full" ? "full" : "metadata");
        return 0;
      }
      if (action === "status" && argv.length === 2) {
        const config = await readConfigFile();
        const mode = readRecordingMode(config.revision?.recording ?? {
          defaultMode: "off", retentionDays: 0, maxTotalMiB: 0, maxBodyMiBPerRequest: 0, onWriteFailure: "ignore",
        });
        let source = "config default";
        try {
          const state = JSON.parse(await (await import("node:fs/promises")).readFile(recordingStatePath(), "utf8")) as { mode?: unknown };
          if (state.mode === "off" || state.mode === "metadata" || state.mode === "full") source = "state file";
        } catch { /* absent state is expected */ }
        print(`mode: ${mode}\nstate file: ${recordingStatePath()}\nsource: ${source}\nrecordings directory: ${recordingDirectory()}`);
        return 0;
      }
      const handled = await runRecordSubcommand(argv.slice(1), io);
      if (handled !== undefined) return handled;
      print(usage);
      return 2;
    }
    if (command === "version" && argv.length === 1) {
      print(`CHR ${chrVersion}`);
      return 0;
    }
    if (command === "doctor" && argv.length === 1) {
      const config = await readConfigFile();
      const report = await runDoctor({
        revision: config.revision,
        configPath: config.path,
        configMissing: config.missing,
        configErrors: config.errors,
      });
      print(report.output);
      return report.exitCode;
    }
    if (command === "secret" && argv[1] === "list" && argv.length === 2) {
      for (const name of await listSecretNames()) print(name);
      return 0;
    }
    if (command === "secret" && argv[1] === "remove" && argv.length === 3) {
      const name = argv[2];
      const removed = await removeSecret(name);
      print(removed ? `Removed secret ${name}` : `Secret ${name} was not found`);
      return 0;
    }
    if (command === "secret" && argv[1] === "set" && (argv.length === 3 || argv.length === 5)) {
      const name = argv[2];
      let value: string | undefined;
      if (argv.length === 5) {
        if (argv[3] !== "--from-env") { print(usage); return 2; }
        value = process.env[argv[4]];
        if (value === undefined) throw new Error(`environment variable ${argv[4]} is not set`);
      } else {
        const input = createInterface({ input: process.stdin, output: process.stdout });
        try {
          value = await input.question("Secret (input may be visible): ");
        } finally {
          input.close();
        }
      }
      await setSecret(name, value);
      print(`Stored secret ${name}`);
      return 0;
    }
    if (command === "verify") {
      let copilot: string | undefined;
      let timeout: number | undefined;
      let json = false;
      let accept = false;
      for (let index = 1; index < argv.length; index += 1) {
        if (argv[index] === "--json") {
          json = true;
        } else if (argv[index] === "--accept") {
          accept = true;
        } else if (argv[index] === "--copilot" && argv[index + 1]) {
          copilot = argv[++index];
        } else if (argv[index] === "--timeout" && argv[index + 1]) {
          timeout = Number(argv[++index]);
          if (!Number.isFinite(timeout) || timeout <= 0) {
            print("Invalid --timeout value.");
            return 2;
          }
        } else {
          print(usage);
          return 2;
        }
      }
      const config = await readConfigFile();
      const result = await runVerify({
        revision: config.revision,
        copilot,
        timeoutMs: timeout,
      });
      // The config is only ever touched when every probe passed on a known version.
      let accepted = false;
      let acceptMessage = "";
      let exitCode = result.exitCode;
      if (accept && result.exitCode === 0 && result.version) {
        const outcome = await acceptTestedCliVersion(result.version, {
          print: (value) => { acceptMessage = acceptMessage ? `${acceptMessage}\n${value}` : value; },
          usage: io.usage,
        });
        accepted = outcome.accepted;
        if (outcome.exitCode !== 0) exitCode = outcome.exitCode;
      }
      print(json
        ? JSON.stringify({ ...result, exitCode, accepted, ...(acceptMessage ? { acceptMessage } : {}) }, null, 2)
        : [result.output, acceptMessage].filter(Boolean).join("\n"));
      return exitCode;
    }
    if (command === "config" && argv[1] === "init" && (argv.length === 2 || (argv.length === 3 && argv[2] === "--force"))) {
      const filePath = process.env.CHR_CONFIG_PATH ?? defaultConfigPath();
      await initConfigFile(filePath, argv[2] === "--force");
      print(`Created starter configuration at ${filePath}.`);
      return 0;
    }
    if (command === "config" && argv[1] === "validate" && argv.length === 2) {
      const config = await readConfigFile();
      if (!config.revision) {
        print(config.missing
          ? `Configuration is missing at ${config.path}. Run "chr config init".`
          : `Configuration is invalid at ${config.path}:\n${config.errors.map((error) => `  ${error.path || "(root)"} [${error.code}]: ${error.message}`).join("\n")}`);
        return 1;
      }
      print(`Configuration is valid (revision ${config.revision.revisionId}).`);
      return 0;
    }
    if (command === "config" && argv[1] === "show" && argv.length === 2) {
      const config = await readConfigFile();
      if (!config.revision) {
        print(config.missing
          ? `Configuration is missing at ${config.path}. Run "chr config init".`
          : `Configuration is invalid at ${config.path}:\n${config.errors.map((error) => `  ${error.path || "(root)"} [${error.code}]: ${error.message}`).join("\n")}`);
        return 1;
      }
      print(JSON.stringify(redactEffective(config.revision), null, 2));
      return 0;
    }
    if (command === "launch") {
      const separator = argv.indexOf("--");
      const args = separator < 0 ? [] : argv.slice(separator + 1);
      let modelAlias: string | undefined;
      const launchArgs = separator < 0 ? argv.slice(1) : argv.slice(1, separator);
      for (let index = 0; index < launchArgs.length; index += 1) {
        if (launchArgs[index] === "--model" && launchArgs[index + 1]) modelAlias = launchArgs[++index];
        else { print(usage); return 2; }
      }
      const revision = await loadConfigFile();
      const doctor = await runDoctor({ revision });
      if (doctor.exitCode !== 0 || !doctor.executable || !doctor.version) {
        process.stderr.write(`CHR: ${doctor.blocking[0] ?? "Copilot is not ready to launch"}\n`);
        return 1;
      }
      return await launchCopilot({
        revision,
        args,
        childCommand: doctor.executable,
        version: doctor.version,
        modelAlias,
      });
    }
    if (command === "intercept") {
      const separator = argv.indexOf("--");
      const launchArgs = separator < 0 ? argv.slice(1) : argv.slice(separator + 1);
      if (separator < 0 && launchArgs.length) {
        print(usage);
        return 2;
      }
      const revision = await loadConfigFile();
      return await launchIntercept({ revision, args: launchArgs });
    }
    if (command === "provider") return await runProviderCommand(argv.slice(1), io);
    if (command === "model") return await runModelCommand(argv.slice(1), io);
    if (
      (command === "sessions" || command === "recording" || command === "traces" || command === "route")
    ) {
      print(`${command} is not implemented in this release.`);
      return 2;
    }
    print(usage);
    return 2;
  } catch (error) {
    print(`CHR error: ${error instanceof Error ? error.message : String(error)}`);
    return 1;
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  runCli().then((code) => {
    process.exitCode = code;
  });
}
