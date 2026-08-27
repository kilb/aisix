import { test, expect } from "@playwright/test";
import { startMoney, ADMIN_TOKEN, CALLER_KEY, type MoneyFixture } from "./money.fixture";

/**
 * 钱路：真网关 + 真流量 + 真指标 + 真账本 + 真停用。
 *
 * 计划初稿在这里留了个洞 —— 要求覆盖「消费耗尽 → 密钥被停用」，却没说消费从
 * 哪来。这一层给出答案：网关跑在文件模式（不需要 etcd），请求打进去、按配置的
 * 定价折算出花费、打到指标；门户对账扣余额；余额归零把密钥写成 disabled；
 * 网关据此拒绝下一次请求。
 *
 * 只有上游 LLM 与 PromQL 引擎是垫片，其余全是生产件。
 */

let fx: MoneyFixture;
/** 第一条用例铸出来的明文，第二条要用它证明网关认得这把密钥。 */
let mintedKey = "";

test.describe.configure({ timeout: 180_000 });

test.beforeAll(async () => {
  fx = await startMoney();
});
test.afterAll(() => fx?.stop());

async function grant(micro: number): Promise<void> {
  const list = await fetch(`${fx.portalUrl}/admin/users`, {
    headers: { authorization: `Bearer ${ADMIN_TOKEN}` },
  }).then((r) => r.json());
  const u = list.users.find((x: { user_id: string }) => x.user_id === fx.userId);
  expect(u, "管理端列表里找不到 e2e 用户").toBeTruthy();
  const r = await fetch(`${fx.portalUrl}/admin/users/${fx.userId}/grant`, {
    method: "POST",
    headers: { authorization: `Bearer ${ADMIN_TOKEN}`, "content-type": "application/json" },
    body: JSON.stringify({ micro_usd: micro, note: "e2e" }),
  });
  expect(r.status).toBe(200);
}

/** 等对账环跑过若干轮。轮询周期 15 秒，所以这里给足时间。 */
async function waitFor(cond: () => Promise<boolean>, label: string, ms = 90_000) {
  const until = Date.now() + ms;
  while (Date.now() < until) {
    if (await cond()) return;
    await new Promise((r) => setTimeout(r, 1000));
  }
  throw new Error(`超时等待：${label}`);
}

/** 当前余额。 */
async function balance(cookie: string): Promise<number> {
  const b = await fetch(`${fx.portalUrl}/api/balance`, { headers: { cookie } }).then((r) =>
    r.json(),
  );
  return b.balance_micro_usd as number;
}

// 这一组用例**按声明顺序**跑在同一份夹具上，所以每条都自带前提、不依赖前一条
// 的残留。第一版让最后一条靠「打请求把余额耗到零」来准备起点 —— 那是错的：
// 每次调用花 $1，而对账每 15 秒才跑一轮，循环拿着过期的正余额疯狂发请求，把
// 余额打到很深的负数，之后发放的额度根本填不平。

test("自助建的密钥在零余额时生下来就是停用的_网关据此拒绝", async () => {
  const cookie = await fx.sessionCookie();
  // 刚注册，余额为零 —— 这条必须跑在任何发放之前。
  expect(await balance(cookie)).toBe(0);

  const minted = await fetch(`${fx.portalUrl}/api/keys`, {
    method: "POST",
    headers: { cookie, "content-type": "application/json" },
    body: JSON.stringify({ label: "money-e2e" }),
  }).then((r) => r.json());
  expect(minted.plaintext).toMatch(/^sk-aisix-[0-9a-f]{64}$/);
  expect(minted.disabled).toBe(true);

  // 网关拒绝。此刻 401 还分不清「不认识」与「已停用」——「发放额度后同一把
  // 密钥转为 200」那条用例才是证明它确实被认出来的地方。
  expect(await fx.chatWith(minted.plaintext)).toBe(401);
  mintedKey = minted.plaintext;
});

test("消费耗尽后密钥被停用_网关随即拒绝_补额后恢复", async () => {
  // 从零起。给 $1.5：一次调用花 $1，第二次就把余额压成负的。
  await grant(1_500_000);
  await waitFor(async () => (await fx.chat()) === 200, "发放后预置密钥可用");
  expect(await fx.chat()).toBe(200);

  await waitFor(
    async () => /disabled:\s*true/.test(fx.readResources()),
    "余额耗尽后密钥被停用",
  );
  // 网关读到 disabled 之后必须拒绝 —— 这是整条链路真正闭合的那一步。
  await waitFor(async () => (await fx.chat()) === 401, "网关拒绝已停用的密钥");

  await grant(20_000_000);
  await waitFor(async () => (await fx.chat()) === 200, "补额后网关重新放行");
});

test("同一把自助密钥在有余额后被网关放行_消费记在这个用户名下", async () => {
  expect(mintedKey, "第一条用例没铸出密钥").toBeTruthy();
  // 上一条用例结束时余额是正的，所以这把密钥应当已被对账环启用。
  //
  // 它能通同时证明三件事：散列算法与网关一致、`user_id` 写对了、SIGHUP 送到了。
  await waitFor(
    async () => (await fx.chatWith(mintedKey)) === 200,
    "自助密钥被网关放行",
  );

  const metrics = await fetch(fx.metricsUrl).then((r) => r.text());
  expect(metrics).toContain(`user_id="${fx.userId}"`);
});

test("真实消费被网关按定价折算并带上_user_id", async () => {
  expect(await fx.chat()).toBe(200);
  const metrics = await fetch(fx.metricsUrl).then((r) => r.text());
  const line = metrics
    .split("\n")
    .find((l) => l.startsWith("aisix_llm_spend_micro_usd_total{") && l.includes(fx.userId));
  expect(line, "花费指标里没有这个用户的序列").toBeTruthy();
  // 1000 输入 + 1000 输出，两边都是 $0.5/1k → 每次调用恰好 $1.00。
  expect(Number(line!.slice(line!.lastIndexOf("}") + 1).trim())).toBeGreaterThanOrEqual(1_000_000);
});

test("网关侧的累计发放额独立于门户余额_用尽即拒且不给重试时间", async () => {
  // 先把门户侧余额垫高。两个闸是独立的：门户按余额停密钥（401），网关按累计
  // 发放额拒请求（429）。不垫高的话下面的轮询会先把余额打穿，对账环停掉密钥，
  // 于是永远等不到 429 —— 第一版就是这么挂的，看起来像网关的闸没生效。
  await grant(200_000_000);
  await waitFor(async () => (await fx.chat()) === 200, "垫高余额后可调用");

  // 给这个用户挂一条**只有累计发放额、没有窗口**的策略。走配置而不是门户
  // 接口：要测的是网关那一侧的闸。
  const before = fx.readResources();
  fx.writeResources(
    `${before}\nrate_limit_policies:\n` +
      `  - name: e2e-allowance\n` +
      `    scope: member\n` +
      `    scope_ref: "${fx.userId}"\n` +
      `    granted_micro_usd: 2500000\n`,
  );
  // 直接改文件的用例要自己发 SIGHUP：平时那一下是对账环写完配置时发的。
  fx.reloadGateway();
  await new Promise((r) => setTimeout(r, 800));

  // 每次调用 $1。累计到 $2.5 之上就拒 —— 准入时比的是**已记录**的消费，
  // 所以越过那一刻的那次请求仍会放行，这是文档里写明的溢出。
  let last = 0;
  await waitFor(async () => {
    last = await fx.chatWithStatus();
    return last === 429;
  }, "累计发放额用尽后被拒");

  // 拒绝理由必须是「额度用尽」而不是「速率超了」：两者都是 429，但一个要
  // 充值、一个等等就好。
  const r = await fetch(`${fx.proxyUrl}/v1/chat/completions`, {
    method: "POST",
    headers: { authorization: `Bearer ${CALLER_KEY}`, "content-type": "application/json" },
    body: JSON.stringify({ model: "gpt-4o-mini", messages: [{ role: "user", content: "hi" }] }),
  });
  expect(r.status).toBe(429);
  const body = await r.text();
  expect(body).toContain("allowance exhausted");
  expect(body).toContain("top up");
  expect(body).toContain("billing_error");
  // 不给 retry-after —— 等是等不回来的，只有人去充值才行。
  expect(r.headers.get("retry-after")).toBeNull();
});
