import { randomUUID } from "node:crypto";
import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "./stack";
import { chat, seedTenant, type BillingWorld } from "./world";

/**
 * 价格 · 计价状态 and the 概览 card: unpriced calls by reason. Real gateway and
 * control plane; the upstream stand-in is the only fake. Expected counts come
 * from the scenario (two calls to a model with no price).
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

async function login(page: Page): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(stack.operator.email);
  await page.getByLabel("口令").fill(stack.operator.password);
  await page.getByRole("button", { name: "登录" }).click();
}

test("未定价模型上的调用出现在计价状态里，按原因计数，概览页同时提示", async ({ page }) => {
  const world = await seedTenant(stack, { unpricedModel: true });
  expect(await chat(stack, world.key.plaintext, world.laterModel!.name)).toBe(200);
  expect(await chat(stack, world.key.plaintext, world.laterModel!.name)).toBe(200);
  expect(await chat(stack, world.key.plaintext, world.model.name)).toBe(200);
  await expect
    .poll(async () => {
      const r = (await stack.admin.get(`/admin/pricing/charges?tenant_id=${world.tenantId}`)) as { items: Json[] };
      return r.items.length;
    }, { timeout: 60_000 })
    .toBe(3);

  await login(page);
  // 概览: the card shows the count and links to the status page.
  const card = page.getByRole("region", { name: "计价异常" });
  await expect(card.locator(".entry", { hasText: "未计价调用" }).locator(".val")).toHaveText("2");
  await expect(card.locator(".entry", { hasText: "不可用的价格版本" }).locator(".val")).toHaveText("0");
  await card.getByRole("button", { name: /计价状态/ }).click();

  // 计价状态 opens directly, by reason.
  await expect(page.getByRole("heading", { name: "计价状态" })).toBeVisible();
  const table = page.getByRole("table", { name: "未计价的调用" });
  await expect(table.getByRole("columnheader")).toContainText(["原因", "次数"]);
  await expect(table.getByRole("row", { name: /^未定价/ }).getByRole("cell").nth(1)).toHaveText("2");
  await expect(table.getByRole("row", { name: /^合计/ })).toContainText("2");
  await expect(page.getByText("价格版本", { exact: false }).first()).toBeVisible();
  await expect(page.getByText("没有不可用的价格版本。")).toBeVisible();
});

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** An organisation with a model of its own that has no price, granted and listed to its key. */
async function ownUnpricedModel(): Promise<{ world: BillingWorld; model: { id: string; name: string } }> {
  const world = await seedTenant(stack);
  const suffix = randomUUID().slice(0, 8);
  const account = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: `acct-st-${suffix}`,
      provider: "openai",
      api_key: `sk-acct-st-${suffix}`,
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups: ["shared"],
    },
  });
  const name = `st-model-${suffix}`;
  const made = await stack.admin.post("/admin/resources/models", {
    doc: { display_name: name, provider: "openai", model_name: `upstream-st-${suffix}`, provider_key_id: account.id },
  });
  const tenant = await stack.admin.get(`/admin/tenants/${world.tenantId}`);
  await stack.admin.patch(`/admin/tenants/${world.tenantId}`, { allowed_model_ids: [...tenant.allowed_model_ids, made.id] }, tenant.version);
  await expect
    .poll(
      async () => {
        const res = await fetch(`${stack.proxyUrl}/v1/models`, { headers: { authorization: `Bearer ${world.key.plaintext}` } });
        const body = (await res.json()) as { data?: { id: string }[] };
        return (body.data ?? []).some((m) => m.id === name);
      },
      { timeout: 60_000 },
    )
    .toBe(true);
  return { world, model: { id: made.id as string, name } };
}

test("每种未计价写明模型和处理办法，可直接打开对应的计费明细；重计价更正后单独计数", async ({ page }) => {
  const { world, model } = await ownUnpricedModel();
  expect(await chat(stack, world.key.plaintext, model.name)).toBe(200);
  expect(await chat(stack, world.key.plaintext, model.name)).toBe(200);
  await expect
    .poll(async () => ((await stack.admin.get(`/admin/pricing/charges?tenant_id=${world.tenantId}`)) as { items: Json[] }).items.length, {
      timeout: 60_000,
    })
    .toBe(2);

  await login(page);
  await page.getByRole("tab", { name: "价格", exact: true }).click();
  await page.getByRole("button", { name: "计价状态" }).click();
  const table = page.getByRole("table", { name: "未计价的调用" });
  await expect(table.getByRole("columnheader")).toContainText(["原因", "次数", "模型", "档位 / 分组", "最近一次（UTC）", "处理"]);
  // One row for this model: its reason, count, name and what to do about it.
  const row = table.getByRole("row").filter({ hasText: model.name });
  await expect(row).toHaveCount(1);
  await expect(row.getByRole("cell").nth(0)).toHaveText("未定价");
  await expect(row.getByRole("cell").nth(1)).toHaveText("2");
  await expect(row.getByRole("cell").nth(2)).toHaveText(model.name);
  await expect(row.getByRole("cell").nth(5)).toHaveText("该模型在调用时没有生效的基准价；在「基准价」补录后用重计价更正");

  // The row's link opens 计费明细 on exactly those calls.
  await row.getByRole("button", { name: "查看调用" }).click();
  await expect(page.getByRole("heading", { name: "计费明细" })).toBeVisible();
  await expect(page.getByRole("status").filter({ hasText: "只看" })).toContainText(`原因：未定价，模型：${model.name}`);
  // The list cannot narrow to the row's window or leave corrected calls out, and says so.
  await expect(page.getByRole("status").filter({ hasText: "只看" })).toContainText("不限分组/档位与时间");
  const charges = page.getByRole("table", { name: "计费明细" });
  await expect(charges.getByRole("row")).toHaveCount(3);
  for (const r of await charges.getByRole("row").all()) {
    if ((await r.getByRole("cell").count()) === 0) continue;
    await expect(r.getByRole("cell").nth(2)).toHaveText(model.name);
    await expect(r.getByRole("cell").nth(4)).toHaveText("未定价");
  }

  // A price, then a run that charges both calls: they leave the unpriced count and are counted apart.
  const startsAt = Date.now() + 3_000;
  await stack.admin.post("/admin/pricing/baselines", {
    model_id: model.id,
    effective_from: new Date(startsAt).toISOString(),
    card: { default: { base: { input: "1000", output: "2000" } } },
    note: "the status test's late card",
  });
  await sleep(Math.max(0, startsAt - Date.now()) + 300);
  const run = await stack.admin.post("/admin/billing/repricing", {
    from: new Date(Date.now() - 3_600_000).toISOString(),
    to: new Date().toISOString(),
    model_id: model.id,
    tenant_id: world.tenantId,
    status: "unpriced",
    reason: "计价状态-更正-ui",
  });
  expect(run.changed).toBe(2);
  await stack.admin.post(`/admin/billing/repricing/${run.id}/apply`, {});

  await page.getByRole("button", { name: "刷新" }).click();
  await expect(charges.getByRole("row").nth(1).getByRole("cell").nth(4)).toHaveText("未定价 · 已通过重计价更正");
  await page.getByRole("button", { name: "计价状态" }).click();
  await expect(page.getByRole("status").filter({ hasText: "已通过重计价更正" })).toContainText("另有 2 次调用已通过重计价更正");
  await expect(page.getByRole("table", { name: "未计价的调用" }).getByRole("row").filter({ hasText: model.name })).toHaveCount(0);
});
