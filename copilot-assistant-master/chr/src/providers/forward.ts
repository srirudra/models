import { Provider } from "../config/types.js";
import { resolveCredential } from "../config/config.js";
import { upstreamHeaders } from "./headers.js";
import { SSEParser } from "./sse.js";

export type ForwardErrorCode =
  | "connect_timeout"
  | "first_byte_timeout"
  | "stream_idle_timeout"
  | "total_timeout"
  | "provider_error"
  | "upstream_error";

export class ForwardError extends Error {
  constructor(
    public readonly code: ForwardErrorCode,
    message: string,
  ) {
    super(`${code}:${message}`);
    this.name = "ForwardError";
  }
}

function timeoutError(code: ForwardErrorCode, milliseconds: number): ForwardError {
  return new ForwardError(code, `upstream timeout after ${milliseconds}ms`);
}

function providerError(data: string): ForwardError | undefined {
  try {
    const value: unknown = JSON.parse(data);
    if (
      value &&
      typeof value === "object" &&
      "error" in value &&
      value.error &&
      typeof value.error === "object"
    ) {
      return new ForwardError("provider_error", data);
    }
  } catch {
    return undefined;
  }
  return undefined;
}

function safeUpstreamErrorBody(data: string): string {
  return data
    .replace(/Bearer\s+\S+/gi, "Bearer [redacted]")
    .replace(/\bsk-[A-Za-z0-9_-]+\b/g, "[redacted]")
    .slice(0, 1024);
}

async function readWithTimeout(
  reader: ReadableStreamDefaultReader<Uint8Array>,
  milliseconds: number,
  code: ForwardErrorCode,
  signal: AbortSignal,
): Promise<ReadableStreamReadResult<Uint8Array>> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  let abort: (() => void) | undefined;
  try {
    return await Promise.race([
      reader.read(),
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => {
          const error = timeoutError(code, milliseconds);
          reject(error);
        }, milliseconds);
        abort = () => reject(signal.reason ?? new Error("request aborted"));
        signal.addEventListener("abort", abort, { once: true });
      }),
    ]);
  } finally {
    if (timer) {
      clearTimeout(timer);
    }
    if (abort) {
      signal.removeEventListener("abort", abort);
    }
  }
}

export async function forwardSSE(
  provider: Provider,
  path: string,
  body: Record<string, unknown>,
  signal: AbortSignal,
  onChunk: (chunk: Uint8Array) => void | Promise<void>,
): Promise<void> {
  const controller = new AbortController();
  const totalTimer = setTimeout(() => controller.abort(new ForwardError(
    "total_timeout",
    `upstream timeout after ${provider.timeoutsMs.total}ms`,
  )), provider.timeoutsMs.total);
  const abort = () => controller.abort(signal.reason);
  signal.addEventListener("abort", abort);
  let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;

  try {
    const connectTimer = setTimeout(() => controller.abort(
      timeoutError("connect_timeout", provider.timeoutsMs.connect),
    ), provider.timeoutsMs.connect);
    let response: Response;
    try {
      response = await fetch(
        new URL(path, provider.baseUrl),
        {
          method: "POST",
          headers: upstreamHeaders(undefined, resolveCredential(provider.credentialRef)),
          body: JSON.stringify(body),
          signal: controller.signal,
          redirect: "manual",
        },
      );
    } catch (error) {
      clearTimeout(connectTimer);
      if (error instanceof ForwardError) {
        throw error;
      }
      if (controller.signal.reason instanceof ForwardError) {
        throw controller.signal.reason;
      }
      throw new ForwardError("upstream_error", error instanceof Error ? error.message : "request failed");
    }
    clearTimeout(connectTimer);

    if (response.status >= 300 && response.status < 400) {
      const requestUrl = new URL(path, provider.baseUrl);
      const location = response.headers.get("location");
      let destination = "unknown origin";
      if (location) {
        try {
          destination = new URL(location, requestUrl).origin;
        } catch {
          destination = "invalid origin";
        }
      }
      throw new ForwardError(
        "provider_error",
        `provider redirect to ${destination} rejected`,
      );
    }
    if (!response.ok) {
      const detail = safeUpstreamErrorBody(await response.text());
      throw new ForwardError(
        "upstream_error",
        `status ${response.status}${detail ? `: ${detail}` : ""}`,
      );
    }
    if (!response.body) {
      throw new ForwardError("upstream_error", `status ${response.status}: empty response body`);
    }

    reader = response.body.getReader();
    const parser = new SSEParser();
    let terminal = false;
    let firstByte = true;
    while (true) {
      const result = await readWithTimeout(
        reader,
        firstByte ? provider.timeoutsMs.firstByte : provider.timeoutsMs.streamIdle,
        firstByte ? "first_byte_timeout" : "stream_idle_timeout",
        controller.signal,
      );

      if (result.done) {
        break;
      }
      firstByte = false;
      for (const event of parser.feed(result.value)) {
        const done = event.data === "[DONE]";
        if (done) terminal = true;
        let parsed: unknown = undefined;
        if (!done) try { parsed = JSON.parse(event.data); } catch {
          throw new ForwardError("provider_error", "malformed SSE JSON");
        }
        const error = providerError(event.data);
        if (error) {
          throw error;
        }
        if (provider.protocol === "openai-responses" && parsed && typeof parsed === "object" &&
          ["response.completed", "response.incomplete", "response.failed"].includes(
            (parsed as { type?: unknown }).type as string,
          )) terminal = true;
        // Awaiting the sink keeps the reader paused while a slow client drains.
        await onChunk(new TextEncoder().encode(event.raw));
      }
    }

    for (const event of parser.feed(new Uint8Array(), true)) {
      const done = event.data === "[DONE]";
      if (done) terminal = true;
      let parsed: unknown = undefined;
      if (!done) try { parsed = JSON.parse(event.data); } catch {
        throw new ForwardError("provider_error", "malformed SSE JSON");
      }
      const error = providerError(event.data);
      if (error) {
        throw error;
      }
      if (provider.protocol === "openai-responses" && parsed && typeof parsed === "object" &&
        ["response.completed", "response.incomplete", "response.failed"].includes(
          (parsed as { type?: unknown }).type as string,
        )) terminal = true;
      await onChunk(new TextEncoder().encode(event.raw));
    }
    if (!terminal) throw new ForwardError("provider_error", "provider stream ended without terminal signal");
  } catch (error) {
    if (error instanceof ForwardError) {
      controller.abort(error);
    }
    throw error;
  } finally {
    if (reader) {
      await reader.cancel().catch(() => undefined);
    }
    clearTimeout(totalTimer);
    signal.removeEventListener("abort", abort);
  }
}
