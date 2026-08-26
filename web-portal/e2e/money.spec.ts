import { test, expect } from "@playwright/test";
import { startMoney, ADMIN_TOKEN, type MoneyFixture } from "./money.fixture";

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

test("真实消费被网关按定价折算并带上_user_id", async () => {
  expect(await fx.chat()).toBe(200);
  const metrics = await fetch(fx.metricsUrl).then((r) => r.text());
  const line = metrics
    .split("\n")
    .find((l) => l.startsWith("aisix_llm_spend_micro_usd_total{") && l.includes(fx.userId));
  expect(line, "花费指标里没有这个用户的序列").toBeTruthy();
  // 1000 输入 + 1000 输出，两边都是 $0.5/1k → 恰好 $1.00 = 1_000_000 micro。
  expect(Number(line!.slice(line!.lastIndexOf("}") + 1).trim())).toBeGreaterThanOrEqual(1_000_000);
});

test("消费耗尽后密钥被停用_网关随即拒绝_补额后恢复", async () => {
  // 给 $1.5：一次调用花 $1，第二次就把余额压成负的。
  await grant(1_500_000);

  expect(await fx.chat()).toBe(200);
  expect(await fx.chat()).toBe(200);

  // 对账环把消费入账，余额转负，于是把这把密钥写成 disabled。
  await waitFor(
    async () => /disabled:\s*true/.test(fx.readResources()),
    "余额耗尽后密钥被停用",
  );

  // 网关读到 disabled 之后必须拒绝 —— 这是整条链路真正闭合的那一步。
  await waitFor(async () => (await fx.chat()) === 401, "网关拒绝已停用的密钥");

  // 补额后恢复。
  await grant(10_000_000);
  await waitFor(
    async () => !/disabled:\s*true/.test(fx.readResources()),
    "补额后密钥被重新启用",
  );
  await waitFor(async () => (await fx.chat()) === 200, "网关重新放行");
});
