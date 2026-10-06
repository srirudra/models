/**
 * Keep the Copilot request shape, removing only extensions that strict
 * OpenAI-compatible servers reject.  This is deliberately non-mutating and
 * safe to apply more than once.
 */
export function normalizeChatCompletionsBody(
  body: Record<string, unknown>,
): Record<string, unknown> {
  const normalized = { ...body };
  // Copilot adds stream_options, which strict vLLM OpenAI endpoints may reject
  // as an unexpected field. It is not needed by the interceptor.
  delete normalized.stream_options;
  return normalized;
}
