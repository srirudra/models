import { randomBytes } from "node:crypto";
import { mkdtemp, unlink, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import forge from "node-forge";

export type EphemeralCa = {
  caPem: string;
  issueLeaf(host: string): { key: string; cert: string };
};

export function createEphemeralCa(): EphemeralCa {
  const keys = forge.pki.rsa.generateKeyPair(2048);
  const ca = forge.pki.createCertificate();
  ca.publicKey = keys.publicKey;
  ca.serialNumber = randomBytes(16).toString("hex");
  ca.validity.notBefore = new Date(Date.now() - 60_000);
  ca.validity.notAfter = new Date(Date.now() + 86_400_000);
  ca.setSubject([{ name: "commonName", value: "CHR ephemeral CA" }]);
  ca.setIssuer(ca.subject.attributes);
  ca.setExtensions([
    { name: "basicConstraints", cA: true },
    { name: "keyUsage", keyCertSign: true, cRLSign: true },
  ]);
  ca.sign(keys.privateKey, forge.md.sha256.create());

  const leaves = new Map<string, { key: string; cert: string }>();
  return {
    caPem: forge.pki.certificateToPem(ca),
    issueLeaf(host) {
      const existing = leaves.get(host);
      if (existing) return existing;
      const leafKeys = forge.pki.rsa.generateKeyPair(2048);
      const leaf = forge.pki.createCertificate();
      leaf.publicKey = leafKeys.publicKey;
      leaf.serialNumber = randomBytes(16).toString("hex");
      leaf.validity.notBefore = new Date(Date.now() - 60_000);
      leaf.validity.notAfter = new Date(Date.now() + 86_400_000);
      leaf.setSubject([{ name: "commonName", value: host }]);
      leaf.setIssuer(ca.subject.attributes);
      leaf.setExtensions([
        { name: "basicConstraints", cA: false },
        { name: "keyUsage", digitalSignature: true, keyEncipherment: true },
        { name: "extKeyUsage", serverAuth: true },
        { name: "subjectAltName", altNames: [{ type: 2, value: host }] },
      ]);
      leaf.sign(keys.privateKey, forge.md.sha256.create());
      const result = {
        key: forge.pki.privateKeyToPem(leafKeys.privateKey),
        cert: forge.pki.certificateToPem(leaf),
      };
      leaves.set(host, result);
      return result;
    },
  };
}

export async function writeCaFile(caPem: string): Promise<string> {
  const directory = await mkdtemp(path.join(os.tmpdir(), "chr-ca-"));
  const filePath = path.join(directory, `${randomBytes(8).toString("hex")}.pem`);
  await writeFile(filePath, caPem, { encoding: "utf8", mode: 0o600 });
  return filePath;
}

export async function removeCaFile(filePath: string): Promise<void> {
  await unlink(filePath).catch((error: NodeJS.ErrnoException) => {
    if (error.code !== "ENOENT") throw error;
  });
}
