import { expect, test, type Browser, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "./stack";
import { rejectTopup, seedTenant, tenantClient, type BillingWorld } from "./world";

/**
 * 「计费 · 充值审批」 against the real control plane through the built
 * `dist/`. Tenants file their requests through the real `/api`; the page
 * decides them. Each test finds its own request by its own tenant's name.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

async function login(page: Page, email: string, password: string): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令", { exact: true }).fill(password);
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
}

/** Signs in as `email` and opens 计费 · 充值审批. */
async function openTopups(page: Page, email = stack.operator.email, password = stack.operator.password): Promise<void> {
  await login(page, email, password);
  await page.locator('[data-tab="billing"]').click();
  // 计费 now has more than one section; 账户 is first.
  await page.getByRole("group", { name: "计费分区" }).getByRole("button", { name: "充值审批" }).click();
  await expect(page.getByRole("heading", { name: "充值审批" })).toBeVisible();
  await expect(page.locator("main").getByText("载入中…")).toHaveCount(0);
}

/** A funded-with-nothing organisation whose owner has asked for `amount` dollars. */
async function requested(amount: string, note: string): Promise<{ world: BillingWorld; topupId: string }> {
  const world = await seedTenant(stack, { creditCny: "0" });
  const tenant = await tenantClient(stack, world);
  const made: Json = await tenant.post("/api/billing/topups", { amount_cny: amount, note, reference: `ref-${note}` });
  return { world, topupId: made.id as string };
}

const rowOf = (page: Page, world: BillingWorld) => page.getByRole("row").filter({ hasText: world.tenantName });

async function balance(world: BillingWorld): Promise<Json[]> {
  return (await stack.admin.get(`/admin/tenants/${world.tenantId}/billing/ledger`)).items;
}

test("批准充值需二次确认，批准后租户余额与流水更新", async ({ page }) => {
  const { world } = await requested("0.05", "批准-ui");
  await openTopups(page);
  const row = rowOf(page, world);
  await expect(row).toContainText("¥0.05");
  await expect(row).toContainText("批准-ui");
  await expect(row).toContainText("租户申请");

  await row.getByRole("button", { name: "批准", exact: true }).click();
  // The first click only asks: nothing is posted yet.
  await expect(page.getByRole("alert").filter({ hasText: "批准后金额立即计入该租户余额" })).toBeVisible();
  expect(await balance(world)).toHaveLength(0);

  await row.getByRole("button", { name: "确认批准" }).click();
  await expect(row).toHaveCount(0);
  await expect(page.getByRole("status").filter({ hasText: "已批准" })).toContainText("¥0.05");

  const entries = await balance(world);
  expect(entries).toHaveLength(1);
  expect(entries[0].kind).toBe("topup");
  expect(entries[0].amount_micro_cny).toBe(50_000);
  expect(entries[0].balance_after_micro_cny).toBe(50_000);

  // 已批准 lists it with its outcome and no controls.
  await page.getByLabel("状态").selectOption("approved");
  await expect(row).toContainText("已批准");
  await expect(row.getByRole("button")).toHaveCount(0);
});

test("驳回必须填写原因，租户看到原因", async ({ page }) => {
  const { world } = await requested("0.07", "驳回-ui");
  await openTopups(page);
  const row = rowOf(page, world);
  await row.getByRole("button", { name: "驳回", exact: true }).click();

  const reason = page.getByLabel("驳回原因");
  await expect(reason).toBeVisible();
  await page.getByRole("button", { name: "确认驳回" }).click();
  // Refused on the page: nothing was sent, the field is flagged and holds focus.
  await expect(page.getByText("请填写驳回原因")).toBeVisible();
  await expect(reason).toBeFocused();
  await expect(reason).toHaveAttribute("aria-invalid", "true");
  const pending = (await stack.admin.get("/admin/billing/topups?status=pending")).items as Json[];
  expect(pending.some((t) => t.tenant_id === world.tenantId)).toBe(true);

  // A pasted tab survives a single-line field; the server refuses it on the field.
  await reason.fill("未收到\t款项");
  await page.getByRole("button", { name: "确认驳回" }).click();
  await expect(reason).toHaveAttribute("aria-invalid", "true");
  await expect(reason).toBeFocused();
  await expect(page.locator(".field-problem")).toContainText("控制字符");

  await reason.fill("未收到款项-ui");
  await page.getByRole("button", { name: "确认驳回" }).click();
  await expect(rowOf(page, world)).toHaveCount(0);
  await expect(page.getByRole("status").filter({ hasText: "已驳回" })).toBeVisible();

  const mine = (await (await tenantClient(stack, world)).get("/api/billing/topups")).items as Json[];
  expect(mine).toHaveLength(1);
  expect(mine[0].status).toBe("rejected");
  expect(mine[0].reject_reason).toBe("未收到款项-ui");
  expect(await balance(world)).toHaveLength(0);
});

async function operatorPage(browser: Browser, world: BillingWorld): Promise<Page> {
  const context = await browser.newContext();
  const page = await context.newPage();
  await openTopups(page);
  await expect(rowOf(page, world)).toBeVisible();
  return page;
}

test("两位运维同时处理同一申请，后到的一方看到已被处理", async ({ browser }) => {
  const { world } = await requested("0.09", "并发-ui");
  const a = await operatorPage(browser, world);
  const b = await operatorPage(browser, world);

  await rowOf(a, world).getByRole("button", { name: "批准", exact: true }).click();
  await rowOf(a, world).getByRole("button", { name: "确认批准" }).click();
  await expect(rowOf(a, world)).toHaveCount(0);

  // B still shows it pending, and approves too.
  await rowOf(b, world).getByRole("button", { name: "批准", exact: true }).click();
  await rowOf(b, world).getByRole("button", { name: "确认批准" }).click();
  await expect(b.getByRole("alert").filter({ hasText: "该申请已被处理，请刷新" })).toBeVisible();
  // The page then shows what is stored: the request has left 待审批.
  await expect(rowOf(b, world)).toHaveCount(0);

  const entries = await balance(world);
  expect(entries.filter((e) => e.kind === "topup")).toHaveLength(1);
  expect(entries[0].balance_after_micro_cny).toBe(90_000);
});

test("有待审批的申请时，计费在导航上有提示", async ({ page }) => {
  const open = (await stack.admin.get("/admin/billing/topups?status=pending")).items as Json[];
  expect(open, "no earlier test left a request pending").toHaveLength(0);

  const { world } = await requested("0.03", "标记-ui");
  await login(page, stack.operator.email, stack.operator.password);
  const tab = page.locator('[data-tab="billing"]');
  await expect(tab).toHaveAttribute("aria-describedby", "attention-billing");
  await expect(page.locator("#attention-billing")).toHaveText("有待处理事项");
  // The marker adds nothing to the tab's own text.
  await expect(tab).toHaveText("计费");

  await tab.click();
  await page.getByRole("group", { name: "计费分区" }).getByRole("button", { name: "充值审批" }).click();
  await rowOf(page, world).getByRole("button", { name: "批准", exact: true }).click();
  await rowOf(page, world).getByRole("button", { name: "确认批准" }).click();
  await expect(rowOf(page, world)).toHaveCount(0);
  await expect(tab).not.toHaveAttribute("aria-describedby", /.+/);
  await expect(page.locator("#attention-billing")).toHaveCount(0);
});

test("只读运维看不到批准与驳回", async ({ page }) => {
  const { world, topupId } = await requested("0.04", "只读-ui");
  const viewer = { email: "viewer-topups@e2e.local", password: "viewer-topups-pass-6d2" };
  await stack.admin.post("/admin/operators", { ...viewer, role: "viewer" });
  await openTopups(page, viewer.email, viewer.password);

  const row = rowOf(page, world);
  await expect(row).toContainText("¥0.04");
  await expect(row.getByRole("button")).toHaveCount(0);
  await expect(page.getByRole("button", { name: /^(批准|驳回)/ })).toHaveCount(0);

  // The control plane agrees: a viewer's decision is refused.
  const refused = await page.evaluate(async (id) => {
    const res = await fetch(`/admin/billing/topups/${id}/approve`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: "{}",
    });
    return res.status;
  }, topupId);
  expect(refused).toBe(403);
  await rejectTopup(stack, topupId, "清理-ui");
});
