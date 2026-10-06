import { access, mkdir, readFile, readdir, unlink, writeFile } from "node:fs/promises";
import { constants } from "node:fs";
import path from "node:path";
import { spawn } from "node:child_process";

const namePattern = /^[A-Za-z0-9_.-]{1,64}$/;

export type PowerShellRunner = (
  script: string,
  environment: NodeJS.ProcessEnv,
) => Promise<string>;

let powerShellExecutable = process.platform === "win32" ? "powershell.exe" : "powershell";

async function defaultRunPowerShell(
  script: string,
  environment: NodeJS.ProcessEnv,
): Promise<string> {
  return new Promise((resolve, reject) => {
    const child = spawn(
      powerShellExecutable,
      ["-NoProfile", "-NonInteractive", "-Command", script],
      { env: environment, windowsHide: true, stdio: ["ignore", "pipe", "pipe"] },
    );
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk) => { stdout += chunk.toString(); });
    child.stderr.on("data", (chunk) => { stderr += chunk.toString(); });
    child.once("error", reject);
    child.once("close", (code) => {
      if (code === 0) resolve(stdout.trim());
      else reject(new Error(`PowerShell DPAPI operation failed${stderr ? `: ${stderr.trim()}` : ""}`));
    });
  });
}

let runPowerShell: PowerShellRunner = defaultRunPowerShell;
let directoryOverride: string | undefined;

export function setDpapiTestSeams(options: {
  runPowerShell?: PowerShellRunner;
  secretsDirectory?: string;
  powerShellExecutable?: string;
}): void {
  if (options.runPowerShell) runPowerShell = options.runPowerShell;
  directoryOverride = options.secretsDirectory;
  if (options.powerShellExecutable) powerShellExecutable = options.powerShellExecutable;
}

export function resetDpapiTestSeams(): void {
  runPowerShell = defaultRunPowerShell;
  directoryOverride = undefined;
  powerShellExecutable = process.platform === "win32" ? "powershell.exe" : "powershell";
}

function validateName(name: string): void {
  if (!namePattern.test(name)) throw new Error("secret name must match [A-Za-z0-9_.-]{1,64}");
}

function secretsDirectory(): string {
  return directoryOverride ?? path.join(
    process.env.LOCALAPPDATA ?? path.join(process.env.USERPROFILE ?? process.cwd(), "AppData", "Local"),
    "CHR",
    "secrets",
  );
}

function secretPath(name: string): string {
  validateName(name);
  return path.join(secretsDirectory(), `${name}.dpapi`);
}

const protectScript = `
Add-Type -AssemblyName System.Security
$bytes = [System.Security.Cryptography.ProtectedData]::Protect(
  [Convert]::FromBase64String($env:CHR_SECRET_B64), $null, 'CurrentUser')
[Convert]::ToBase64String($bytes)
`;
const unprotectScript = `
Add-Type -AssemblyName System.Security
$bytes = [System.Security.Cryptography.ProtectedData]::Unprotect(
  [Convert]::FromBase64String($env:CHR_SECRET_B64), $null, 'CurrentUser')
[Convert]::ToBase64String($bytes)
`;

async function transform(script: string, value: Buffer): Promise<Buffer> {
  const output = await runPowerShell(script, {
    ...process.env,
    CHR_SECRET_B64: value.toString("base64"),
  });
  return Buffer.from(output, "base64");
}

export async function setSecret(name: string, plaintext: string): Promise<void> {
  const filename = secretPath(name);
  const encrypted = await transform(protectScript, Buffer.from(plaintext, "utf8"));
  await mkdir(secretsDirectory(), { recursive: true });
  await writeFile(filename, encrypted.toString("base64"), { mode: 0o600 });
}

export async function getSecret(name: string): Promise<string | undefined> {
  const filename = secretPath(name);
  try {
    await access(filename, constants.F_OK);
  } catch {
    return undefined;
  }
  const encoded = await readFile(filename, "utf8");
  return (await transform(unprotectScript, Buffer.from(encoded.trim(), "base64"))).toString("utf8");
}

export async function removeSecret(name: string): Promise<boolean> {
  try {
    await unlink(secretPath(name));
    return true;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return false;
    throw error;
  }
}

export async function listSecretNames(): Promise<string[]> {
  try {
    const entries = await readdir(secretsDirectory(), { withFileTypes: true });
    return entries
      .filter((entry) => entry.isFile() && entry.name.endsWith(".dpapi"))
      .map((entry) => entry.name.slice(0, -".dpapi".length))
      .filter((name) => namePattern.test(name))
      .sort();
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return [];
    throw error;
  }
}
