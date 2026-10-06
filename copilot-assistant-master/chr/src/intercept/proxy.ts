import http from "node:http";
import https from "node:https";
import net from "node:net";
import tls from "node:tls";
import crypto from "node:crypto";
import type { EphemeralCa } from "./ca.js";

export const defaultMitmHost = (host: string): boolean =>
  /^api\.[^.]+\.githubcopilot\.com$/i.test(host) || /^api\.githubcopilot\.com$/i.test(host);

export type ExchangeMeta = {
  method: string;
  host: string;
  path: string;
  status?: number;
  headers: Record<string, string>;
};

export type InterceptProxyOptions = {
  ca: EphemeralCa;
  mitmHost?: (host: string) => boolean;
  handleRequest?: (req: http.IncomingMessage, res: http.ServerResponse, ctx: { host: string }) => void;
  onExchange?: (meta: ExchangeMeta) => void;
  bodyLimitBytes?: number;
  /** Test seam; production leaves this undefined and validates public certificates. */
  upstreamPort?: number;
  upstreamTls?: Pick<https.RequestOptions, "ca" | "rejectUnauthorized">;
  proxyAuthToken?: string;
};

export type InterceptProxy = { port: number; close(): Promise<void> };

function safeHeaderValue(name: string, value: string): string {
  return /authorization|cookie|set-cookie|token|api-key/i.test(name)
    ? `${value.slice(0, 6)}...`
    : value;
}

function exchangeHeaders(headers: http.IncomingHttpHeaders): Record<string, string> {
  return Object.fromEntries(Object.entries(headers).map(([name, value]) => [
    name,
    safeHeaderValue(name, Array.isArray(value) ? value.join(", ") : value ?? ""),
  ]));
}

function proxyCredential(token: string): Buffer {
  return Buffer.from(`Basic ${Buffer.from(`chr:${token}`).toString("base64")}`);
}

function hasValidProxyAuth(request: http.IncomingMessage, expected: Buffer): boolean {
  const actual = request.headers["proxy-authorization"];
  if (typeof actual !== "string") return false;
  const bytes = Buffer.from(actual);
  return bytes.length === expected.length && crypto.timingSafeEqual(bytes, expected);
}

function parseConnectTarget(target: string): { host: string; port: number } | undefined {
  const match = target.match(/^\[([^\]]+)\]:(\d+)$/) ?? target.match(/^([^:]+):(\d+)$/);
  if (!match || !match[1] || /\s/.test(match[1])) return undefined;
  const host = match[1];
  const validIp = net.isIP(host) !== 0;
  const validName = host.split(".").every((label) => /^[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?$/.test(label));
  if (!validIp && !validName) return undefined;
  const port = Number(match[2]);
  if (!Number.isInteger(port) || port < 1 || port > 65535) return undefined;
  return { host, port };
}

type ErrorSocket = {
  once(event: "error", listener: () => void): unknown;
  destroyed: boolean;
  destroy(): void;
};

function handleSocketError(socket: ErrorSocket, peer?: ErrorSocket): void {
  socket.once("error", () => {
    if (peer && !peer.destroyed) peer.destroy();
    if (!socket.destroyed) socket.destroy();
  });
}

function defaultHandler(options: InterceptProxyOptions, host: string, req: http.IncomingMessage, res: http.ServerResponse): void {
  const headers = { ...req.headers };
  delete headers.connection;
  delete headers["proxy-connection"];
  const upstream = https.request({
    hostname: host,
    port: options.upstreamPort ?? 443,
    servername: host,
    ...( { ALPNProtocols: ["http/1.1"] } as tls.ConnectionOptions),
    method: req.method,
    path: req.url,
    headers,
    ...options.upstreamTls,
  }, (upstreamResponse) => {
    res.writeHead(upstreamResponse.statusCode ?? 502, upstreamResponse.headers);
    upstreamResponse.pipe(res);
    options.onExchange?.({
      method: req.method ?? "GET",
      host,
      path: req.url ?? "/",
      status: upstreamResponse.statusCode,
      headers: {
        ...exchangeHeaders(req.headers),
        ...exchangeHeaders(upstreamResponse.headers),
      },
    });
  });
  handleSocketError(upstream);
  req.once("error", () => upstream.destroy());
  res.once("error", () => upstream.destroy());
  upstream.once("error", () => {
    if (!res.headersSent) res.writeHead(502, { "content-type": "text/plain" });
    res.end("Bad Gateway");
  });
  req.pipe(upstream);
}

export async function startInterceptProxy(options: InterceptProxyOptions): Promise<InterceptProxy> {
  const sockets = new Set<net.Socket>();
  const servers = new Set<http.Server>();
  const mitmHost = options.mitmHost ?? defaultMitmHost;
  const limit = options.bodyLimitBytes ?? 32 * 1024 * 1024;
  const expectedProxyAuth = options.proxyAuthToken === undefined
    ? undefined
    : proxyCredential(options.proxyAuthToken);
  const server = http.createServer();
  server.on("request", (request, response) => {
    handleSocketError(request.socket);
    response.once("error", () => request.socket.destroy());
    if (expectedProxyAuth && !hasValidProxyAuth(request, expectedProxyAuth)) {
      response.writeHead(407, { "Proxy-Authenticate": 'Basic realm="chr"', connection: "close" });
      response.end();
      request.socket.destroy();
      return;
    }
    response.writeHead(501, { connection: "close" });
    response.end("plain HTTP proxying is not supported");
    request.socket.destroy();
  });
  servers.add(server);
  server.on("connect", (request, client, head) => {
    handleSocketError(client);
    const target = request.url ?? "";
    sockets.add(client as unknown as net.Socket);
    client.once("close", () => sockets.delete(client as unknown as net.Socket));
    const parsed = parseConnectTarget(target);
    if (!parsed) {
      client.end("HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n");
      return;
    }
    if (expectedProxyAuth && !hasValidProxyAuth(request, expectedProxyAuth)) {
      client.end('HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm="chr"\r\nConnection: close\r\n\r\n');
      return;
    }
    const { host, port } = parsed;
    if (!mitmHost(host)) {
      const upstream = net.connect(port, host);
      handleSocketError(upstream, client);
      sockets.add(upstream);
      upstream.once("close", () => sockets.delete(upstream));
      upstream.once("connect", () => {
        client.write("HTTP/1.1 200 Connection Established\r\n\r\n");
        if (head.length) upstream.write(head);
        client.pipe(upstream);
        upstream.pipe(client);
      });
      return;
    }
    client.write("HTTP/1.1 200 Connection Established\r\n\r\n");
    const secureContext = tls.createSecureContext(options.ca.issueLeaf(host));
    const secure = new tls.TLSSocket(client, {
      isServer: true,
      secureContext,
      ALPNProtocols: ["http/1.1"],
      SNICallback: (servername, callback) => {
        try {
          callback(null, tls.createSecureContext(options.ca.issueLeaf(servername || host)));
        } catch (error) {
          callback(error as Error);
        }
      },
    });
    handleSocketError(secure);
    sockets.add(secure);
    secure.once("close", () => sockets.delete(secure));
    const requestServer = http.createServer((req, res) => {
      let received = 0;
      req.on("data", (chunk: Buffer) => {
        received += chunk.length;
        if (received > limit) {
          res.writeHead(413);
          res.end("request body too large");
          req.destroy();
        }
      });
      if (options.handleRequest) options.handleRequest(req, res, { host });
      else defaultHandler(options, host, req, res);
    });
    requestServer.on("clientError", (_error, socket) => {
      handleSocketError(socket);
      socket.destroy();
    });
    servers.add(requestServer);
    requestServer.once("close", () => servers.delete(requestServer));
    secure.once("secure", () => {
      requestServer.emit("connection", secure as unknown as net.Socket);
      if (head.length) secure.unshift(head);
    });
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.on("error", () => undefined);
    server.listen(0, "127.0.0.1", () => resolve());
  });
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("proxy did not bind a TCP port");
  return {
    port: address.port,
    async close() {
      for (const socket of sockets) {
        try {
          socket.destroy();
        } catch {
          // Teardown must remain best-effort after a peer reset.
        }
      }
      for (const item of servers) {
        await new Promise<void>((resolve) => {
          try {
            item.close(() => resolve());
          } catch {
            resolve();
          }
        });
      }
    },
  };
}
