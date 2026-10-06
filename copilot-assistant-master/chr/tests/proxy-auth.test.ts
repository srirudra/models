import net from "node:net";
import https from "node:https";
import tls from "node:tls";
import { afterEach, describe, expect, it } from "vitest";
import { createEphemeralCa } from "../src/intercept/ca.js";
import { startInterceptProxy, type InterceptProxy } from "../src/intercept/proxy.js";

let proxy: InterceptProxy | undefined;
let upstream: net.Server | undefined;

afterEach(async () => {
  await proxy?.close();
  proxy = undefined;
  await new Promise<void>((resolve) => upstream?.close(() => resolve()) ?? resolve());
  upstream = undefined;
});

function response(socket: net.Socket): Promise<string> {
  return new Promise((resolve, reject) => {
    let value = "";
    const onData = (chunk: Buffer) => {
      value += chunk.toString();
      if (value.includes("\r\n\r\n")) {
        socket.off("data", onData);
        resolve(value);
      }
    };
    socket.on("data", onData);
    socket.once("error", reject);
  });
}

async function port(server: net.Server): Promise<number> {
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  return (server.address() as net.AddressInfo).port;
}

describe("intercept proxy authentication", () => {
  it("rejects missing and wrong credentials without connecting upstream", async () => {
    let connections = 0;
    upstream = net.createServer(() => { connections++; });
    const upstreamPort = await port(upstream);
    proxy = await startInterceptProxy({
      ca: createEphemeralCa(),
      proxyAuthToken: "correct",
      mitmHost: () => false,
    });

    for (const authorization of ["", "Basic d3Jvbmc6d3Jvbmc="]) {
      const socket = net.connect(proxy.port, "127.0.0.1");
      await new Promise<void>((resolve) => socket.once("connect", resolve));
      socket.write(`CONNECT 127.0.0.1:${upstreamPort} HTTP/1.1\r\nHost: 127.0.0.1:${upstreamPort}\r\n${authorization ? `Proxy-Authorization: ${authorization}\r\n` : ""}\r\n`);
      await expect(response(socket)).resolves.toContain("407 Proxy Authentication Required");
      socket.destroy();
    }
    expect(connections).toBe(0);
  });

  it("allows a correctly authenticated tunnel", async () => {
    upstream = net.createServer((socket) => setTimeout(() => socket.write("hello"), 25));
    const upstreamPort = await port(upstream);
    proxy = await startInterceptProxy({
      ca: createEphemeralCa(),
      proxyAuthToken: "correct",
      mitmHost: () => false,
    });
    const socket = net.connect(proxy.port, "127.0.0.1");
    await new Promise<void>((resolve) => socket.once("connect", resolve));
    socket.write(`CONNECT 127.0.0.1:${upstreamPort} HTTP/1.1\r\nHost: 127.0.0.1:${upstreamPort}\r\nProxy-Authorization: Basic ${Buffer.from("chr:correct").toString("base64")}\r\n\r\n`);
    await expect(response(socket)).resolves.toContain("200 Connection Established");
    await expect(new Promise<string>((resolve) => socket.once("data", (chunk) => resolve(chunk.toString())))).resolves.toContain("hello");
    socket.destroy();
  });

  it("survives abrupt resets on blind and MITM connections", async () => {
    const upstreamCa = createEphemeralCa();
    const upstreamLeaf = upstreamCa.issueLeaf("localhost");
    const upstreamSockets: net.Socket[] = [];
    upstream = net.createServer((socket) => upstreamSockets.push(socket));
    const blindPort = await port(upstream);
    const mitmUpstream = https.createServer(
      { key: upstreamLeaf.key, cert: upstreamLeaf.cert },
      (_req, res) => res.end("ok"),
    );
    mitmUpstream.on("connection", (socket) => upstreamSockets.push(socket));
    const mitmPort = await port(mitmUpstream);
    const proxyCa = createEphemeralCa();
    proxy = await startInterceptProxy({
      ca: proxyCa,
      proxyAuthToken: "correct",
      mitmHost: (host) => host === "localhost",
      upstreamPort: mitmPort,
      upstreamTls: { ca: upstreamCa.caPem },
    });
    const auth = `Proxy-Authorization: Basic ${Buffer.from("chr:correct").toString("base64")}`;
    const connect = async (target: string): Promise<net.Socket> => {
      const socket = net.connect(proxy!.port, "127.0.0.1");
      await new Promise<void>((resolve) => socket.once("connect", resolve));
      socket.write(`CONNECT ${target} HTTP/1.1\r\nHost: ${target}\r\n${auth}\r\n\r\n`);
      await expect(response(socket)).resolves.toContain("200 Connection Established");
      return socket;
    };
    const uncaught: Error[] = [];
    const onUncaught = (error: Error) => uncaught.push(error);
    process.once("uncaughtException", onUncaught);
    const blind = await connect(`127.0.0.1:${blindPort}`);
    const mitm = await connect(`localhost:${mitmPort}`);
    const secure = tls.connect({ socket: mitm, ca: proxyCa.caPem, servername: "localhost" });
    secure.on("error", () => undefined);
    await new Promise<void>((resolve) => secure.once("secureConnect", resolve));
    blind.resetAndDestroy();
    mitm.resetAndDestroy();
    for (const socket of upstreamSockets) socket.resetAndDestroy();
    await new Promise((resolve) => setTimeout(resolve, 50));
    const next = await connect(`127.0.0.1:${blindPort}`);
    next.resetAndDestroy();
    process.removeListener("uncaughtException", onUncaught);
    expect(uncaught).toEqual([]);
  }, 10000);
});
