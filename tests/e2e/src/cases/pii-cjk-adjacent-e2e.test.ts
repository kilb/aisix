import { createHash } from "node:crypto";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  EtcdClient,
  SeedClient,
  spawnApp,
  startOpenAiUpstream,
  waitConfigPropagation,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

// E2E：内置 PII 检测在中文里也认得出紧贴着汉字的号码、邮箱、身份证号。
//
// 中文不用空格分词：「手机号是13800138000请回电」里号码前后都是汉字，这是中文
// 用户最常见的写法。检测要是只认被空格或标点隔开的号码，这类输入就原样送到
// 上游 —— 脱敏规则形同虚设。对照：一串更长的数字里恰好含着手机号模样的一段，
// 不是手机号，不能被打码。

const CALLER = "sk-pii-cjk-adjacent";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");
const MODEL = "pii-cjk";

const MOBILE = "13800138000";
const EMAIL = "zhang.san@example.com";
const ID_CARD = "11010519491231002X";
const ORDER_NO = "913800138000123";

describe("built-in PII detectors find values written flush against CJK text", () => {
  let app: SpawnedApp | undefined;
  let upstream: OpenAiUpstream | undefined;
  let etcdReachable = false;

  const send = (content: string) =>
    fetch(`${app!.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${CALLER}` },
      body: JSON.stringify({ model: MODEL, messages: [{ role: "user", content }] }),
    });

  /** What the upstream received as the user turn of the latest request. */
  const forwarded = () => {
    const body = JSON.parse(upstream!.receivedRequests.at(-1)?.body ?? "{}") as {
      messages?: Array<{ role: string; content: unknown }>;
    };
    return String(body.messages?.find((m) => m.role === "user")?.content ?? "");
  };

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;
    upstream = await startOpenAiUpstream();
    app = await spawnApp();
    const seed = new SeedClient(etcd, app.etcdPrefix);
    const pk = await seed.createProviderKey({
      display_name: "pii-cjk-pk",
      provider: "openai",
      adapter: "openai",
      secret: "sk-mock",
      api_base: `${upstream.baseUrl}/v1`,
    });
    await seed.createModel({
      display_name: MODEL,
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: pk.id,
    });
    await seed.createGuardrail({
      name: "pii-cjk-mask",
      enabled: true,
      hook_point: "input",
      kind: "pii",
      detectors: [
        { type: "china_mobile", action: "mask" },
        { type: "email", action: "mask" },
        { type: "china_id_card", action: "mask" },
      ],
    });
    await seed.createApiKey({ key_hash: CALLER_HASH, allowed_models: [MODEL] });
    // 就绪：模型可达，且护栏已经生效（空格隔开的号码被打码，这是改动前就成立的）。
    await waitConfigPropagation(async () => {
      const r = await send(`call ${MOBILE} now`);
      await r.text();
      return r.status === 200 && !forwarded().includes(MOBILE);
    });
  }, 60_000);

  afterAll(async () => {
    await app?.exit();
    await upstream?.close();
  });

  for (const [label, text, secret] of [
    ["mobile", `我的手机号是${MOBILE}请回电`, MOBILE],
    ["email", `邮箱是${EMAIL}谢谢`, EMAIL],
    ["national ID", `身份证${ID_CARD}已提交`, ID_CARD],
  ] as const) {
    test(`a ${label} between CJK characters never reaches the upstream`, async (ctx) => {
      if (!etcdReachable || !app || !upstream) return ctx.skip();
      const res = await send(text);
      expect(res.status, await res.text()).toBe(200);
      const sent = forwarded();
      expect(sent).not.toContain(secret);
      // 周围的文字照常送达。
      expect(sent.startsWith(text.slice(0, 2))).toBe(true);
      expect(sent.endsWith(text.slice(-2))).toBe(true);
    });
  }

  test("a longer digit run that merely contains a mobile-shaped slice is left alone", async (ctx) => {
    if (!etcdReachable || !app || !upstream) return ctx.skip();
    const text = `订单号${ORDER_NO}已发货`;
    const res = await send(text);
    expect(res.status, await res.text()).toBe(200);
    expect(forwarded()).toBe(text);
  });
});
