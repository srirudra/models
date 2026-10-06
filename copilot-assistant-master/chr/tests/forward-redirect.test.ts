import http from "node:http";
import { afterEach, describe, expect, it, vi } from "vitest";
import { forwardSSE, ForwardError } from "../src/providers/forward.js";
import type { Provider } from "../src/config/types.js";

let first: http.Server | undefined;
let second: http.Server | undefined;

afterEach(async () => {
  vi.unstubAllEnvs();
  await new Promise<void>((resolve) => first?.close(() => resolve()) ?? resolve());
  await new Promise<void>((resolve) => second?.close(() => resolve()) ?? resolve());
  first = undefined;
  second = undefined;
});

function listen(server: http.Server): Promise<number> {
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve((server.address() as import("node:net").AddressInfo).port)));
}

describe("forward redirect hardening", () => {
  it("does not follow a provider redirect", async () => {
    let secondRequests = 0;
    second = http.createServer((_req, res) => { secondRequests++; res.end("unexpected"); });
    const secondPort = await listen(second);
    first = http.createServer((_req, res) => {
      res.writeHead(307, { location: `http://127.0.0.1:${secondPort}/leak` });
      res.end();
    });
    const firstPort = await listen(first);
    vi.stubEnv("FORWARD_REDIRECT_TEST_KEY", "secret");
    const provider: Provider = {
      protocol: "openai-chat-completions",
      baseUrl: `http://127.0.0.1:${firstPort}/v1`,
      credentialRef: "env:FORWARD_REDIRECT_TEST_KEY",
      timeoutsMs: { connect: 1000, firstByte: 1000, streamIdle: 1000, total: 2000 },
    };
    const error = await forwardSSE(provider, "/chat/completions", {}, new AbortController().signal, () => undefined)
      .catch((value: unknown) => value);
    expect(error).toBeInstanceOf(ForwardError);
    expect(error).toMatchObject({ code: "provider_error" });
    expect((error as ForwardError).message).toContain(`http://127.0.0.1:${secondPort}`);
    expect(secondRequests).toBe(0);
  });
});
