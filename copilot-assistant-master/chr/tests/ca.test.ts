import { readFile } from "node:fs/promises";
import { describe, expect, it } from "vitest";
import forge from "node-forge";
import { createEphemeralCa, removeCaFile, writeCaFile } from "../src/intercept/ca.js";

describe("interceptor CA", () => {
  it("issues a cached leaf with the requested CN and SAN", () => {
    const ca = createEphemeralCa();
    const first = ca.issueLeaf("api.example.test");
    const second = ca.issueLeaf("api.example.test");
    const certificate = forge.pki.certificateFromPem(first.cert);
    expect(second).toBe(first);
    expect(certificate.subject.getField("CN")?.value).toBe("api.example.test");
    expect(certificate.getExtension("subjectAltName").altNames[0].value).toBe("api.example.test");
  });

  it("signs leaves with the ephemeral CA", () => {
    const ca = createEphemeralCa();
    const leaf = forge.pki.certificateFromPem(ca.issueLeaf("localhost").cert);
    const store = forge.pki.createCaStore([forge.pki.certificateFromPem(ca.caPem)]);
    expect(() => forge.pki.verifyCertificateChain(store, [leaf])).not.toThrow();
  });

  it("writes only the public certificate and removes it", async () => {
    const ca = createEphemeralCa();
    const file = await writeCaFile(ca.caPem);
    const contents = await readFile(file, "utf8");
    expect(contents).toContain("BEGIN CERTIFICATE");
    expect(contents).not.toContain("PRIVATE KEY");
    await removeCaFile(file);
    await expect(readFile(file)).rejects.toMatchObject({ code: "ENOENT" });
  });
});
