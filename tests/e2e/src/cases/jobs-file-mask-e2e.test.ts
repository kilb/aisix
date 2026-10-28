import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  waitConfigPropagation,
  type SpawnedApp,
} from "../harness/index.js";
import { pickFreePort } from "../harness/ports.js";

// E2E：批处理输入文件上的 mask 规则，作用对象是每一行里的请求内容，而不是
// 文件的原始字节。
//
//   1. 内容按 JSON 语义被 mask：一个用 JSON 转义写出的邮箱（`@`）和直接写
//      出来的一样要被遮蔽；`custom_id` / `method` / `url` / `body.model` 原样
//      保留，每一行仍是合法 JSON。
//   2. 协议字段本身命中 mask 规则时（`custom_id` 是邮箱），网关既不能改写它
//      （两行会变成同一个 id，结果再也对不回请求），也不能原样放行（规则就白
//      设了），所以拒绝这次上传，上游一个字节都收不到。
//   3. 对象的键命中 mask 规则时也一样：键改不了（改了，上游读到的就是另一个字
//      段），原样放行又白设了规则，所以拒绝上传 —— 和 /v1/chat/completions 对
//      改写不了的键的处理一致。

const CALLER_PLAINTEXT = "sk-jobs-file-mask-caller";
const CALLER_KEY_HASH = createHash("sha256").update(CALLER_PLAINTEXT).digest("hex");
const MODEL = "jfm-model";

interface FilesUpstream {
  baseUrl: string;
  uploads: string[];
  close(): Promise<void>;
}

/** 只认 `POST /v1/files` 的上游，记下每次收到的 multipart 原文。 */
async function startFilesUpstream(): Promise<FilesUpstream> {
  const uploads: string[] = [];
  const server: Server = createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => {
      res.setHeader("content-type", "application/json");
      if (req.method === "POST" && req.url?.endsWith("/v1/files")) {
        uploads.push(Buffer.concat(chunks).toString("utf8"));
        res.statusCode = 200;
        res.end(
          JSON.stringify({ id: "file-jfm", object: "file", purpose: "batch", filename: "input.jsonl" }),
        );
        return;
      }
      res.statusCode = 404;
      res.end("{}");
    });
  });
  const port = await pickFreePort();
  await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", resolve));
  return {
    baseUrl: `http://127.0.0.1:${port}`,
    uploads,
    async close() {
      server.closeAllConnections?.();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

/** 从上游收到的 multipart 原文里取出文件部分的 JSONL 行。 */
function jsonlLinesOf(multipart: string): string[] {
  return multipart.split(/\r?\n/).filter((l) => l.trim().startsWith("{"));
}

function batchLine(customId: string, content: string): string {
  return JSON.stringify({
    custom_id: customId,
    method: "POST",
    url: "/v1/chat/completions",
    body: { model: "gpt-4o-mini", messages: [{ role: "user", content }] },
  });
}

describe("mask rules on an uploaded batch file act on each line's request content", () => {
  let app: SpawnedApp | undefined;
  let upstream: FilesUpstream | undefined;
  let etcdReachable = false;

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    upstream = await startFilesUpstream();

    await seed.createGuardrail({
      name: "jfm-mask-email",
      enabled: true,
      hook_point: "input",
      kind: "pii",
      detectors: [{ type: "email", action: "mask" }],
    });
    const pk = await seed.createProviderKey({
      display_name: "jfm-pk",
      secret: "sk-upstream-jfm",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: MODEL,
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    // key 最后写：它能鉴权时，按修订号顺序前面的护栏与模型都已生效。
    await seed.createApiKey({ key_hash: CALLER_KEY_HASH, allowed_models: ["*"] });
    await waitConfigPropagation(async () => {
      const res = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER_PLAINTEXT}` },
      });
      await res.arrayBuffer();
      return res.status === 200;
    });
  });

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  async function upload(jsonl: string): Promise<{ status: number; body: string }> {
    const form = new FormData();
    form.append("purpose", "batch");
    form.append("file", new Blob([jsonl], { type: "application/jsonl" }), "input.jsonl");
    const res = await fetch(`${app!.proxyUrl}/v1/files`, {
      method: "POST",
      headers: { authorization: `Bearer ${CALLER_PLAINTEXT}`, "x-aisix-model": MODEL },
      body: form,
    });
    return { status: res.status, body: await res.text() };
  }

  test("content is masked by meaning, protocol fields and JSON stay intact", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }
    const before = upstream.uploads.length;
    // 第二行的邮箱是 JSON 转义写法：解码后就是同一个邮箱。
    const jsonl =
      [
        batchLine("r1", "mail alice@example.com please"),
        batchLine("r2", "mail bob@example.com please").replace("bob@", "bob\\u0040"),
      ].join("\n") + "\n";

    const res = await upload(jsonl);
    expect(res.status, res.body).toBe(200);
    expect(upstream.uploads.length).toBe(before + 1);

    const lines = jsonlLinesOf(upstream.uploads.at(-1)!);
    expect(lines.length, "both lines reach the provider").toBe(2);
    const parsed = lines.map((l) => JSON.parse(l));
    expect(parsed.map((l) => l.custom_id)).toEqual(["r1", "r2"]);
    for (const l of parsed) {
      expect(l.method).toBe("POST");
      expect(l.url).toBe("/v1/chat/completions");
      expect(l.body.model).toBe("gpt-4o-mini");
    }
    const contents = parsed.map((l) => String(l.body.messages[0].content));
    expect(contents[0], "a plainly written email is masked").not.toContain("alice@example.com");
    expect(contents[1], "a JSON-escaped email is masked too").not.toContain("bob@example.com");
  });

  test("a protocol field matching a mask rule refuses the upload instead of rewriting it", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }
    const before = upstream.uploads.length;
    const jsonl =
      [batchLine("a@corp.example", "hello"), batchLine("b@corp.example", "hello")].join("\n") + "\n";

    const res = await upload(jsonl);
    expect(res.status, res.body).toBe(422);
    expect(upstream.uploads.length, "the refused file must not reach the provider").toBe(before);
  });

  test("an object key matching a mask rule refuses the upload instead of passing it unmasked", async (ctx) => {
    if (!etcdReachable || !app || !upstream) {
      ctx.skip();
      return;
    }
    const lines = {
      "a key inside the request body": JSON.stringify({
        custom_id: "r1",
        method: "POST",
        url: "/v1/chat/completions",
        body: {
          model: "gpt-4o-mini",
          messages: [{ role: "user", content: "hello" }],
          metadata: { "bob@example.com": "1" },
        },
      }),
      "a key at the top of a line": JSON.stringify({
        custom_id: "r1",
        method: "POST",
        url: "/v1/chat/completions",
        body: { model: "gpt-4o-mini", messages: [{ role: "user", content: "hello" }] },
        "bob@example.com": 1,
      }),
    };
    for (const [where, line] of Object.entries(lines)) {
      const before = upstream.uploads.length;
      const res = await upload(line + "\n");
      expect.soft(res.status, `${where}: ${res.body}`).toBe(422);
      expect.soft(res.body, where).not.toContain("bob@example.com");
      expect.soft(upstream.uploads.length, `${where}: the refused file must not reach the provider`).toBe(before);
    }
  });
});
