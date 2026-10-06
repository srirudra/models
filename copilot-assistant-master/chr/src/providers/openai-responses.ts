import { Provider } from "../config/types.js";
import { forwardSSE } from "./forward.js";
export function forwardResponses(provider: Provider, body: Record<string, unknown>, signal: AbortSignal, onChunk: (chunk: Uint8Array) => void): Promise<void> {
  return forwardSSE(provider, "/v1/responses", body, signal, onChunk);
}
