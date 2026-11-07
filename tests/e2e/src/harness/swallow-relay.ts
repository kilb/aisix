import { createServer, connect, type Socket } from "node:net";

import { pickFreePort } from "./ports.js";

/**
 * A TCP relay in front of the control plane's gateway listener that can lose
 * one answer on purpose: the request reaches the control plane, which stores
 * it, and its answer never reaches the gateway, which is what a timeout or a
 * dropped connection looks like from the gateway's side. TLS passes through
 * untouched, so the gateway still talks mTLS to the real control plane.
 *
 * `swallowNextResponse()` arms only connections that already carried at least
 * one complete exchange (client bytes, then server bytes) when it is called,
 * and then send client bytes again: a connection the gateway reuses from its
 * pool. On the first such connection it lets those client bytes through,
 * drops the next server bytes and destroys both ends. A connection opened
 * after the call is never armed, so a gateway that opened a fresh one shows
 * up as `swallowed == 0`.
 */
export interface SwallowRelay {
  port: number;
  swallowNextResponse(): void;
  stats(): { connectionsOpened: number; swallowed: number };
  close(): Promise<void>;
}

interface Link {
  client: Socket;
  server: Socket;
  /** Complete exchanges seen: client bytes followed by server bytes. */
  exchanges: number;
  /** Client bytes arrived since the last server bytes. */
  clientSpoke: boolean;
  /** Eligible for the armed swallow: had an exchange when it was armed. */
  armed: boolean;
  /** The armed swallow chose this link: its next server bytes are dropped. */
  swallowing: boolean;
}

/** `target` is `host:port` of the listener to relay to. */
export async function startSwallowRelay(target: string): Promise<SwallowRelay> {
  const at = target.lastIndexOf(":");
  const host = target.slice(0, at);
  const targetPort = Number(target.slice(at + 1));
  const links = new Set<Link>();
  let connectionsOpened = 0;
  let swallowed = 0;
  let pending = false;

  const server = createServer((client) => {
    connectionsOpened++;
    const upstream = connect(targetPort, host);
    const link: Link = {
      client,
      server: upstream,
      exchanges: 0,
      clientSpoke: false,
      armed: false,
      swallowing: false,
    };
    links.add(link);
    const end = () => {
      links.delete(link);
      client.destroy();
      upstream.destroy();
    };
    client.on("error", end);
    upstream.on("error", end);
    client.on("close", end);
    upstream.on("close", end);

    client.on("data", (chunk: Buffer) => {
      if (pending && link.armed && !link.clientSpoke && !link.swallowing) {
        pending = false;
        link.swallowing = true;
        for (const other of links) other.armed = false;
      }
      link.clientSpoke = true;
      upstream.write(chunk);
    });
    upstream.on("data", (chunk: Buffer) => {
      if (link.swallowing) {
        swallowed++;
        end();
        return;
      }
      if (link.clientSpoke) {
        link.exchanges++;
        link.clientSpoke = false;
      }
      client.write(chunk);
    });
  });

  const port = await pickFreePort();
  await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", resolve));

  return {
    port,
    swallowNextResponse() {
      pending = true;
      for (const link of links) link.armed = link.exchanges >= 1;
    },
    stats: () => ({ connectionsOpened, swallowed }),
    async close() {
      for (const link of links) {
        link.client.destroy();
        link.server.destroy();
      }
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}
