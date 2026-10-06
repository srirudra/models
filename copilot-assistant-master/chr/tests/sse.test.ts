import { describe, expect, it } from "vitest";
import { SSEParser } from "../src/providers/sse.js";

const bytes = (value: string): Uint8Array => new TextEncoder().encode(value);

describe("SSEParser", () => {
  it("parses multiple frames in one chunk", () => {
    const parser = new SSEParser();
    const events = parser.feed(bytes("data: one\n\ndata: two\n\n"));

    expect(events.map((event) => event.data)).toEqual(["one", "two"]);
  });

  it("parses a frame split across three chunks", () => {
    const parser = new SSEParser();
    const chunks = [
      bytes("data: {\"text\""),
      bytes(":\"hel"),
      bytes("lo\"}\n\n"),
    ];

    const events = chunks.flatMap((chunk) => parser.feed(chunk));
    expect(events[0].data).toBe('{"text":"hello"}');
  });

  it("ignores comments and event fields", () => {
    const parser = new SSEParser();
    const events = parser.feed(bytes(": keepalive\n\nevent: message\ndata: value\n\n"));

    expect(events.map((event) => event.data)).toEqual(["value"]);
  });

  it("supports CRLF and DONE followed by another frame", () => {
    const parser = new SSEParser();
    const events = parser.feed(bytes("data: [DONE]\r\n\r\ndata: trailing\r\n\r\n"));

    expect(events.map((event) => event.data)).toEqual(["[DONE]", "trailing"]);
  });

  it("rejects non-SSE terminal bytes", () => {
    expect(() => new SSEParser().feed(bytes("not an SSE frame"), true)).toThrow(
      "malformed SSE frame",
    );
  });
});
