import { ConfigRevision } from "../config/types.js";
import { classifyRequest } from "./classifier.js";

export type Route =
  | { kind: "custom"; provider: string; upstreamModel: string }
  | { kind: "github-leg" }
  | { kind: "auxiliary"; policy: string; provider?: string; upstreamModel?: string }
  | { kind: "error"; code: string; message: string };

export function resolveRoute(
  modelId: string,
  revision: ConfigRevision,
  body?: Record<string, unknown>,
  selectedModel?: string,
): Route {
  const model = revision.models.find((candidate) => candidate.alias === modelId);
  if (body && classifyRequest(body, selectedModel).kind === "auxiliary") {
    const auxiliary = revision.routing.auxiliary;
    if (auxiliary.policy === "block" || auxiliary.policy === "allow") {
      return { kind: "auxiliary", policy: auxiliary.policy };
    }
    return {
      kind: "auxiliary",
      policy: "route-to",
      provider: auxiliary.provider,
      upstreamModel: model?.upstreamModel ?? modelId,
    };
  }

  if (model) {
    return {
      kind: "custom",
      provider: model.provider,
      upstreamModel: model.upstreamModel,
    };
  }
  if (modelId.startsWith("github/") || modelId.startsWith("gpt-")) {
    return revision.routing.githubLeg.mode === "fail-closed"
      ? { kind: "github-leg" }
      : {
        kind: "error",
        code: "SEC-15",
        message: "GitHub forwarding is disabled (SEC-15)",
      };
  }
  return {
    kind: "error",
    code: "unknown_model",
    message: `unknown model alias: ${modelId}`,
  };
}
