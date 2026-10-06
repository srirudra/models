import { describe, expect, it, vi } from "vitest";
import { buildCatalogEntry, mergeCatalog } from "../src/intercept/catalog.js";

const model = {
  alias: "qwen",
  displayName: "Qwen",
  provider: "local",
  upstreamModel: "Qwen/Qwen3",
  capabilities: {
    streaming: true,
    tools: true,
    vision: false,
    contextWindowTokens: 131072,
    maxOutputTokens: 16384,
  },
};

describe("intercept catalog", () => {
  it("builds a chat-completions-only entry", () => {
    const entry = buildCatalogEntry(model);
    expect(entry).toMatchObject({
      id: "qwen",
      name: "Qwen",
      vendor: "CHR",
      supported_endpoints: ["/chat/completions"],
      capabilities: {
        supports: { tool_calls: true, streaming: true, vision: false },
        limits: { max_context_window_tokens: 131072, max_output_tokens: 16384 },
      },
    });
    expect(entry).not.toHaveProperty("billing");
    expect(entry).not.toHaveProperty("info_messages");
    expect(entry).not.toHaveProperty("reasoning_effort");
    expect(entry).not.toHaveProperty("ws");
  });

  it("appends custom entries while preserving GitHub entries", () => {
    const result = mergeCatalog({ data: [{ id: "gpt-5", name: "GPT" }] }, [model]);
    expect(result.data).toEqual([
      { id: "gpt-5", name: "GPT" },
      expect.objectContaining({ id: "qwen" }),
    ]);
  });

  it("skips collisions and warns", () => {
    const warn = vi.fn();
    const result = mergeCatalog({ data: [{ id: "qwen" }] }, [model], warn);
    expect(result.data).toHaveLength(1);
    expect(warn).toHaveBeenCalledWith(expect.stringContaining("qwen"));
  });

  it("uses default token limits when the model leaves them unspecified", () => {
    const entry = buildCatalogEntry({
      ...model,
      capabilities: { ...model.capabilities, contextWindowTokens: null, maxOutputTokens: undefined },
    });
    expect(entry).toMatchObject({
      capabilities: { limits: { max_context_window_tokens: 131072, max_output_tokens: 16384, max_prompt_tokens: 114688 } },
    });
  });
});
