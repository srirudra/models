import { Config, ConfigRevision, ValidationError } from "./types.js";
import { getSecret } from "../secrets/dpapi.js";

let nextRevision = 0;

const clone = <T>(value: T): T => structuredClone(value);

export function validateConfig(input: unknown): ValidationError[] {
  const errors: ValidationError[] = [];
  if (!input || typeof input !== "object") {
    return [{ path: "", code: "schema", message: "configuration must be an object" }];
  }

  const config = input as Partial<Config>;
  if (config.schemaVersion !== 1) {
    errors.push({
      path: "schemaVersion",
      code: "schema",
      message: "schemaVersion must be 1",
    });
  }
  if (!config.providers || typeof config.providers !== "object") {
    errors.push({
      path: "providers",
      code: "schema",
      message: "providers is required",
    });
  }

  const providers = config.providers ?? {};
  for (const [id, provider] of Object.entries(providers)) {
    if (
      !provider ||
      !["openai-chat-completions", "openai-responses"].includes(provider.protocol)
    ) {
      errors.push({
        path: `providers.${id}.protocol`,
        code: "protocol",
        message: "unsupported protocol",
      });
    }
    try {
      const url = new URL(provider.baseUrl);
      if (!["http:", "https:"].includes(url.protocol)) {
        throw new Error("unsupported URL protocol");
      }
    } catch {
      errors.push({
        path: `providers.${id}.baseUrl`,
        code: "url",
        message: "baseUrl must be an HTTP(S) URL",
      });
    }
    if (
      !provider.credentialRef?.startsWith("env:") &&
      !provider.credentialRef?.startsWith("windows-credential:")
    ) {
      errors.push({
        path: `providers.${id}.credentialRef`,
        code: "credential",
        message: "credentialRef must reference env: or windows-credential:",
      });
    }
    if (
      !provider.timeoutsMs ||
      Object.values(provider.timeoutsMs).some(
        (timeout) => !Number.isInteger(timeout) || timeout <= 0,
      )
    ) {
      errors.push({
        path: `providers.${id}.timeoutsMs`,
        code: "schema",
        message: "all timeouts must be positive integers",
      });
    }
  }

  const models = config.models ?? [];
  const aliases = new Set<string>();
  for (let index = 0; index < models.length; index++) {
    const model = models[index];
    if (aliases.has(model.alias)) {
      errors.push({
        path: `models.${index}.alias`,
        code: "duplicate",
        message: `duplicate alias ${model.alias}`,
      });
    }
    aliases.add(model.alias);

    const provider = providers[model.provider];
    if (!provider) {
      errors.push({
        path: `models.${index}.provider`,
        code: "reference",
        message: `unknown provider ${model.provider}`,
      });
      continue;
    }
    if (
      !model.capabilities ||
      model.capabilities.streaming !== true ||
      typeof model.capabilities.tools !== "boolean" ||
      typeof model.capabilities.vision !== "boolean"
    ) {
      errors.push({
        path: `models.${index}.capabilities`,
        code: "capability",
        message: "capabilities must advertise streaming and supported feature flags",
      });
    }
    const maxOutputTokens = model.capabilities?.maxOutputTokens ?? 16384;
    if (
      model.capabilities &&
      ((!Number.isInteger(model.capabilities.maxOutputTokens) &&
        model.capabilities.maxOutputTokens !== undefined) ||
        maxOutputTokens <= 0 ||
        (model.capabilities.contextWindowTokens !== null &&
          model.capabilities.contextWindowTokens <= maxOutputTokens))
    ) {
      errors.push({
        path: `models.${index}.capabilities.maxOutputTokens`,
        code: "capability",
        message: "maxOutputTokens must be positive and less than contextWindowTokens",
      });
    }
    if (
      provider.protocol !== "openai-chat-completions" &&
      provider.protocol !== "openai-responses"
    ) {
      errors.push({
        path: `models.${index}.provider`,
        code: "protocol",
        message: "model capability does not match provider protocol",
      });
    }
  }

  if (config.routing?.githubLeg?.mode === "forward") {
    errors.push({
      path: "routing.githubLeg.mode",
      code: "SEC-15",
      message: "forwarding the GitHub leg is refused pending legal/ToS approval (SEC-15)",
    });
  }
  if (
    !config.routing?.auxiliary ||
    !["block", "allow", "route-to"].includes(config.routing.auxiliary.policy)
  ) {
    errors.push({
      path: "routing.auxiliary",
      code: "schema",
      message: "auxiliary policy must be block, allow, or route-to",
    });
  }
  if (
    config.routing?.auxiliary?.policy === "route-to" &&
    !providers[config.routing.auxiliary.provider ?? ""]
  ) {
    errors.push({
      path: "routing.auxiliary.provider",
      code: "reference",
      message: "auxiliary route-to provider is unknown",
    });
  }
  return errors;
}

export function loadConfig(input: unknown): {
  revision?: ConfigRevision;
  errors: ValidationError[];
} {
  const errors = validateConfig(input);
  if (errors.length > 0) {
    return { errors };
  }
  const value = clone(input as Config);
  for (const model of value.models) {
    model.capabilities.maxOutputTokens ??= 16384;
  }
  const revision = Object.assign(value, { revisionId: ++nextRevision });
  Object.freeze(revision.providers);
  Object.freeze(revision.models);
  Object.freeze(revision.routing);
  Object.freeze(revision);
  return {
    revision: revision as ConfigRevision,
    errors: [],
  };
}

const credentialCache = new Map<string, string>();

export async function primeCredentials(
  revision: ConfigRevision,
  environment: NodeJS.ProcessEnv = process.env,
): Promise<void> {
  for (const provider of Object.values(revision.providers)) {
    const ref = provider.credentialRef;
    let value: string | undefined;
    if (ref.startsWith("env:")) value = environment[ref.slice(4)];
    else if (ref.startsWith("windows-credential:")) value = await getSecret(ref.slice("windows-credential:".length));
    if (value === undefined) {
      const command = ref.startsWith("windows-credential:")
        ? `; run "chr secret set ${ref.slice("windows-credential:".length)}"`
        : "";
      throw new Error(`credential ${ref} not found${command}`);
    }
    credentialCache.set(ref, value);
  }
}

export function resolveCredential(ref: string): string | undefined {
  return credentialCache.get(ref) ??
    (ref.startsWith("env:") ? process.env[ref.slice(4)] : undefined);
}

export function redactEffective(revision: ConfigRevision): Config {
  const config = clone(revision);
  for (const provider of Object.values(config.providers)) {
    provider.credentialRef = "redacted";
  }
  return config;
}
