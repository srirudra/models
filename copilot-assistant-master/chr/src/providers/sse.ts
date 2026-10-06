export type SSEEvent = {
  data: string;
  raw: string;
};

export class SSEParser {
  private decoder = new TextDecoder();
  private buffer = "";

  feed(chunk: Uint8Array, final = false): SSEEvent[] {
    this.buffer += this.decoder.decode(chunk, { stream: !final });
    const events: SSEEvent[] = [];

    while (true) {
      const separator = this.findSeparator();
      if (separator < 0) {
        break;
      }

      const end = separator + this.separatorLength(separator);
      const block = this.buffer.slice(0, separator);
      this.buffer = this.buffer.slice(end);
      const event = this.parseBlock(block);
      if (event) {
        events.push(event);
      }
    }

    if (final) {
      this.buffer += this.decoder.decode();
      if (this.buffer.length > 0) {
        const event = this.parseBlock(this.buffer);
        this.buffer = "";
        if (event) {
          events.push(event);
        }
      }
    }

    return events;
  }

  private findSeparator(): number {
    const lf = this.buffer.indexOf("\n\n");
    const crlf = this.buffer.indexOf("\r\n\r\n");
    if (lf < 0) {
      return crlf;
    }
    if (crlf < 0) {
      return lf;
    }
    return Math.min(lf, crlf);
  }

  private separatorLength(index: number): number {
    return this.buffer.startsWith("\r\n\r\n", index) ? 4 : 2;
  }

  private parseBlock(block: string): SSEEvent | undefined {
    const lines = block.split(/\r\n|\n|\r/);
    const dataLines: string[] = [];

    for (const line of lines) {
      if (line.startsWith(":") || line.startsWith("event:")) {
        continue;
      }
      if (line.startsWith("data:")) {
        dataLines.push(line.startsWith("data: ") ? line.slice(6) : line.slice(5));
        continue;
      }
      if (line.trim() !== "") {
        throw new Error("malformed SSE frame");
      }
    }

    if (dataLines.length === 0) {
      return undefined;
    }

    const raw = `${dataLines.map((line) => `data: ${line}`).join("\n")}\n\n`;
    return {
      data: dataLines.join("\n"),
      raw,
    };
  }
}
