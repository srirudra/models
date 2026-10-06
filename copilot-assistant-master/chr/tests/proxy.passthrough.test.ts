import http from "node:http";
import https from "node:https";
import net from "node:net";
import tls from "node:tls";
import { describe, expect, it, afterEach } from "vitest";
import { createEphemeralCa } from "../src/intercept/ca.js";
import { startInterceptProxy, type InterceptProxy } from "../src/intercept/proxy.js";

const upstreamCa = createEphemeralCa();
const upstreamLeaf = upstreamCa.issueLeaf("localhost");

let upstream: https.Server | undefined;
let proxy: InterceptProxy | undefined;

afterEach(async () => {
  await proxy?.close();
  proxy = undefined;
  await new Promise<void>((resolve) => upstream?.close(() => resolve()) ?? resolve());
  upstream = undefined;
});

async function startUpstream(): Promise<number> {
  upstream = https.createServer(
    { key: upstreamLeaf.key, cert: upstreamLeaf.cert },
    (req, res) => {
      if (req.url === "/json") {
        const body = Buffer.from('{"message":"passthrough"}');
        res.writeHead(200, { "content-type": "application/json", "content-length": body.length, "x-upstream": "yes" });
        res.end(body);
        return;
      }
      if (req.url === "/events") {
        res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache" });
        const chunks = ["data: one\n\n", "data: two\n\n", "data: [DONE]\n\n"];
        let index = 0;
        const timer = setInterval(() => {
          res.write(chunks[index++]);
          if (index === chunks.length) {
            clearInterval(timer);
            res.end();
          }
        }, 20);
        return;
      }
      res.writeHead(404).end();
    },
  );
  await new Promise<void>((resolve) => upstream!.listen(0, "127.0.0.1", () => resolve()));
  return (upstream.address() as net.AddressInfo).port;
}

async function connect(proxyPort: number, targetPort: number, ca: string, servername = "localhost"): Promise<tls.TLSSocket> {
  const socket = net.connect(proxyPort, "127.0.0.1");
  await new Promise<void>((resolve, reject) => {
    socket.once("error", reject);
    socket.once("connect", resolve);
  });
  socket.write(`CONNECT ${servername}:${targetPort} HTTP/1.1\r\nHost: ${servername}:${targetPort}\r\n\r\n`);
  let response = "";
  await new Promise<void>((resolve, reject) => {
    const onData = (chunk: Buffer) => {
      response += chunk.toString();
      if (response.includes("\r\n\r\n")) {
        socket.off("data", onData);
        resolve();
      }
    };
    socket.on("data", onData);
    socket.once("error", reject);
  });
  expect(response).toContain("200 Connection Established");
  const secure = tls.connect({ socket, ca, servername, ALPNProtocols: ["http/1.1"] });
  await new Promise<void>((resolve, reject) => {
    secure.once("secureConnect", resolve);
    secure.once("error", reject);
  });
  return secure;
}

async function request(socket: tls.TLSSocket, path: string, headers: Record<string, string> = {}): Promise<{ body: Buffer; headers: http.IncomingHttpHeaders; chunks: number }> {
  return await new Promise((resolve, reject) => {
    const req = https.request({ host: "127.0.0.1", path, method: "GET", createConnection: () => socket, headers }, (res) => {
      const parts: Buffer[] = [];
      let chunks = 0;
      res.on("data", (part: Buffer) => { parts.push(part); chunks++; });
      res.on("end", () => resolve({ body: Buffer.concat(parts), headers: res.headers, chunks }));
    });
    req.once("error", reject);
    req.end();
  });
}

describe("intercept proxy passthrough", () => {
  it("forwards JSON and streams SSE while redacting exchange headers", async () => {
    const upstreamPort = await startUpstream();
    const proxyCa = createEphemeralCa();
    const exchanges: unknown[] = [];
    proxy = await startInterceptProxy({
      ca: proxyCa,
      mitmHost: (host) => host === "localhost",
      upstreamPort,
      upstreamTls: { ca: upstreamCa.caPem },
      onExchange: (meta) => exchanges.push(meta),
    });
    const json = await request(await connect(proxy.port, upstreamPort, proxyCa.caPem, "localhost"), "/json", {
      authorization: "Bearer super-secret-token",
      cookie: "session=secret-cookie",
    });
    expect(json.body.toString()).toBe('{"message":"passthrough"}');
    expect(json.headers["x-upstream"]).toBe("yes");
    expect(JSON.stringify(exchanges)).not.toContain("super-secret-token");
    expect(JSON.stringify(exchanges)).not.toContain("secret-cookie");

    const socket = await connect(proxy.port, upstreamPort, proxyCa.caPem, "localhost");
    const sse = await request(socket, "/events");
    expect(sse.body.toString()).toBe("data: one\n\ndata: two\n\ndata: [DONE]\n\n");
    expect(sse.chunks).toBeGreaterThan(1);
  });

  it("blind-tunnels hosts that are not allow-listed", async () => {
    const upstreamPort = await startUpstream();
    proxy = await startInterceptProxy({ ca: createEphemeralCa(), mitmHost: () => false });
    const result = await request(await connect(proxy.port, upstreamPort, upstreamCa.caPem, "localhost"), "/json");
    expect(result.body.toString()).toBe('{"message":"passthrough"}');
  });

  it("closes its listening port", async () => {
    proxy = await startInterceptProxy({ ca: createEphemeralCa(), mitmHost: () => false });
    const port = proxy.port;
    await proxy.close();
    proxy = undefined;
    await expect(new Promise<void>((resolve, reject) => {
      const socket = net.connect(port, "127.0.0.1");
      socket.once("connect", () => { socket.destroy(); reject(new Error("port still open")); });
      socket.once("error", () => resolve());
    })).resolves.toBeUndefined();
  });
});
