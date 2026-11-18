import { randomUUID } from "node:crypto";
import { request } from "node:https";
import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

let stack: Stack;
const account = "历史成本补填验收";
const operator = { email: "cost-operator@e2e.local", password: "cost-operator-pass-103" };
let accountId: string;
let groupId: string;
const groupName = "historical-cost-e2e";

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
  const group = await stack.admin.createGroup(groupName, "补填验收分组", "7");
  groupId = group.id;
  const key = await stack.admin.post("/admin/resources/provider_keys", {
    doc: { display_name: account, provider: "openai", api_key: "e2e-unused-key", groups: [groupName] },
  });
  accountId = key.id;
  await stack.admin.post("/admin/operators", { ...operator, role: "operator" });
});

test.afterAll(async () => { await stack?.stop(); });

async function openAccount(page: Page, user: typeof operator) {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱", { exact: true }).fill(user.email);
  await page.getByLabel("口令", { exact: true }).fill(user.password);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await expect(page.locator("main")).toBeVisible();
  await page.goto(`${stack.previewUrl}/#pricing?section=cost`);
  await page.getByRole("table", { name: "上游账号成本价" }).getByRole("button", { name: account }).click();
  return page.getByRole("region", { name: `上游账号 ${account}` });
}

// 通过隔离环境签发的 mTLS 网关身份走生产遥测入口，得到计费引擎实际生成的扣款与余额。
async function recordUsage() {
  const model = await stack.admin.post("/admin/public-models", { name: "cost-backfill-model" });
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${model.id}`, { priority: 1, weight: 1 });
  await stack.admin.post("/admin/pricing/baselines", {
    model_id: model.id, effective_from: new Date().toISOString(), note: "补填验收基准价",
    card: { default: { base: { input: "10", output: "30" } } },
  });
  const tenant = await stack.admin.post("/admin/tenants", { name: "补填验收租户" });
  await stack.admin.patch(`/admin/tenants/${tenant.id}`, { allowed_provider_groups: [groupName] }, tenant.version);
  const key = await stack.admin.post(`/admin/tenants/${tenant.id}/keys`, { group_id: groupId, label: "验收密钥" });
  await stack.admin.post(`/admin/tenants/${tenant.id}/credits`, { amount_cny: "10", reason: "验收额度" });

  const gateway = await stack.admin.post("/admin/gateways/credentials", { name: "cost-backfill-meter" });
  const eventId = randomUUID();
  const event = {
    event_id: eventId, request_id: randomUUID(), occurred_at: new Date().toISOString(),
    team_id: tenant.id, api_key_id: key.key.id, requested_model_id: model.id, provider_key_id: accountId,
    operation: "chat", status_code: 200, prompt_tokens: 10_000, completion_tokens: 10_000,
  };
  const ack = await new Promise<string>((resolve, reject) => {
    const req = request(new URL("/dp/telemetry", gateway.env.AISIX_MANAGED__CP_BASE_URL), {
      method: "POST", cert: gateway.cert_pem, key: gateway.key_pem, ca: gateway.ca_pem,
      headers: { "content-type": "application/json", "x-aisix-usage-batch-id": randomUUID() },
    }, (res) => {
      let body = "";
      res.setEncoding("utf8");
      res.on("data", (part: string) => { body += part; });
      res.on("error", reject);
      res.on("end", () => res.statusCode === 200 ? resolve(body) : reject(new Error(`遥测返回 ${res.statusCode}: ${body}`)));
    });
    req.on("error", reject).end(JSON.stringify({ events: [event] }));
  });
  expect(JSON.parse(ack)).toMatchObject({ accepted: 1, rejected: 0 });
  return {
    charge: async () => (await stack.admin.get(`/admin/pricing/charges?event_id=${eventId}`)).items[0],
    balance: async () => (await stack.admin.get(`/admin/tenants/${tenant.id}/billing`)).balance_micro_cny,
  };
}

test("管理员显式补填历史成本，网络恢复后重试并读到真实结果", async ({ page, context }) => {
  const history = await recordUsage();
  const endpoint = `/admin/provider-keys/${accountId}/cost-backfill`;
  const before = { status: "charged", charge_micro_cny: 2_800_000, cost_micro_cny: null };
  const after = { ...before, cost_micro_cny: 120_000 };
  await expect.poll(history.charge).toMatchObject(before);
  expect(await history.balance()).toBe(7_200_000);
  const detail = await openAccount(page, stack.operator);
  const posted: unknown[] = [];
  page.on("request", (req) => {
    if (new URL(req.url()).pathname === endpoint && req.method() === "POST") posted.push(req.postDataJSON());
  });
  const form = detail.getByRole("group", { name: "修改默认成本换算率" });
  await form.getByLabel("新默认成本换算率").fill("0.3");
  await form.getByRole("button", { name: "保存默认成本换算率" }).click();
  await expect(form).toContainText("现在 0.3");
  expect(posted).toEqual([]);
  expect(await history.charge()).toMatchObject(before);

  const fill = detail.getByRole("group", { name: "历史成本补填" });
  const response = (field: "prepare" | "after") => page.waitForResponse((r) =>
    new URL(r.url()).pathname === endpoint && r.request().method() === "POST" && field in r.request().postDataJSON());
  const prepared = response("prepare");
  // 在浏览器发出第二笔请求前断网。准备请求由真实控制面签发游标，执行请求也没有被替换。
  const cdp = await context.newCDPSession(page);
  await cdp.send("Debugger.enable");
  await cdp.send("DOMDebugger.setXHRBreakpoint", { url: endpoint });
  let attempts = 0;
  cdp.on("Debugger.paused", async () => {
    if (++attempts === 2) {
      await context.setOffline(true);
      await cdp.send("DOMDebugger.removeXHRBreakpoint", { url: endpoint });
    }
    await cdp.send("Debugger.resume");
  });
  await fill.getByRole("button", { name: "补填历史成本", exact: true }).click();
  const first = await (await prepared).json();
  expect(first).toEqual({ examined: 0, filled: 0, next_after: expect.any(String) });
  await expect(fill.getByRole("alert")).toContainText("无法连接控制面");
  await cdp.detach();
  await context.setOffline(false);
  const result = response("after");
  await fill.getByRole("button", { name: "继续补填" }).click();
  expect(await (await result).json()).toEqual({ examined: 1, filled: 1, next_after: null });
  await expect(fill.getByRole("status")).toHaveText("补填完成。本次已确认检查 1 条，补填 1 条。");
  expect(await history.charge()).toMatchObject(after);
  expect(await history.balance()).toBe(7_200_000);

  const again = response("prepare");
  const repeated = response("after");
  await fill.getByRole("button", { name: "补填历史成本", exact: true }).click();
  const second = await (await again).json();
  expect(second).toEqual({ examined: 0, filled: 0, next_after: expect.any(String) });
  expect(second.next_after).not.toBe(first.next_after);
  expect(await (await repeated).json()).toEqual({ examined: 1, filled: 0, next_after: null });
  await expect(fill.getByRole("status")).toHaveText("补填完成。本次已确认检查 1 条，补填 0 条。");
  expect(posted).toEqual([
    { prepare: true }, { after: first.next_after }, { after: first.next_after },
    { prepare: true }, { after: second.next_after },
  ]);
  expect(await history.charge()).toMatchObject(after);
  expect(await history.balance()).toBe(7_200_000);
});

test("可编辑费率的运营人员看不到补填操作，控制面拒绝直接补填", async ({ page }) => {
  const detail = await openAccount(page, operator);
  await expect(detail.getByRole("button", { name: "保存默认成本换算率" })).toBeVisible();
  await expect(detail.getByRole("button", { name: "补填历史成本", exact: true })).toHaveCount(0);
  const status = await page.evaluate(async (path) => (await fetch(path, {
    method: "POST", headers: { "content-type": "application/json" }, body: "{}",
  })).status, `/admin/provider-keys/${accountId}/cost-backfill`);
  expect(status).toBe(403);
});
