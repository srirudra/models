export type Classification = {
  kind: "primary" | "auxiliary";
  signals: string[];
};

export function classifyRequest(
  body: Record<string, unknown>,
  selectedModel?: string,
): Classification {
  const signals: string[] = [];
  const model = typeof body.model === "string" ? body.model : "";
  if (selectedModel && model && model !== selectedModel) {
    signals.push("model-differs-from-selected");
  }

  const messages = Array.isArray(body.messages)
    ? body.messages as Array<Record<string, unknown>>
    : [];
  const system = messages.find((message) => message.role === "system")?.content;
  if (
    typeof system === "string" &&
    /classif|frustrat|prompt refinement|session and branch naming/i.test(system)
  ) {
    signals.push("classifier-style-system-prompt");
  }
  if (!body.tools) {
    signals.push("no-tools");
  }
  if (typeof body.max_tokens === "number" && body.max_tokens <= 256) {
    signals.push("small-max-tokens");
  }

  const strongSignal =
    signals.includes("model-differs-from-selected") ||
    signals.includes("classifier-style-system-prompt") ||
    (signals.includes("small-max-tokens") && signals.includes("no-tools"));
  return {
    kind: strongSignal ? "auxiliary" : "primary",
    signals,
  };
}
