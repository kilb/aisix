import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";
import { chat, seedTenant, tenantClient, type BillingWorld } from "./world";

/**
 * 计费 · 组织额度: operators allocate, change and remove a tenant's
 * organization-level quota from its panel. Real control plane, real gateway;
 * the upstream stand-in is the only fake.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

async function login(page: Page, email = stack.operator.email, password = stack.operator.password): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令").fill(password);
  await page.getByRole("button", { name: "登录" }).click();
  await page.locator('[data-tab="billing"]').click();
}

async function openPanel(page: Page, world: BillingWorld): Promise<Locator> {
  await page
    .getByRole("table", { name: "账户列表" })
    .locator("tbody tr")
    .filter({ hasText: world.tenantName })
    .getByRole("button", { name: world.tenantName })
    .click();
  return page.getByRole("dialog");
}

const region = (panel: Locator, name: string): Locator => panel.getByRole("region", { name, exact: true });
const cell = (row: Locator, column: number): Locator => row.getByRole("cell").nth(column);
const orgRows = (panel: Locator): Locator => region(panel, "组织额度").getByRole("row").filter({ has: panel.page().getByRole("cell") });

async function budgets(world: BillingWorld): Promise<Record<string, any>[]> {
  return (await stack.admin.get(`/admin/tenants/${world.tenantId}/budgets`)).items;
}

test("运维为组织分配月度额度，用尽后该组织所有密钥被拒", async ({ page }) => {
  const world = await seedTenant(stack, { mode: "prepaid", creditCny: "10.00" });
  const owner = await tenantClient(stack, world);
  const second = await owner.post("/api/keys", { label: "second-key" });
  const key2: string = second.plaintext;

  await login(page);
  const panel = await openPanel(page, world);
  const org = region(panel, "组织额度");
  await expect(org.getByText("这个租户没有组织额度。")).toBeVisible();

  // A malformed limit is refused before anything is sent, and holds focus.
  await org.getByLabel("限额（元）").fill("0.001");
  await org.getByRole("button", { name: "新建", exact: true }).click();
  await expect(org.getByText("请输入金额，最多两位小数")).toBeVisible();
  await expect(org.getByLabel("限额（元）")).toBeFocused();
  expect(await budgets(world)).toHaveLength(0);

  // 周期 defaults to 每月.
  await expect(org.locator("select").nth(1)).toHaveValue("month");
  await org.getByLabel("限额（元）").fill("0.02");
  await org.getByRole("button", { name: "新建", exact: true }).click();
  const row = orgRows(panel);
  await expect(row).toHaveCount(1);
  await expect(cell(row, 0)).toHaveText("按周期");
  await expect(cell(row, 1)).toHaveText("每月");
  await expect(cell(row, 2)).toHaveText("¥0.02");
  await expect(cell(row, 3)).toHaveText("¥0.00");
  await expect(cell(row, 4)).toHaveText("¥0.02");
  await expect(cell(row, 5)).toHaveText("正常");
  // The read-only 额度 region follows the write without a reopen.
  const allQuotas = region(panel, "额度").getByRole("row").filter({ has: panel.page().getByRole("cell") });
  await expect(allQuotas).toHaveCount(1);
  await expect(cell(allQuotas, 0)).toHaveText("组织");
  await expect(cell(allQuotas, 2)).toHaveText("平台");
  const stored = await budgets(world);
  expect(stored).toHaveLength(1);
  expect(stored[0]).toMatchObject({ scope: "team", set_by: "operator", kind: "periodic", period: "month", limit_micro_cny: 20_000 });

  // The same kind and period twice is refused, and says so.
  await org.getByLabel("限额（元）").fill("0.03");
  await org.getByRole("button", { name: "新建", exact: true }).click();
  await expect(org.getByText("该租户已有同类型、同周期的组织额度，请直接修改限额")).toBeVisible();
  expect(await budgets(world)).toHaveLength(1);

  // Two calls on two keys spend 0.022, past the 0.02 limit.
  expect(await chat(stack, world.key.plaintext, world.model.name)).toBe(200);
  await expect.poll(() => chat(stack, key2, world.model.name), { timeout: 30_000 }).toBe(200);
  for (const key of [world.key.plaintext, key2]) {
    await expect.poll(() => chat(stack, key, world.model.name), { timeout: 15_000 }).toBe(429);
  }

  // Reopened, the row reads 已用尽.
  await page.reload();
  await page.locator('[data-tab="billing"]').click();
  const again = await openPanel(page, world);
  const exhausted = orgRows(again);
  await expect(cell(exhausted, 5)).toHaveText("已用尽");
  await expect(cell(exhausted, 5).locator("[data-state]")).toHaveAttribute("data-state", "exhausted");
  // Calls made while the limit was still reaching the gateway were admitted and charged: 2 to 5 calls of 0.011.
  await expect(cell(exhausted, 3)).toHaveText(/^\¥0\.0(22|33|44|55)$/);
  // Past the limit, what remains is negative.
  await expect(cell(exhausted, 4)).toHaveText(/^-\¥0\.0\d+$/);
});

test("运维只能修改平台分配的额度", async ({ page }) => {
  const world = await seedTenant(stack, { mode: "prepaid", creditCny: "10.00", members: ["member"] });
  const owner = await tenantClient(stack, world);
  const member = world.members[0]!;
  // The tenant's own owner sets a member quota; the operator sets the organization's.
  await owner.post("/api/budgets", { scope: "member", scope_ref: member.id, kind: "periodic", period: "month", limit_cny: "1.50" });
  const team = await stack.admin.post(`/admin/tenants/${world.tenantId}/budgets`, { kind: "cumulative", limit_cny: "5.00" });

  await login(page);
  const panel = await openPanel(page, world);

  // The read-only 额度 region lists the member quota as the organization admin's, with no controls.
  const all = region(panel, "额度");
  const memberRow = all.getByRole("row").filter({ hasText: "成员" }).filter({ hasText: "¥1.50" });
  await expect(memberRow).toHaveCount(1);
  await expect(cell(memberRow, 2)).toHaveText("组织管理员");
  await expect(all.getByRole("button")).toHaveCount(0);

  // 组织额度 holds only the organization's own quota, and it has the controls.
  const org = region(panel, "组织额度");
  const row = orgRows(panel);
  await expect(row).toHaveCount(1);
  await expect(cell(row, 0)).toHaveText("一次性额度");
  await expect(cell(row, 2)).toHaveText("¥5.00");
  await expect(org).not.toContainText("¥1.50");
  await expect(row.getByRole("button", { name: "修改限额" })).toBeVisible();
  await expect(row.getByRole("button", { name: "删除", exact: true })).toBeVisible();

  // The backend agrees: a tenant-set quota is not the operator's to change.
  const refused = await stack.admin.request("PATCH", `/admin/tenants/${world.tenantId}/budgets/${(await budgets(world)).find((b) => b.scope === "member")!.id}`, {
    limit_cny: "9.00",
  });
  expect(refused.status).toBe(403);
  expect(refused.body.code).toBe("budget_managed");

  // 修改限额: a bad value is refused and keeps focus; a good one is saved.
  await row.getByRole("button", { name: "修改限额" }).click();
  await org.getByLabel("新限额（元）").fill("0");
  await org.getByRole("button", { name: "保存", exact: true }).click();
  await expect(org.getByText("请输入金额，最多两位小数")).toBeVisible();
  await expect(org.getByLabel("新限额（元）")).toBeFocused();
  await org.getByLabel("新限额（元）").fill("7.25");
  await org.getByRole("button", { name: "保存", exact: true }).click();
  await expect(cell(orgRows(panel), 2)).toHaveText("¥7.25");
  expect((await budgets(world)).find((b) => b.id === team.id)!.limit_micro_cny).toBe(7_250_000);

  // 删除 asks twice; cancelling leaves the quota.
  await orgRows(panel).getByRole("button", { name: "删除", exact: true }).click();
  await orgRows(panel).getByRole("button", { name: "取消" }).click();
  expect((await budgets(world)).some((b) => b.id === team.id)).toBe(true);
  await orgRows(panel).getByRole("button", { name: "删除", exact: true }).click();
  await orgRows(panel).getByRole("button", { name: "确认删除" }).click();
  await expect(org.getByText("这个租户没有组织额度。")).toBeVisible();
  expect((await budgets(world)).some((b) => b.id === team.id)).toBe(false);
  // The 额度 region no longer lists the deleted quota (the member quota stays).
  await expect(all.getByRole("row").filter({ hasText: "¥5.00" })).toHaveCount(0);
  await expect(all.getByRole("row").filter({ hasText: "¥7.25" })).toHaveCount(0);
  await expect(memberRow).toHaveCount(1);
  // The member quota is untouched.
  expect((await budgets(world)).some((b) => b.scope === "member")).toBe(true);
});

test("只读运维看不到组织额度的编辑控件", async ({ page }) => {
  const world = await seedTenant(stack, { mode: "prepaid", creditCny: "10.00" });
  await stack.admin.post(`/admin/tenants/${world.tenantId}/budgets`, { kind: "periodic", period: "week", limit_cny: "3.00" });
  const viewer = { email: "viewer-orgquota@e2e.local", password: "viewer-orgquota-pass-4k9" };
  await stack.admin.post("/admin/operators", { ...viewer, role: "viewer" });
  await login(page, viewer.email, viewer.password);
  const panel = await openPanel(page, world);
  const org = region(panel, "组织额度");
  const row = orgRows(panel);
  await expect(row).toHaveCount(1);
  await expect(cell(row, 1)).toHaveText("每周（周一起）");
  await expect(cell(row, 2)).toHaveText("¥3.00");
  await expect(org.getByRole("button")).toHaveCount(0);
  await expect(org.getByLabel("限额（元）")).toHaveCount(0);
});

test("切换租户后，面板里不再出现上一个租户的额度", async ({ page }) => {
  const a = await seedTenant(stack, { mode: "prepaid", creditCny: "10.00" });
  const b = await seedTenant(stack, { mode: "prepaid", creditCny: "10.00" });
  await stack.admin.post(`/admin/tenants/${a.tenantId}/budgets`, { kind: "cumulative", limit_cny: "7.77" });
  await login(page);
  const panelA = await openPanel(page, a);
  await expect(orgRows(panelA)).toHaveCount(1);
  await expect(panelA).toContainText("¥7.77");
  const panelB = await openPanel(page, b);
  await expect(panelB.getByRole("region", { name: "组织额度", exact: true }).getByText("这个租户没有组织额度。")).toBeVisible();
  await expect(panelB).not.toContainText("¥7.77");
});
