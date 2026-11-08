import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "./stack";
import { CALL_MICRO_USD, chat, seedTenant } from "./world";

/**
 * 价格 · 计费明细: every charge with its pricing reason. Real gateway, real
 * control plane; the upstream stand-in is the only fake. Expected values come
 * from the scenario (which model was called, by whom), not from the product.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

async function openCharges(page: Page): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(stack.operator.email);
  await page.getByLabel("口令").fill(stack.operator.password);
  await page.getByRole("button", { name: "登录" }).click();
  await page.getByRole("tab", { name: "价格", exact: true }).click();
  await page.getByRole("button", { name: "计费明细" }).click();
}

/** Waits until the control plane has priced `count` charges for the tenant. */
async function chargesFor(tenantId: string, count: number): Promise<Json[]> {
  let items: Json[] = [];
  await expect
    .poll(async () => {
      items = ((await stack.admin.get(`/admin/pricing/charges?tenant_id=${tenantId}`)) as { items: Json[] }).items;
      return items.length;
    }, { timeout: 60_000 })
    .toBe(count);
  return items;
}

test("计费明细按租户与状态筛选", async ({ page }) => {
  const world = await seedTenant(stack, { unpricedModel: true });
  const other = await seedTenant(stack);
  expect(world.laterModel).toBeDefined();
  expect(await chat(stack, world.key.plaintext, world.model.name)).toBe(200);
  expect(await chat(stack, world.key.plaintext, world.laterModel!.name)).toBe(200);
  expect(await chat(stack, other.key.plaintext, other.model.name)).toBe(200);
  const mine = await chargesFor(world.tenantId, 2);
  const pricedId = mine.find((c) => c.requested_model_id === world.model.id)!.event_id as string;
  await chargesFor(other.tenantId, 1);

  await openCharges(page);
  const table = page.getByRole("table", { name: "计费明细" });
  await expect(table.getByRole("columnheader")).toContainText(["时间（UTC）", "租户", "模型", "状态", "原因", "售价", "成本", "档位"]);

  // Tenant filter: only this organisation's two rows.
  await page.getByLabel("租户").selectOption({ label: world.tenantName });
  await expect(table.getByRole("row")).toHaveCount(3);
  await expect(table.getByRole("row", { name: new RegExp(other.tenantName) })).toHaveCount(0);
  const priced = table.getByRole("row", { name: new RegExp(`${world.model.name}(?!-)`) });
  await expect(priced.getByRole("cell").nth(5)).toHaveText("$0.011");
  expect(CALL_MICRO_USD).toBe(11_000);
  await expect(priced).toContainText("已计价");

  // Status filter: only the unpriced one, with its reason.
  await page.getByLabel("状态").selectOption({ label: "未计价" });
  await expect(table.getByRole("row")).toHaveCount(2);
  const row = table.getByRole("row").nth(1);
  await expect(row).toContainText(world.laterModel!.name);
  await expect(row.getByRole("cell").nth(4)).toHaveText("未定价");
  await expect(row.getByRole("cell").nth(5)).toHaveText("—");

  // Event filter: one known call, with the status filter cleared.
  await page.getByLabel("状态").selectOption({ label: "全部" });
  await page.getByLabel("事件").fill(pricedId);
  await page.getByRole("button", { name: "查找" }).click();
  await expect(table.getByRole("row")).toHaveCount(2);
  await expect(table.getByRole("row").nth(1)).toContainText(world.model.name);

  // A key filter within the tenant.
  await page.getByLabel("事件").fill("");
  await page.getByRole("button", { name: "查找" }).click();
  await page.getByLabel("密钥").selectOption({ index: 1 });
  await expect(table.getByRole("row")).toHaveCount(3);
});

test("加载更多按 next_before 续页", async ({ page }) => {
  test.setTimeout(240_000);
  const world = await seedTenant(stack);
  // One more than a page of 50, sent in parallel batches.
  const total = 52;
  for (let sent = 0; sent < total; sent += 4) {
    const statuses = await Promise.all(
      Array.from({ length: Math.min(4, total - sent) }, () => chat(stack, world.key.plaintext, world.model.name)),
    );
    expect(statuses).toEqual(statuses.map(() => 200));
  }
  await chargesFor(world.tenantId, 50);
  await expect
    .poll(async () => {
      const p = (await stack.admin.get(`/admin/pricing/charges?tenant_id=${world.tenantId}&limit=500`)) as { items: Json[] };
      return p.items.length;
    }, { timeout: 60_000 })
    .toBe(total);

  await openCharges(page);
  await page.getByLabel("租户").selectOption({ label: world.tenantName });
  const table = page.getByRole("table", { name: "计费明细" });
  await expect(table.getByRole("row")).toHaveCount(51);
  await page.getByRole("button", { name: "加载更多" }).click();
  await expect(table.getByRole("row")).toHaveCount(total + 1);
  await expect(page.getByRole("button", { name: "加载更多" })).toHaveCount(0);
});
