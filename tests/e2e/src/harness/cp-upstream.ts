import { createServer, type IncomingMessage, type ServerResponse } from "node:http";

import { pickFreePort } from "./ports.js";

/**
 * An OpenAI-shaped upstream account for the control-plane cases: unlike
 * `startOpenAiUpstream`, which answers every request with one configured
 * shape, it answers each endpoint in that endpoint's own shape and streams
 * exactly when the request asks to, so one account can serve chat, messages
 * (bridged through chat), responses, embeddings and token counting, both
 * buffered and streamed, as a real provider does.
 *
 * Every id it issues carries `marker`, and so does the text of every error it
 * answers with: the channel marker a platform deployment must keep from the
 * tenant. `failWith` makes it answer every request with that status.
 */
export interface PlatformUpstream {
  baseUrl: string;
  requests: Array<{ path: string; body: Record<string, unknown> }>;
  /** How many requests arrived on `path` (exact match, query excluded). */
  count(path: string): number;
  /** When set, every request is answered with this status and an error naming `marker`. */
  failWith: number | undefined;
  close(): Promise<void>;
}

const sseData = (payload: unknown) => `data: ${JSON.stringify(payload)}\n\n`;
const sseEvent = (event: string, payload: unknown) =>
  `event: ${event}\ndata: ${JSON.stringify(payload)}\n\n`;

function chat(res: ServerResponse, body: Record<string, unknown>, marker: string): void {
  const id = `chatcmpl-${marker}`;
  const model = String(body.model ?? "");
  const usage = { prompt_tokens: 5, completion_tokens: 2, total_tokens: 7 };
  if (body.stream === true) {
    const chunk = (delta: unknown, finish: string | null) => ({
      id,
      object: "chat.completion.chunk",
      created: 1,
      model,
      system_fingerprint: `fp_${marker}`,
      choices: [{ index: 0, delta, finish_reason: finish }],
    });
    res.writeHead(200, { "content-type": "text/event-stream" });
    res.write(sseData(chunk({ role: "assistant", content: "ok" }, null)));
    res.write(sseData(chunk({}, "stop")));
    const options = body.stream_options as { include_usage?: boolean } | undefined;
    if (options?.include_usage) res.write(sseData({ ...chunk({}, null), choices: [], usage }));
    res.end("data: [DONE]\n\n");
    return;
  }
  res.writeHead(200, { "content-type": "application/json" });
  res.end(
    JSON.stringify({
      id,
      object: "chat.completion",
      created: 1,
      model,
      system_fingerprint: `fp_${marker}`,
      choices: [{ index: 0, message: { role: "assistant", content: "ok" }, finish_reason: "stop" }],
      usage,
    }),
  );
}

function responses(res: ServerResponse, body: Record<string, unknown>, marker: string): void {
  const model = String(body.model ?? "");
  const item = {
    type: "message",
    id: `msg_${marker}`,
    status: "completed",
    role: "assistant",
    content: [{ type: "output_text", text: "ok", annotations: [] }],
  };
  const response = (status: string, output: unknown[]) => ({
    id: `resp_${marker}`,
    object: "response",
    created_at: 1,
    status,
    model,
    output,
    usage:
      status === "completed"
        ? { input_tokens: 5, output_tokens: 2, total_tokens: 7 }
        : null,
  });
  if (body.stream === true) {
    res.writeHead(200, { "content-type": "text/event-stream" });
    let seq = 0;
    const send = (type: string, payload: Record<string, unknown>) =>
      res.write(sseEvent(type, { type, sequence_number: seq++, ...payload }));
    send("response.created", { response: response("in_progress", []) });
    send("response.output_item.added", {
      output_index: 0,
      item: { ...item, status: "in_progress", content: [] },
    });
    send("response.output_text.delta", {
      item_id: item.id,
      output_index: 0,
      content_index: 0,
      delta: "ok",
    });
    send("response.output_text.done", {
      item_id: item.id,
      output_index: 0,
      content_index: 0,
      text: "ok",
    });
    send("response.output_item.done", { output_index: 0, item });
    send("response.completed", { response: response("completed", [item]) });
    res.end();
    return;
  }
  res.writeHead(200, { "content-type": "application/json" });
  res.end(JSON.stringify(response("completed", [item])));
}

export async function startPlatformUpstream(marker: string): Promise<PlatformUpstream> {
  const upstream: PlatformUpstream = {
    baseUrl: "",
    requests: [],
    count: (path) => upstream.requests.filter((r) => r.path === path).length,
    failWith: undefined,
    close: async () => {},
  };
  const server = createServer((req: IncomingMessage, res: ServerResponse) => {
    res.on("error", () => {});
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => {
      const path = (req.url ?? "").split("?")[0];
      let body: Record<string, unknown> = {};
      try {
        body = JSON.parse(Buffer.concat(chunks).toString("utf8")) as Record<string, unknown>;
      } catch {
        // Not a JSON body.
      }
      upstream.requests.push({ path, body });
      if (upstream.failWith !== undefined) {
        res.writeHead(upstream.failWith, { "content-type": "application/json" });
        res.end(
          JSON.stringify({
            error: { message: `upstream failure on ${marker}`, type: "server_error", code: null },
          }),
        );
        return;
      }
      switch (path) {
        case "/v1/chat/completions":
          return chat(res, body, marker);
        case "/v1/responses":
          return responses(res, body, marker);
        case "/v1/embeddings":
          res.writeHead(200, { "content-type": "application/json" });
          res.end(
            JSON.stringify({
              object: "list",
              data: [{ object: "embedding", index: 0, embedding: [0.25, 0.5] }],
              model: String(body.model ?? ""),
              usage: { prompt_tokens: 2, total_tokens: 2 },
            }),
          );
          return;
        case "/v1/messages/count_tokens":
          res.writeHead(200, { "content-type": "application/json" });
          res.end(JSON.stringify({ input_tokens: 7 }));
          return;
        case "/v1/images/generations":
          res.writeHead(200, { "content-type": "application/json" });
          res.end(JSON.stringify({ created: 1, data: [{ url: `https://img.invalid/${marker}.png` }] }));
          return;
        default:
          res.writeHead(404, { "content-type": "application/json" });
          res.end(JSON.stringify({ error: { message: `no route ${path} on ${marker}`, type: "invalid_request_error" } }));
      }
    });
  });
  server.keepAliveTimeout = 120_000;
  server.headersTimeout = 125_000;
  const port = await pickFreePort();
  await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", resolve));
  upstream.baseUrl = `http://127.0.0.1:${port}`;
  upstream.close = () =>
    new Promise<void>((resolve) => {
      server.closeAllConnections();
      server.close(() => resolve());
    });
  return upstream;
}

/** A gateway answer, read whole: status, headers and body text. */
export interface GatewayAnswer {
  status: number;
  headers: Headers;
  text: string;
  /** The body as JSON, or `undefined` for a body that is not JSON (a stream). */
  json: any;
}

/**
 * One request to the gateway with a caller key, sent the way each endpoint's
 * own SDK sends it: `x-api-key` and `anthropic-version` on `/v1/messages*`,
 * a bearer token elsewhere.
 */
export async function callGateway(
  proxyUrl: string,
  key: string,
  path: string,
  body: unknown,
  headers: Record<string, string> = {},
): Promise<GatewayAnswer> {
  const anthropic = path.startsWith("/v1/messages");
  const res = await fetch(`${proxyUrl}${path}`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      ...(anthropic
        ? { "x-api-key": key, "anthropic-version": "2023-06-01" }
        : { authorization: `Bearer ${key}` }),
      ...headers,
    },
    body: JSON.stringify(body),
  });
  const text = await res.text();
  let json: unknown;
  try {
    json = JSON.parse(text);
  } catch {
    json = undefined;
  }
  return { status: res.status, headers: res.headers, text, json };
}
