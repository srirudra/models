import http from "node:http";
import { ConfigRevision, Provider } from "../config/types.js";
import { resolveRoute } from "../routing/resolve.js";
import { ForwardError, forwardSSE } from "../providers/forward.js";

export type RecordingSink = {
  routeDecision(event: Record<string, string | number>): void;
};

const noop: RecordingSink = {
  routeDecision: () => undefined,
};

const maxBodyBytes = 8 * 1024 * 1024;
const maxConcurrentRequests = 32;
let activeRequests = 0;

function json(res: http.ServerResponse, status: number, value: unknown): void {
  if (!res.headersSent) {
    res.writeHead(status, { "content-type": "application/json" });
  }
  res.end(JSON.stringify(value));
}

async function readBody(req: http.IncomingMessage): Promise<Record<string, unknown>> {
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const chunk of req) {
    const value = Buffer.from(chunk);
    size += value.byteLength;
    if (size > maxBodyBytes) {
      throw new Error("request body exceeds 8 MiB limit");
    }
    chunks.push(value);
  }
  const parsed: unknown = JSON.parse(Buffer.concat(chunks).toString("utf8"));
  if (!parsed || typeof parsed !== "object") {
    throw new Error("request body must be a JSON object");
  }
  const envelope = parsed as Record<string, unknown>;
  const body = envelope.body;
  if (body && typeof body === "object" && !Array.isArray(body)) {
    return body as Record<string, unknown>;
  }
  return envelope;
}

function writeBlockResponse(res: http.ServerResponse): void {
  json(res, 200, {
    id: "chr-blocked",
    object: "chat.completion",
    choices: [
      {
        index: 0,
        message: { role: "assistant", content: "" },
        finish_reason: "stop",
      },
    ],
  });
}

function writeStream(
  res: http.ServerResponse,
  chunk: Uint8Array,
  signal: AbortSignal,
): Promise<void> {
  if (res.write(chunk)) {
    return Promise.resolve();
  }
  return new Promise((resolve, reject) => {
    const onDrain = () => {
      cleanup();
      resolve();
    };
    const onError = (error: Error) => {
      cleanup();
      reject(error);
    };
    const onAbort = () => {
      cleanup();
      reject(signal.reason ?? new Error("client disconnected"));
    };
    const cleanup = () => {
      res.removeListener("drain", onDrain);
      res.removeListener("error", onError);
      signal.removeEventListener("abort", onAbort);
    };
    res.once("drain", onDrain);
    res.once("error", onError);
    signal.addEventListener("abort", onAbort, { once: true });
    if (signal.aborted) {
      onAbort();
    }
  });
}

export function createServer(
  revision: ConfigRevision,
  sink: RecordingSink = noop,
): http.Server {
  return http.createServer(async (req, res) => {
    if (++activeRequests > maxConcurrentRequests) {
      --activeRequests;
      json(res, 429, {
        error: {
          message: "concurrency limit exceeded",
          type: "rate_limit_error",
        },
      });
      return;
    }

    const abort = new AbortController();
    const onAborted = () => abort.abort(new Error("client disconnected"));
    let responseFinished = false;
    // A close before finish is a premature disconnect; finish marks a normal response completion.
    const onFinished = () => {
      responseFinished = true;
    };
    const onClosed = () => {
      if (!responseFinished) {
        onAborted();
      }
    };
    req.once("aborted", onAborted);
    res.once("finish", onFinished);
    res.once("close", onClosed);

    try {
      const path = new URL(req.url ?? "/", "http://127.0.0.1").pathname;
      if (req.method === "GET" && path === "/v1/models") {
        const data = revision.models.map((model) => ({
          id: model.alias,
          object: "model",
          owned_by: model.provider,
        }));
        json(res, 200, { object: "list", data });
        return;
      }
      if (
        req.method !== "POST" ||
        !["/v1/chat/completions", "/v1/responses"].includes(path)
      ) {
        res.writeHead(404);
        res.end();
        return;
      }

      const body = await readBody(req);
      const model = typeof body.model === "string" ? body.model : "";
      const route = resolveRoute(model, revision, body);
      sink.routeDecision({
        requestedModel: model,
        route: route.kind,
        revisionId: revision.revisionId,
      });

      if (route.kind === "auxiliary" && route.policy === "block") {
        writeBlockResponse(res);
        return;
      }
      if (route.kind === "auxiliary" && route.policy === "allow") {
        writeBlockResponse(res);
        return;
      }
      if (route.kind === "github-leg" || route.kind === "error") {
        json(res, 400, {
          error: {
            message: route.kind === "error"
              ? route.message
              : "GitHub leg is fail-closed",
            type: "invalid_request_error",
            code: route.kind,
          },
        });
        return;
      }

      const provider: Provider | undefined = route.provider
        ? revision.providers[route.provider]
        : undefined;
      if (!provider) {
        throw new Error(`unknown provider: ${route.provider}`);
      }
      const expectedProtocol = path === "/v1/responses"
        ? "openai-responses"
        : "openai-chat-completions";
      if (provider.protocol !== expectedProtocol) {
        throw new Error(`protocol mismatch: ${provider.protocol} cannot serve ${path}`);
      }
      const rewritten = {
        ...body,
        model: route.kind === "custom" ? route.upstreamModel : model,
      };
      res.writeHead(200, {
        "content-type": "text/event-stream",
        "cache-control": "no-cache",
        connection: "keep-alive",
      });
      res.flushHeaders();
      await forwardSSE(
        provider,
        path,
        rewritten,
        abort.signal,
        (chunk) => writeStream(res, chunk, abort.signal),
      );
      if (!res.writableEnded) {
        res.end();
      }
    } catch (error) {
      if (res.writableEnded || res.destroyed) {
        return;
      }
      const status = error instanceof Error && error.message.includes("8 MiB")
        ? 413
        : error instanceof ForwardError && error.code.endsWith("timeout")
          ? 504
          : 400;
      json(res, status, {
        error: {
          message: error instanceof Error ? error.message : "request failed",
          type: "invalid_request_error",
          code: error instanceof ForwardError ? error.code : undefined,
        },
      });
    } finally {
      req.removeListener("aborted", onAborted);
      res.removeListener("finish", onFinished);
      res.removeListener("close", onClosed);
      --activeRequests;
    }
  });
}

export function listen(server: http.Server, port = 0): Promise<number> {
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", () => {
      resolve((server.address() as { port: number }).port);
    });
  });
}
