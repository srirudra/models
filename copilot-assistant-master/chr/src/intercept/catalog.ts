import type { ConfigRevision } from "../config/types.js";

type CatalogModel = ConfigRevision["models"][number];
export type CatalogJson = { data: unknown[]; [key: string]: unknown };

export function buildCatalogEntry(model: CatalogModel): Record<string, unknown> {
  const context = model.capabilities.contextWindowTokens ?? 131072;
  const output = model.capabilities.maxOutputTokens ?? 16384;
  return {
    id: model.alias,
    name: model.displayName,
    version: model.alias,
    family: model.alias,
    vendor: "CHR",
    object: "model",
    supported_endpoints: ["/chat/completions"],
    model_picker_enabled: true,
    preview: false,
    policy: { state: "enabled", terms: "Custom model routed locally by CHR." },
    model_picker_category: "versatile",
    capabilities: {
      object: "model_capabilities",
      type: "chat",
      family: model.alias,
      tokenizer: "o200k_base",
      supports: {
        tool_calls: model.capabilities.tools,
        streaming: true,
        parallel_tool_calls: true,
        vision: model.capabilities.vision,
        structured_outputs: true,
      },
      limits: {
        max_context_window_tokens: context,
        max_prompt_tokens: context - output,
        max_output_tokens: output,
      },
    },
  };
}

export function mergeCatalog(
  rawGithubJson: unknown,
  models: readonly CatalogModel[],
  warn: (message: string) => void = () => undefined,
): CatalogJson {
  if (!rawGithubJson || typeof rawGithubJson !== "object") {
    throw new Error("GitHub catalog must be an object");
  }
  const source = rawGithubJson as { data?: unknown };
  if (!Array.isArray(source.data)) {
    throw new Error("GitHub catalog data must be an array");
  }
  const data = [...source.data];
  const ids = new Set(data.flatMap((entry) =>
    entry && typeof entry === "object" && typeof (entry as { id?: unknown }).id === "string"
      ? [(entry as { id: string }).id]
      : [],
  ));
  for (const model of models) {
    if (model.provider && ids.has(model.alias)) {
      warn(`Skipping custom model ${model.alias}: GitHub catalog already contains that id`);
      continue;
    }
    if (model.provider && model.capabilities.streaming && model.provider) {
      data.push(buildCatalogEntry(model));
      ids.add(model.alias);
    }
  }
  return { ...source, data };
}
