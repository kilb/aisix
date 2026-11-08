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
 *
 * `pause()` makes the control plane unreachable without refusing anything:
 * the relay keeps accepting connections and holds every link, forwarding
 * nothing in either direction. A connection accepted while paused gets no
 * upstream side until `resume()`. A link whose client closes while paused is
 * torn down at once, upstream side included, so what it held never reaches
 * the control plane: a request the gateway already gave up on stays
 * undelivered. `resume()` then forwards what every remaining link held, in
 * the order it arrived. The relay works at the TCP level, so it cannot read
 * or forge a TLS answer.
 */
export interface SwallowRelay {
  port: number;
  swallowNextResponse(): void;
  /** Hold every connection, new ones included, without forwarding anything. */
  pause(): void;
  /** Forward what every link still open held, then relay as usual. */
  resume(): void;
  stats(): { connectionsOpened: number; swallowed: number };
  close(): Promise<void>;
}

interface Link {
  client: Socket;
  /** `undefined` while a connection accepted during a pause waits for `resume()`. */
  server: Socket | undefined;
  /** Complete exchanges seen: client bytes followed by server bytes. */
  exchanges: number;
  /** Client bytes arrived since the last server bytes. */
  clientSpoke: boolean;
  /** Eligible for the armed swallow: had an exchange when it was armed. */
  armed: boolean;
  /** The armed swallow chose this link: its next server bytes are dropped. */
  swallowing: boolean;
  /** What arrived while paused, in order; `up` is client to server. */
  held: { up: boolean; chunk: Buffer }[];
  /** Tears down both ends. */
  end(): void;
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
  let paused = false;

  const fromClient = (link: Link, chunk: Buffer) => {
    if (pending && link.armed && !link.clientSpoke && !link.swallowing) {
      pending = false;
      link.swallowing = true;
      for (const other of links) other.armed = false;
    }
    link.clientSpoke = true;
    link.server!.write(chunk);
  };
  const fromServer = (link: Link, chunk: Buffer) => {
    if (link.swallowing) {
      swallowed++;
      link.end();
      return;
    }
    if (link.clientSpoke) {
      link.exchanges++;
      link.clientSpoke = false;
    }
    link.client.write(chunk);
  };
  const openUpstream = (link: Link) => {
    const upstream = connect(targetPort, host);
    link.server = upstream;
    upstream.on("error", link.end);
    upstream.on("close", link.end);
    upstream.on("data", (chunk: Buffer) => {
      if (paused) link.held.push({ up: false, chunk });
      else fromServer(link, chunk);
    });
  };

  const server = createServer((client) => {
    connectionsOpened++;
    const link: Link = {
      client,
      server: undefined,
      exchanges: 0,
      clientSpoke: false,
      armed: false,
      swallowing: false,
      held: [],
      end: () => {
        links.delete(link);
        client.destroy();
        link.server?.destroy();
      },
    };
    links.add(link);
    client.on("error", link.end);
    client.on("close", link.end);
    client.on("data", (chunk: Buffer) => {
      if (paused) link.held.push({ up: true, chunk });
      else fromClient(link, chunk);
    });
    if (!paused) openUpstream(link);
  });

  const port = await pickFreePort();
  await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", resolve));

  return {
    port,
    swallowNextResponse() {
      pending = true;
      for (const link of links) link.armed = link.exchanges >= 1;
    },
    pause() {
      paused = true;
    },
    resume() {
      paused = false;
      for (const link of [...links]) {
        if (link.server === undefined) openUpstream(link);
        const held = link.held;
        link.held = [];
        for (const { up, chunk } of held) {
          // A swallowed answer ends the link part way through.
          if (!links.has(link)) break;
          if (up) fromClient(link, chunk);
          else fromServer(link, chunk);
        }
      }
    },
    stats: () => ({ connectionsOpened, swallowed }),
    async close() {
      for (const link of links) {
        link.client.destroy();
        link.server?.destroy();
      }
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}
