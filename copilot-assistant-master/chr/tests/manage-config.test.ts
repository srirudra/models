import { readFile, readdir } from "node:fs/promises";
import path from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { removeAll, run, seedConfigFile, tempDirectory, withEnv } from "./manage-support.js";

const directories: string[] = [];
const restore: Array<() => void> = [];
let configFile = "";
let directory = "";

beforeEach(async () => {
  directory = await tempDirectory();
  directories.push(directory);
  configFile = await seedConfigFile(directory);
  restore.push(withEnv("CHR_CONFIG_PATH", configFile));
  restore.push(withEnv("CHR_MANAGE_TEST_KEY", "test-key-value"));
});

afterEach(async () => {
  while (restore.length > 0) restore.pop()!();
  await removeAll(directories);
});

const read = async (): Promise<string> => await readFile(configFile, "utf8");

describe("chr provider management", () => {
  it("adds a provider, keeps unknown keys, and leaves no temporary file", async () => {
    const added = await run(["provider", "add", "extra", "--base-url", "http://127.0.0.1:9/v1", "--key-env", "CHR_MANAGE_TEST_KEY"]);
    expect(added.code).toBe(0);
    const text = await read();
    expect(text).toContain("unknown keys must survive CHR writes");
    const parsed = JSON.parse(text) as { providers: Record<string, Record<string, unknown>> };
    expect(parsed.providers.extra).toEqual({
      protocol: "openai-chat-completions",
      baseUrl: "http://127.0.0.1:9/v1",
      credentialRef: "env:CHR_MANAGE_TEST_KEY",
      timeoutsMs: { connect: 10000, firstByte: 120000, streamIdle: 60000, total: 900000 },
    });
    expect(Object.keys(parsed.providers)).toEqual(["local", "extra"]);
    expect((await readdir(directory)).filter((name) => name.endsWith(".tmp"))).toEqual([]);
  });

  it("maps --protocol responses and lists credential refs without values", async () => {
    expect((await run(["provider", "add", "r", "--base-url", "https://example.test/v1", "--key-env", "CHR_MANAGE_TEST_KEY", "--protocol", "responses"])).code).toBe(0);
    const listed = await run(["provider", "list"]);
    expect(listed.code).toBe(0);
    expect(listed.out).toContain("openai-responses");
    expect(listed.out).toContain("env:CHR_MANAGE_TEST_KEY");
    expect(listed.out).not.toContain("test-key-value");
    expect(listed.out).toMatch(/r\s+openai-responses\s+\S+\s+env:CHR_MANAGE_TEST_KEY\s+yes/);
  });

  it("reports an unresolvable credential as no", async () => {
    restore.push(withEnv("CHR_MANAGE_MISSING_KEY", undefined));
    expect((await run(["provider", "add", "gap", "--base-url", "https://example.test/v1", "--key-env", "CHR_MANAGE_MISSING_KEY"])).code).toBe(0);
    const listed = await run(["provider", "list"]);
    expect(listed.out).toMatch(/gap\s+\S+\s+\S+\s+env:CHR_MANAGE_MISSING_KEY\s+no/);
  });

  it("refuses to overwrite an existing provider unless --force", async () => {
    const duplicate = await run(["provider", "add", "local", "--base-url", "https://other.test/v1", "--key-env", "CHR_MANAGE_TEST_KEY"]);
    expect(duplicate.code).toBe(1);
    expect(duplicate.out).toContain("already exists");
    expect(await read()).toContain("https://example.test/v1");

    const forced = await run(["provider", "add", "local", "--base-url", "https://other.test/v1", "--key-env", "CHR_MANAGE_TEST_KEY", "--force"]);
    expect(forced.code).toBe(0);
    expect(await read()).toContain("https://other.test/v1");
  });

  it("rejects a non-HTTP base URL and leaves the file untouched", async () => {
    const before = await read();
    const result = await run(["provider", "add", "bad", "--base-url", "ftp://example.test", "--key-env", "CHR_MANAGE_TEST_KEY"]);
    expect(result.code).toBe(1);
    expect(result.out).toContain("HTTP(S) URL");
    expect(await read()).toBe(before);
  });

  it("hints at chr secret set when the named secret is absent", async () => {
    restore.push(withEnv("LOCALAPPDATA", directory));
    const result = await run(["provider", "add", "vault", "--base-url", "https://example.test/v1", "--key-secret", "CHR_MANAGE_ABSENT"]);
    expect(result.code).toBe(0);
    expect(result.out).toContain('chr secret set CHR_MANAGE_ABSENT');
  });

  it("refuses to remove a provider that models still reference", async () => {
    const result = await run(["provider", "remove", "local"]);
    expect(result.code).toBe(1);
    expect(result.out).toContain("alpha");
    expect(JSON.parse(await read()).providers.local).toBeDefined();
  });

  it("removes an unused provider", async () => {
    expect((await run(["provider", "add", "spare", "--base-url", "https://example.test/v1", "--key-env", "CHR_MANAGE_TEST_KEY"])).code).toBe(0);
    const result = await run(["provider", "remove", "spare"]);
    expect(result.code).toBe(0);
    expect(JSON.parse(await read()).providers.spare).toBeUndefined();
  });

  it("prints usage and exits 2 for unknown flags", async () => {
    const result = await run(["provider", "add", "x", "--base-url", "https://example.test/v1", "--key-env", "K", "--bogus"]);
    expect(result.code).toBe(2);
    expect(result.out).toContain("Usage:");
  });

  it("tells the user to run config init when the config is missing", async () => {
    restore.push(withEnv("CHR_CONFIG_PATH", path.join(directory, "absent.json")));
    const result = await run(["provider", "remove", "local"]);
    expect(result.code).toBe(1);
    expect(result.out).toContain('chr config init');
  });
});

describe("chr model management", () => {
  it("adds a model with documented defaults", async () => {
    const result = await run(["model", "add", "beta", "--provider", "local", "--upstream", "gpt-beta"]);
    expect(result.code).toBe(0);
    const models = JSON.parse(await read()).models as Array<Record<string, unknown>>;
    expect(models[1]).toEqual({
      alias: "beta",
      displayName: "beta",
      provider: "local",
      upstreamModel: "gpt-beta",
      capabilities: { streaming: true, tools: true, vision: false, contextWindowTokens: null },
    });
  });

  it("honours capability flags", async () => {
    expect((await run([
      "model", "add", "gamma", "--provider", "local", "--upstream", "gpt-gamma",
      "--display-name", "Gamma", "--context", "128000", "--max-output", "4096", "--no-tools", "--vision",
    ])).code).toBe(0);
    const models = JSON.parse(await read()).models as Array<Record<string, unknown>>;
    expect(models[1]).toMatchObject({
      displayName: "Gamma",
      capabilities: { streaming: true, tools: false, vision: true, contextWindowTokens: 128000, maxOutputTokens: 4096 },
    });
  });

  it("rejects an unknown provider", async () => {
    const before = await read();
    const result = await run(["model", "add", "delta", "--provider", "nope", "--upstream", "x"]);
    expect(result.code).toBe(1);
    expect(result.out).toContain("Unknown provider nope");
    expect(await read()).toBe(before);
  });

  it("rejects an invalid alias", async () => {
    const result = await run(["model", "add", "bad alias", "--provider", "local", "--upstream", "x"]);
    expect(result.code).toBe(1);
    expect(result.out).toContain("must match");
  });

  it("refuses a duplicate alias unless --force", async () => {
    const duplicate = await run(["model", "add", "alpha", "--provider", "local", "--upstream", "gpt-other"]);
    expect(duplicate.code).toBe(1);
    expect(duplicate.out).toContain("already exists");
    const forced = await run(["model", "add", "alpha", "--provider", "local", "--upstream", "gpt-other", "--force"]);
    expect(forced.code).toBe(0);
    const models = JSON.parse(await read()).models as Array<Record<string, unknown>>;
    expect(models).toHaveLength(1);
    expect(models[0].upstreamModel).toBe("gpt-other");
  });

  it("lists and removes models", async () => {
    const listed = await run(["model", "list"]);
    expect(listed.code).toBe(0);
    expect(listed.out).toMatch(/alpha\s+Alpha\s+local\s+gpt-test\s+yes\s+no\s+-/);
    expect((await run(["model", "remove", "alpha"])).code).toBe(0);
    expect(JSON.parse(await read()).models).toEqual([]);
    const missing = await run(["model", "remove", "alpha"]);
    expect(missing.code).toBe(1);
    expect(missing.out).toContain("was not found");
  });
});
