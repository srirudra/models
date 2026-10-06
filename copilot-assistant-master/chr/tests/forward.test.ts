import { describe, expect, it, vi } from "vitest";
import { ForwardError, forwardSSE } from "../src/providers/forward.js";
import { Provider } from "../src/config/types.js";

const provider: Provider = {
  protocol: "openai-chat-completions",
  baseUrl: "http://provider.test/v1",
  credentialRef: "env:KEY",
  timeoutsMs: {
    connect: 50,
    firstByte: 50,
    streamIdle: 50,
    total: 200,
  },
};

function responseFrom(chunks: string[], delay = 0): Response {
  const stream = new ReadableStream<Uint8Array>({
    async start(controller) {
      for (const chunk of chunks) {
        if (delay) {
          await new Promise((resolve) => setTimeout(resolve, delay));
        }
        controller.enqueue(new TextEncoder().encode(chunk));
      }
      controller.close();
    },
  });
  return new Response(stream, { status: 200 });
}

describe("forwardSSE", () => {
  it("re-emits parsed frames and rejects provider errors", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(responseFrom([
      "data: {\"ok\":true}\n\ndata: {\"error\":{\"message\":\"no\"}}\n\n",
    ])));
    const chunks: string[] = [];

    await expect(forwardSSE(
      provider,
      "/v1/chat/completions",
      { model: "coding" },
      new AbortController().signal,
      (chunk) => chunks.push(new TextDecoder().decode(chunk)),
    )).rejects.toMatchObject({ code: "provider_error" });
    expect(chunks).toEqual(['data: {"ok":true}\n\n']);
    vi.unstubAllGlobals();
  });

  it("distinguishes first-byte timeout", async () => {
    vi.stubGlobal("fetch", vi.fn().mockImplementation(() => {
      const stream = new ReadableStream<Uint8Array>({
        start() {
          return undefined;
        },
      });
      return Promise.resolve(new Response(stream));
    }));

    await expect(forwardSSE(
      provider,
      "/v1/chat/completions",
      {},
      new AbortController().signal,
      () => undefined,
    )).rejects.toMatchObject({ code: "first_byte_timeout" });
    vi.unstubAllGlobals();
  });
});
