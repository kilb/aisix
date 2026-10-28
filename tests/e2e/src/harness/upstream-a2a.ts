import {
  createServer,
  type IncomingMessage,
  type Server as HttpServer,
  type ServerResponse,
} from "node:http";

/** One request the stub agent received, as the gateway sent it. */
export interface A2aReceivedRequest {
  httpMethod: string;
  path: string;
  /** The `A2A-Version` header, or `null` when the gateway sent none. */
  version: string | null;
  authorization: string | null;
  apiKey: string | null;
  body?: Record<string, unknown>;
}

/**
 * Where the agent publishes its card.
 *
 * - `origin` — at the RFC 8615 origin URI, the shape of an agent that owns its
 *   whole domain. This is the only shape the gateway used to be able to reach.
 * - `path` — under the service endpoint's own path prefix, with every other
 *   path answering `405` from a catch-all. This is the shape of any platform
 *   that multiplexes tenants under a prefix, and of a self-hosted agent behind
 *   an ingress path (api7/aisix#913).
 */
export type A2aCardMount = "origin" | "path";

/**
 * Which wire shape the agent answers in.
 *
 * - `0.3` — the Task / update event sits directly at `result`, tagged with a
 *   `kind` discriminator.
 * - `1.0` — the same object is wrapped in the response's `oneof payload`,
 *   which protobuf JSON renders as the set field's own name (`task`,
 *   `statusUpdate`). This is the shape a 1.0 agent actually returns, so a test
 *   that never uses it cannot see a reader that only understands 0.3.
 */
export type A2aWireShape = "0.3" | "1.0";

export interface A2aUpstream {
  /** Register this as the agent's `url`: the A2A service endpoint. */
  url: string;
  /** Every request the agent received, in arrival order. */
  requests: A2aReceivedRequest[];
  close(): Promise<void>;
}

export interface A2aUpstreamOptions {
  cardMount?: A2aCardMount;
  /** When set, the agent is registered with `auth_type: bearer` and this token. */
  token?: string;
  /** Wire shape of the results this agent returns. Defaults to `0.3`. */
  wireShape?: A2aWireShape;
  /**
   * Stream an actual answer: [`STREAM_ANSWER`] as two continued artifact
   * chunks with a progress note between them and a closing statement — the
   * shape a reference agent produces. Off by default, because it changes the
   * event sequence the streaming-relay suite counts.
   */
  streamAnswer?: boolean;
  /**
   * Stream this answer instead of [`STREAM_ANSWER`]: each entry is one
   * continued artifact chunk — a string is a single text part, an array is
   * the chunk's parts as written — with the same progress note and closing
   * statement around them. Implies `streamAnswer`.
   */
  streamChunks?: Array<string | unknown[]>;
  /**
   * Answer a streamed call with this one Message and nothing else, the way an
   * agent that replies without creating a task does.
   */
  streamMessage?: string;
  /** Answer a streamed call with this one JSON-RPC `error` event and nothing else. */
  streamError?: Record<string, unknown>;
  /**
   * Leave the connection open after the final event, the way some agents do:
   * a client is expected to stop reading at the final event, not wait for EOF.
   */
  holdStreamOpen?: boolean;
  /** Text of the agent's answer to a unary call. Defaults to a one-liner. */
  unaryAnswer?: string;
  /**
   * Further fields of the Task every unary call answers with — `history`,
   * `artifacts`, or a `status` replacing the default one — written already in
   * this agent's wire shape. Omitted, the Task carries only its status message.
   */
  unaryTask?: Record<string, unknown>;
  /**
   * Answer every unary call with this JSON-RPC `error` member instead of a
   * result. Omitted, unary calls answer with the Task.
   */
  unaryError?: Record<string, unknown>;
}

/** The id of the Task every unary call answers with, so a test can read it back. */
export const UNARY_TASK_ID = "task-e2e-1";

/**
 * 1.0 methods whose response is the bare `Task`, with no payload wrapper
 * (`rpc GetTask(GetTaskRequest) returns (Task)`, likewise `CancelTask`).
 */
const V1_BARE_TASK_METHODS = new Set(["GetTask", "CancelTask"]);

const PATH_PREFIX = "/v3/agents/serve/tenant-42";

/**
 * Pause between streamed events. Sized so the arrival spread a relayed stream
 * produces dwarfs the noise a buffered one would show: a client does not begin
 * reading the instant the headers land (connection setup and the fetch
 * implementation's own buffering cost tens of ms), which compresses the
 * measured spread. A small gap leaves that compression the same order as the
 * signal; this makes the signal an order larger instead.
 */
const STREAM_GAP_MS = 120;

/**
 * The two halves of the streamed answer under `streamAnswer`, delivered as one
 * artifact continued across two chunks. Exported so a test can assert the whole
 * of it survived the progress note and the terminal statement around it.
 */
export const STREAM_ANSWER = [
  "The invoice totals four hundred and twenty euros",
  ", payable within thirty days of receipt.",
] as const;

/** JSON-RPC methods this stub answers with an SSE stream, in both spellings. */
const STREAMING_METHODS = new Set([
  "message/stream",
  "SendStreamingMessage",
  "tasks/resubscribe",
  "SubscribeToTask",
]);

/**
 * A stub upstream A2A agent: serves an agent card and answers JSON-RPC, while
 * recording what the gateway actually sent it — the wire version it announced
 * and the credential it presented.
 *
 * The card it serves deliberately advertises an unreachable `https://` address
 * both at the top level and inside `supportedInterfaces`, so a test can prove
 * the gateway rewrote every one of them before handing the card to a caller.
 */
export async function startA2aUpstream(
  options: A2aUpstreamOptions = {},
): Promise<A2aUpstream> {
  const mount: A2aCardMount = options.cardMount ?? "origin";
  const requests: A2aReceivedRequest[] = [];
  const openStreams = new Set<ServerResponse>();
  const servicePath = mount === "path" ? PATH_PREFIX : "/a2a";
  const cardPath =
    mount === "path"
      ? `${PATH_PREFIX}/.well-known/agent-card.json`
      : "/.well-known/agent-card.json";

  const httpServer: HttpServer = createServer((req, res) => {
    handle(req, res, {
      requests,
      servicePath,
      cardPath,
      token: options.token,
      wireShape: options.wireShape ?? "0.3",
      streamChunks:
        options.streamChunks ?? (options.streamAnswer ? [...STREAM_ANSWER] : undefined),
      holdStreamOpen: options.holdStreamOpen ?? false,
      streamMessage: options.streamMessage,
      streamError: options.streamError,
      openStreams,
      unaryAnswer: options.unaryAnswer ?? "The invoice is settled.",
      unaryTask: options.unaryTask ?? {},
      unaryError: options.unaryError,
    }).catch((err: unknown) => {
      // `handle` rejects on a malformed body, and on a write to an already
      // closed socket. Unhandled, that terminates the test process; worse, the
      // request never gets a response, so a stub fault reads as a gateway
      // timeout instead of what it is. Answer on the wire either way.
      if (!res.headersSent) {
        res.writeHead(500, { "content-type": "application/json" });
      }
      res.end(JSON.stringify({ error: `a2a stub failed: ${String(err)}` }));
    });
  });
  await new Promise<void>((resolve) =>
    httpServer.listen(0, "127.0.0.1", resolve),
  );
  const address = httpServer.address();
  if (address === null || typeof address === "string") {
    throw new Error("a2a upstream: no listen address");
  }

  return {
    url: `http://127.0.0.1:${address.port}${servicePath}`,
    requests,
    close: () => {
      // A stream held open never ends on its own, so `close` would wait forever.
      for (const res of openStreams) res.end();
      return new Promise((resolve) => httpServer.close(() => resolve()));
    },
  };
}

async function handle(
  req: IncomingMessage,
  res: ServerResponse,
  ctx: {
    requests: A2aReceivedRequest[];
    servicePath: string;
    cardPath: string;
    token?: string;
    wireShape: A2aWireShape;
    streamChunks?: Array<string | unknown[]>;
    holdStreamOpen: boolean;
    streamMessage?: string;
    streamError?: Record<string, unknown>;
    openStreams: Set<ServerResponse>;
    unaryAnswer: string;
    unaryTask: Record<string, unknown>;
    unaryError?: Record<string, unknown>;
  },
): Promise<void> {
  const path = new URL(req.url ?? "/", "http://127.0.0.1").pathname;
  const header = (name: string): string | null => {
    const value = req.headers[name];
    return typeof value === "string" ? value : null;
  };
  const send = (status: number, body: unknown): void => {
    res.writeHead(status, { "content-type": "application/json" });
    res.end(JSON.stringify(body));
  };

  let body: Record<string, unknown> | undefined;
  if (req.method === "POST") {
    const chunks: Buffer[] = [];
    for await (const chunk of req) chunks.push(chunk as Buffer);
    const raw = Buffer.concat(chunks).toString("utf8");
    body = raw ? (JSON.parse(raw) as Record<string, unknown>) : undefined;
  }

  ctx.requests.push({
    httpMethod: req.method ?? "GET",
    path,
    version: header("a2a-version"),
    authorization: header("authorization"),
    apiKey: header("x-api-key"),
    body,
  });

  // The context the caller asked to continue, echoed back on every result the
  // way a real agent does — so a test can prove the gateway correlated the
  // call to that conversation rather than to one it invented.
  const params = body?.params as Record<string, unknown> | undefined;
  const message = params?.message as Record<string, unknown> | undefined;
  const contextId =
    typeof message?.contextId === "string" ? message.contextId : undefined;

  /**
   * Put a Task or an update event where this agent's wire version puts it:
   * flat under `result` with a `kind` tag on 0.3, inside the response's
   * payload wrapper on 1.0.
   */
  const V1_PAYLOAD_KEY = {
    task: "task",
    message: "message",
    "status-update": "statusUpdate",
    "artifact-update": "artifactUpdate",
  } as const;
  const payload = (
    kind: keyof typeof V1_PAYLOAD_KEY,
    obj: Record<string, unknown>,
  ): Record<string, unknown> =>
    ctx.wireShape === "1.0" ? { [V1_PAYLOAD_KEY[kind]]: obj } : { kind, ...obj };

  if (ctx.token !== undefined && header("authorization") !== `Bearer ${ctx.token}`) {
    send(401, { error: "unauthorized" });
    return;
  }

  if (req.method === "GET" && path === ctx.cardPath) {
    send(200, {
      name: "Invoice Processor",
      description: "Stub agent for the A2A gateway e2e.",
      protocolVersion: "1.0",
      version: "2.1.0",
      url: "https://internal.upstream.invalid/a2a",
      supportedInterfaces: [
        {
          url: "https://internal.upstream.invalid/a2a",
          protocolBinding: "JSONRPC",
          protocolVersion: "1.0",
        },
      ],
      skills: [{ id: "invoice", name: "Process invoice", tags: ["billing"] }],
    });
    return;
  }

  if (
    req.method === "POST" &&
    path === ctx.servicePath &&
    typeof body?.method === "string" &&
    STREAMING_METHODS.has(body.method)
  ) {
    // A real SSE body, written as three separate flushes with a comment and an
    // `event:` field mixed in, so a test can tell a relayed stream from a
    // buffered one: the gateway must forward each event as it lands.
    res.writeHead(200, {
      "content-type": "text/event-stream",
      "cache-control": "no-cache",
      connection: "keep-alive",
    });
    const envelope = (result: Record<string, unknown>) =>
      `data: ${JSON.stringify({ jsonrpc: "2.0", id: body?.id ?? null, result })}\n\n`;
    res.write(": open\n\n");
    // The agent thinks before it says anything, like a real one. Without this
    // the first event lands sub-millisecond and a time-to-first-event of 0 is
    // indistinguishable from one that was never recorded.
    await new Promise((resolve) => setTimeout(resolve, STREAM_GAP_MS));
    const holdOrEnd = () => {
      if (ctx.holdStreamOpen) {
        ctx.openStreams.add(res);
        res.on("close", () => ctx.openStreams.delete(res));
        return;
      }
      res.end();
    };
    if (ctx.streamMessage !== undefined) {
      res.write(
        envelope(
          payload("message", {
            messageId: "msg-e2e-stream",
            contextId,
            role: ctx.wireShape === "1.0" ? "ROLE_AGENT" : "agent",
            parts: [{ kind: "text", text: ctx.streamMessage }],
          }),
        ),
      );
      holdOrEnd();
      return;
    }
    if (ctx.streamError !== undefined) {
      res.write(`data: ${JSON.stringify({ jsonrpc: "2.0", id: body?.id ?? null, error: ctx.streamError })}\n\n`);
      holdOrEnd();
      return;
    }
    res.write(
      envelope(
        payload("status-update", {
          taskId: "task-e2e-stream",
          contextId,
          status: { state: "working" },
          seq: 1,
          sawVersion: header("a2a-version"),
          sawAccept: header("accept"),
        }),
      ),
    );
    await new Promise((resolve) => setTimeout(resolve, STREAM_GAP_MS));
    res.write("event: status-update\n");
    res.write(
      envelope(payload("status-update", { taskId: "task-e2e-stream", contextId, seq: 2 })),
    );
    if (ctx.streamChunks) {
      const parts = (chunk: string | unknown[]) =>
        typeof chunk === "string" ? [{ kind: "text", text: chunk }] : chunk;
      const [first, ...rest] = ctx.streamChunks;
      // The answer in continued chunks with a progress note after the first:
      // an accumulator that lets a note replace the answer keeps only the note.
      res.write(
        envelope(
          payload("artifact-update", {
            taskId: "task-e2e-stream",
            contextId,
            artifact: {
              artifactId: "report",
              parts: parts(first),
            },
          }),
        ),
      );
      res.write(
        envelope(
          payload("status-update", {
            taskId: "task-e2e-stream",
            contextId,
            status: {
              state: "working",
              message: { role: "agent", parts: [{ kind: "text", text: "halfway" }] },
            },
          }),
        ),
      );
      for (const chunk of rest) {
        res.write(
          envelope(
            payload("artifact-update", {
              taskId: "task-e2e-stream",
              contextId,
              append: true,
              artifact: {
                artifactId: "report",
                parts: parts(chunk),
              },
            }),
          ),
        );
      }
    }
    await new Promise((resolve) => setTimeout(resolve, STREAM_GAP_MS));
    res.write(
      envelope(
        payload("task", {
          id: "task-e2e-stream",
          contextId,
          status: ctx.streamChunks
            ? {
                state: "completed",
                message: { role: "agent", parts: [{ kind: "text", text: "Report generated." }] },
              }
            : { state: "completed" },
          seq: 3,
        }),
      ),
    );
    holdOrEnd();
    return;
  }

  if (req.method === "POST" && path === ctx.servicePath && ctx.unaryError) {
    send(200, { jsonrpc: "2.0", id: body?.id ?? null, error: ctx.unaryError });
    return;
  }

  if (req.method === "POST" && path === ctx.servicePath) {
    const task = {
      id: UNARY_TASK_ID,
      contextId,
      status: {
        state: "completed",
        message: { role: "agent", parts: [{ kind: "text", text: ctx.unaryAnswer }] },
      },
      // Echoed so the gateway's forwarding can be asserted from the caller
      // side as well as from `requests`.
      sawVersion: header("a2a-version"),
      sawMethod: body?.method ?? null,
      ...ctx.unaryTask,
    };
    const bare =
      ctx.wireShape === "1.0" &&
      typeof body?.method === "string" &&
      V1_BARE_TASK_METHODS.has(body.method);
    send(200, {
      jsonrpc: "2.0",
      id: body?.id ?? null,
      result: bare ? task : payload("task", task),
    });
    return;
  }

  // The catch-all a real path-hosting agent platform answers with: not a 404,
  // which is what made the original report look like a missing card rather
  // than a mis-built URL.
  send(405, { error: "method not allowed" });
}
