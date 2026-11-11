import { randomUUID } from "node:crypto";
import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "./stack";
import { CALL_MICRO_CNY, chat, seedTenant } from "./world";

/**
 * 计费 · 计费明细: every charge with its pricing reason, its upstream and its
 * usage (the owner moved it from 价格 to 计费 on 2026-11-11). Real gateway, real
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
  await page.getByRole("tab", { name: "计费", exact: true }).click();
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
  await expect(table.getByRole("columnheader")).toContainText([
    "时间（UTC）",
    "租户",
    "模型",
    "状态",
    "原因",
    "售价（元）",
    "成本（元）",
    "档位",
  ]);

  // Tenant filter: only this organisation's two rows.
  await page.getByLabel("租户").selectOption({ label: world.tenantName });
  await expect(table.getByRole("row")).toHaveCount(3);
  await expect(table.getByRole("row", { name: new RegExp(other.tenantName) })).toHaveCount(0);
  const priced = table.getByRole("row", { name: new RegExp(`${world.model.name}(?!-)`) });
  await expect(priced.getByRole("cell").nth(5)).toHaveText("¥0.011");
  expect(CALL_MICRO_CNY).toBe(11_000);
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

test("换筛选时不能用上一个筛选的 cursor 续页：新筛选的首页读回前没有「加载更多」，遍历覆盖全部记录", async ({ page }) => {
  test.setTimeout(240_000);
  // Filter A pages (more than one page of 50); filter B does not (12 rows).
  const a = await seedTenant(stack);
  const totalA = 52;
  for (let sent = 0; sent < totalA; sent += 4) {
    const statuses = await Promise.all(
      Array.from({ length: Math.min(4, totalA - sent) }, () => chat(stack, a.key.plaintext, a.model.name)),
    );
    expect(statuses).toEqual(statuses.map(() => 200));
  }
  const b = await seedTenant(stack);
  for (let sent = 0; sent < 12; sent += 4) {
    await Promise.all(Array.from({ length: Math.min(4, 12 - sent) }, () => chat(stack, b.key.plaintext, b.model.name)));
  }
  await expect
    .poll(async () => {
      const p = (await stack.admin.get(`/admin/pricing/charges?tenant_id=${a.tenantId}&limit=500`)) as { items: Json[] };
      return p.items.length;
    }, { timeout: 60_000 })
    .toBe(totalA);
  const bRows = await chargesFor(b.tenantId, 12);

  await openCharges(page);
  const table = page.getByRole("table", { name: "计费明细" });
  await page.getByLabel("租户").selectOption({ label: a.tenantName });
  await expect(table.getByRole("row")).toHaveCount(51);
  await expect(page.getByRole("button", { name: "加载更多" })).toBeVisible();

  // Filter B's first page is held back while it is in flight: the request
  // still reaches the real control plane and its answer is passed on
  // unchanged; only when it arrives moves. The window the review found is
  // open for as long as a slow answer takes.
  let release: (() => void) | null = null;
  const held = new Promise<void>((resolve) => {
    release = resolve;
  });
  const isFirstPageOfB = (url: URL) =>
    url.pathname === "/admin/pricing/charges" && url.searchParams.get("tenant_id") === b.tenantId && !url.searchParams.has("before");
  await page.route(
    (url) => isFirstPageOfB(url),
    async (route) => {
      await held;
      await route.continue();
    },
  );
  await page.getByLabel("租户").selectOption({ label: b.tenantName });
  // While B's first page is unresolved there is nothing to continue, so no
  // continuation is offered — and the one on screen is A's, not B's.
  await expect(table).toHaveCount(0);
  await expect(page.getByRole("button", { name: "加载更多" })).toHaveCount(0);
  release!();
  await expect(table.getByRole("row")).toHaveCount(13);
  await expect(page.getByRole("button", { name: "加载更多" })).toHaveCount(0);
  await page.unroute((url) => isFirstPageOfB(url));

  // The traversal is the filter's own set: every one of B's charges, once.
  const events = await table
    .getByRole("row")
    .evaluateAll((rows) => rows.slice(1).map((r) => r.textContent ?? ""));
  const tenantCell = b.tenantName;
  expect(events.filter((t) => t.includes(tenantCell))).toHaveLength(bRows.length);
  expect(events.some((t) => t.includes(a.tenantName))).toBe(false);
});

test("无需计费的行写明原因；模型删除后仍显示名称；可以隐藏无需计费的行", async ({ page }) => {
  const world = await seedTenant(stack);
  const suffix = randomUUID().slice(0, 8);
  // An account whose upstream answers 404 to every chat: each call fails upstream and bills nothing.
  const account = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: `acct-broken-${suffix}`,
      provider: "openai",
      api_key: `sk-acct-broken-${suffix}`,
      api_base: `${stack.mockUpstreamUrl}/broken/v1`,
      groups: ["shared"],
    },
  });
  const name = `broken-${suffix}`;
  const made = await stack.admin.post("/admin/resources/models", {
    doc: { display_name: name, provider: "openai", model_name: `upstream-broken-${suffix}`, provider_key_id: account.id },
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
  expect(await chat(stack, world.key.plaintext, name)).not.toBe(200);
  expect(await chat(stack, world.key.plaintext, world.model.name)).toBe(200);
  await chargesFor(world.tenantId, 2);
  // The model is retired afterwards; its call is still on record.
  const doc = await stack.admin.request("GET", `/admin/resources/models/${made.id}`);
  await stack.admin.del(`/admin/resources/models/${made.id}`, doc.version ?? doc.body.version);

  await openCharges(page);
  await page.getByLabel("租户").selectOption({ label: world.tenantName });
  const table = page.getByRole("table", { name: "计费明细" });
  await expect(table.getByRole("row")).toHaveCount(3);
  const failed = table.getByRole("row").filter({ hasText: "无需计费" });
  await expect(failed).toHaveCount(1);
  await expect(failed.getByRole("cell").nth(2)).toHaveText(`${name}（已删除）`);
  await expect(failed.getByRole("cell").nth(4)).toHaveText("调用失败 · 上游 404");

  // Hidden on request; the charged call stays.
  await page.getByLabel("隐藏无需计费").check();
  await expect(table.getByRole("row")).toHaveCount(2);
  await expect(table.getByRole("row").nth(1)).toContainText(world.model.name);
  await expect(table.getByRole("row").filter({ hasText: "无需计费" })).toHaveCount(0);
});

test("计费明细写明实际调用的上游与用量，展开后能按公式复核售价", async ({ page }) => {
  const world = await seedTenant(stack);
  expect(await chat(stack, world.key.plaintext, world.model.name)).toBe(200);
  await chargesFor(world.tenantId, 1);

  await openCharges(page);
  await page.getByLabel("租户").selectOption({ label: world.tenantName });
  const table = page.getByRole("table", { name: "计费明细" });
  await expect(table.getByRole("columnheader")).toContainText(["售价（元）", "成本（元）", "档位", "上游", "用量"]);
  const row = table.getByRole("row").filter({ hasText: world.model.name });
  await expect(row).toHaveCount(1);
  // The world's model is account acct-ui-7c1's upstream-mdl-ui-9f3, and the
  // stand-in reports 5 prompt and 3 completion tokens for every chat.
  await expect(row.getByRole("cell").nth(8)).toContainText("acct-ui-7c1");
  await expect(row.getByRole("cell").nth(8)).toContainText("upstream-mdl-ui-9f3");
  await expect(row.getByRole("cell").nth(9)).toHaveText("输入 5 · 输出 3");

  // Opened, the row shows what it billed: 5 input tokens at USD 1000/M and
  // 3 output tokens at USD 2000/M, which at 1 CNY per USD is ¥0.011.
  const toggle = row.getByRole("button", { name: /^展开用量明细/ });
  await toggle.click();
  await expect(row.getByRole("button", { name: /^收起用量明细/ })).toHaveAttribute("aria-expanded", "true");
  const billed = page.getByRole("region", { name: /^计费数量/ });
  const lines = billed.getByRole("row");
  await expect(lines).toHaveCount(3);
  await expect(lines.nth(1).getByRole("cell")).toHaveText(["输入", "5", "$1000 / 百万 tokens", "0.005"]);
  await expect(lines.nth(2).getByRole("cell")).toHaveText(["输出", "3", "$2000 / 百万 tokens", "0.006"]);
  await expect(billed).toContainText("分组换算率 1");
  await expect(billed).toContainText("¥0.011");
  await expect(billed).toContainText("与实收一致");
  const reported = page.getByRole("region", { name: /^上游报告的用量/ });
  await expect(reported).toContainText("输入5");
  await expect(reported).toContainText("输出3");

  // Closed again, the row is one row.
  await row.getByRole("button", { name: /^收起用量明细/ }).click();
  await expect(billed).toHaveCount(0);
  await expect(table.getByRole("row")).toHaveCount(2);
});

