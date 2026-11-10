import { randomUUID } from "node:crypto";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "./stack";

/**
 * 价格 · 成本价 against the real control plane through the built `dist/`.
 * Accounts, public models, prices and the tenant are set up through the
 * real `/admin` and `/api`, the call goes through the real gateway to the
 * upstream stand-in, and nothing is stubbed.
 *
 * Every figure asserted comes from the rates the test itself sets. The
 * stand-in reports 5 prompt and 3 completion tokens; at a baseline of 10 000
 * USD per million input and 30 000 per million output that is 140 000 µ$ of
 * official price a call. The group's rate of 7 charges ¥0.98 for it; the
 * account's cost rate of 0.3 costs ¥0.042, leaving ¥0.938, a margin of
 * 95.71 %; an override of 0.2 costs ¥0.028, leaving ¥0.952, 97.14 %.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

const PASSWORD = "cost-rate-pass-4d2";

async function login(page: Page, who = stack.operator): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(who.email);
  await page.getByLabel("口令", { exact: true }).fill(who.password);
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
}

/** Opens a section of 价格 and waits until what it lists is read. */
async function openSection(page: Page, label: string): Promise<void> {
  await page.locator('[data-tab="pricing"]').click();
  await page.getByRole("group", { name: "价格分区" }).getByRole("button", { name: label, exact: true }).click();
  await expect(page.locator("main").getByText("载入中…")).toHaveCount(0);
}

/** The account list's row for the account shown as `name`. */
function accountRow(page: Page, name: string): Locator {
  return page
    .getByRole("table", { name: "上游账号成本价" })
    .getByRole("row")
    .filter({ has: page.getByRole("button", { name, exact: true }) });
}

/** Opens an account's detail from the list, and answers the region that holds it. */
async function openAccount(page: Page, name: string): Promise<Locator> {
  await accountRow(page, name).getByRole("button", { name, exact: true }).click();
  const detail = page.getByRole("region", { name: `上游账号 ${name}` });
  await expect(detail).toBeVisible();
  await expect(page.locator("main").getByText("载入中…")).toHaveCount(0);
  return detail;
}

/** The account detail's row for `model`. */
function modelRow(page: Page, model: string): Locator {
  return page.getByRole("table", { name: "账号成本换算率" }).getByRole("row").filter({ hasText: model });
}

/** A group, an account serving only it, and two public models ticked on the account, each with the test's baseline. */
async function catalogue(): Promise<{ group: Json; account: { id: string; name: string }; models: { id: string; name: string }[] }> {
  const suffix = randomUUID().slice(0, 8);
  const group = await stack.admin.createGroup(`grp-cost-${suffix}`, `成本分组 ${suffix}`, "7");
  const accountName = `acct-cost-${suffix}`;
  const key = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: accountName,
      provider: "openai",
      api_key: `sk-acct-cost-${suffix}-upstream`,
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups: [group.name],
    },
  });
  const models = [];
  for (const prefix of ["pub-cost-a", "pub-cost-b"]) {
    const name = `${prefix}-${suffix}`;
    const pub = await stack.admin.post("/admin/public-models", { name });
    await stack.admin.put(`/admin/provider-keys/${key.id}/channel-models/${pub.id}`, { priority: 1, weight: 1 });
    await stack.admin.post("/admin/pricing/baselines", {
      model_id: pub.id,
      effective_from: new Date().toISOString(),
      card: { default: { base: { input: "10000", output: "30000" } } },
      note: "成本价用例的官方价",
    });
    models.push({ id: pub.id as string, name });
  }
  return { group, account: { id: key.id as string, name: accountName }, models };
}

/** A funded tenant on `group` with one key, which has reached the gateway; its plaintext. */
async function caller(group: Json): Promise<string> {
  const suffix = randomUUID().slice(0, 8);
  const owner = { email: `owner-cost-${suffix}@cost.test`, password: PASSWORD };
  const signedUp = await stack.tenant().post("/api/signup", {
    org_name: `成本价租户 ${suffix}`,
    email: owner.email,
    password: owner.password,
  });
  const tenantId = signedUp.tenant.id as string;
  await stack.admin.post(`/admin/tenants/${tenantId}/approve`);
  const current = await stack.admin.get(`/admin/tenants/${tenantId}`);
  await stack.admin.patch(`/admin/tenants/${tenantId}`, { allowed_provider_groups: [group.name] }, current.version);
  await stack.admin.post(`/admin/tenants/${tenantId}/credits`, { amount_cny: "10.00", reason: "成本价用例" });
  const api = stack.tenant();
  await api.post("/api/login", owner);
  const created = await api.post("/api/keys", { label: "成本价用例" });
  const plaintext = created.plaintext as string;
  const deadline = Date.now() + 60_000;
  for (;;) {
    const res = await fetch(`${stack.proxyUrl}/v1/models`, { headers: { authorization: `Bearer ${plaintext}` } });
    await res.arrayBuffer();
    if (res.status === 200) break;
    if (Date.now() >= deadline) throw new Error(`the gateway still answers ${res.status} to the new key`);
    await new Promise((r) => setTimeout(r, 250));
  }
  return plaintext;
}

async function chat(key: string, model: string): Promise<void> {
  const res = await fetch(`${stack.proxyUrl}/v1/chat/completions`, {
    method: "POST",
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    body: JSON.stringify({ model, messages: [{ role: "user", content: "hi" }] }),
  });
  await res.arrayBuffer();
  expect(res.status).toBe(200);
}

test("成本价按账号设置默认与覆盖，调用的成本和利润出现在分组详情", async ({ page }) => {
  const { group, account, models } = await catalogue();
  const [plain, overridden] = models as [{ id: string; name: string }, { id: string; name: string }];
  await login(page);
  await openSection(page, "成本价");

  // No rate yet: the account is listed, and says so.
  await expect(accountRow(page, account.name).locator("td").nth(2)).toHaveText("未设置");
  const detail = await openAccount(page, account.name);
  for (const m of models) {
    await expect(modelRow(page, m.name)).toContainText("默认");
    await expect(modelRow(page, m.name)).toContainText("未设置");
  }

  // The account's default.
  const form = detail.getByRole("group", { name: "修改默认成本换算率" });
  await form.getByLabel("新默认成本换算率").fill("0.3");
  await form.getByRole("button", { name: "保存默认成本换算率" }).click();
  await expect(page.getByText("已保存").first()).toBeVisible();
  await expect(form.locator(".grp-now")).toHaveText("0.3");
  await expect(modelRow(page, plain.name).getByRole("cell").nth(3)).toHaveText("默认 0.3");

  // An override on the second model beats it.
  await modelRow(page, overridden.name).getByRole("button", { name: "设置覆盖" }).click();
  const override = detail.getByRole("group", { name: "设置覆盖" });
  await override.getByLabel("成本换算率").fill("0.2");
  await override.getByRole("button", { name: "保存覆盖" }).click();
  await expect(modelRow(page, overridden.name).getByRole("cell").nth(3)).toHaveText("覆盖 0.2");
  await expect(modelRow(page, plain.name).getByRole("cell").nth(3)).toHaveText("默认 0.3");

  // Back on the list, the account's default is shown.
  await detail.getByRole("button", { name: "返回成本价" }).click();
  await expect(accountRow(page, account.name).locator("td").nth(2)).toHaveText("0.3");

  // One real call to each model, through the gateway.
  const key = await caller(group);
  await chat(key, plain.name);
  await chat(key, overridden.name);

  // The charges land asynchronously; the page reads the margin route, so
  // wait for both rows there rather than for a timer.
  const now = new Date();
  const from = new Date(now.getTime() - 30 * 86_400_000);
  const query = `from=${from.toISOString()}&to=${new Date(now.getTime() + 60_000).toISOString()}&group_id=${group.id}`;
  await expect
    .poll(
      async () => {
        const margin = (await stack.admin.get(`/admin/pricing/margin?${query}`)) as Json;
        return margin.items.filter((i: Json) => i.rows === 1).length;
      },
      { timeout: 60_000 },
    )
    .toBe(2);

  await openSection(page, "分组");
  await page.getByRole("table", { name: "分组" }).getByRole("button", { name: group.display_name, exact: true }).click();
  const groupDetail = page.getByRole("region", { name: `分组 ${group.display_name}` });
  await expect(groupDetail).toBeVisible();
  const marginCell = (model: string) =>
    page.getByRole("table", { name: "分组换算率" }).getByRole("row").filter({ hasText: model }).getByRole("cell").nth(7);
  await expect(marginCell(plain.name)).toContainText("95.71%");
  await expect(marginCell(plain.name)).toContainText("售价 ¥0.98，成本 ¥0.042，利润 ¥0.938");
  await expect(marginCell(overridden.name)).toContainText("97.14%");
  await expect(marginCell(overridden.name)).toContainText("售价 ¥0.98，成本 ¥0.028，利润 ¥0.952");

  // Ending the override puts the default back on the model.
  await openSection(page, "成本价");
  await openAccount(page, account.name);
  await modelRow(page, overridden.name).getByRole("button", { name: "结束覆盖" }).click();
  await page.getByRole("group", { name: "结束覆盖" }).getByRole("button", { name: "结束覆盖" }).click();
  await expect(modelRow(page, overridden.name).getByRole("cell").nth(3)).toHaveText("默认 0.3");
});

test("成本换算率超出范围时在字段处被拒，什么也没写", async ({ page }) => {
  const { account } = await catalogue();
  await login(page);
  await openSection(page, "成本价");
  const detail = await openAccount(page, account.name);
  const form = detail.getByRole("group", { name: "修改默认成本换算率" });
  await form.getByLabel("新默认成本换算率").fill("100.0001");
  await form.getByRole("button", { name: "保存默认成本换算率" }).click();
  await expect(form.getByText("换算率应为 0 到 100 之间、最多四位小数的数字")).toBeVisible();
  await expect(form.getByLabel("新默认成本换算率")).toBeFocused();
  const stored = (await stack.admin.get(`/admin/provider-keys/${account.id}/cost-rates`)) as Json;
  expect(stored.default.current).toBeNull();
});

test("只读运维看得到成本价，看不到修改入口", async ({ page }) => {
  const { account } = await catalogue();
  await stack.admin.post(`/admin/provider-keys/${account.id}/cost-rates`, {
    model_id: null,
    cny_per_usd: "0.45",
    effective_from: new Date().toISOString(),
  });
  const viewer = { email: `viewer-cost-${randomUUID().slice(0, 6)}@e2e.local`, password: "viewer-cost-pass-3c8" };
  await stack.admin.post("/admin/operators", { ...viewer, display_name: "只读", role: "viewer" });
  await login(page, viewer);
  await openSection(page, "成本价");
  await expect(accountRow(page, account.name).locator("td").nth(2)).toHaveText("0.45");
  const detail = await openAccount(page, account.name);
  await expect(detail.getByRole("group", { name: "修改默认成本换算率" })).toHaveCount(0);
  await expect(detail.getByRole("button", { name: "设置覆盖" })).toHaveCount(0);
});
