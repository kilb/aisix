import { randomUUID } from "node:crypto";
import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";
import { approveTopup, chat, internalKey, seedTenant, tenantClient } from "./world";

/**
 * 「计费 · 对账」 against the real control plane and gateway. The gateway's
 * link to the control plane runs through a relay the test can pause, which
 * is how the usage-loss test makes the gateway's usage queue overflow for
 * real. Expected values come from the scenario.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ telemetryRelay: true });
});

test.afterAll(async () => {
  await stack?.stop();
});

async function open(page: Page): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(stack.operator.email);
  await page.getByLabel("口令", { exact: true }).fill(stack.operator.password);
  await page.getByRole("button", { name: "登录" }).click();
  await page.locator('[data-tab="billing"]').click();
  await page.getByRole("button", { name: "对账", exact: true }).click();
  await expect(page.getByRole("heading", { name: "对账", exact: true })).toBeVisible();
  await expect(page.locator("main").getByText("载入中…")).toHaveCount(0);
}

const region = (page: Page, name: string) => page.getByRole("region", { name, exact: true });

/** The gateway's own count of usage events it dropped for `reason`. */
async function dropped(reason: string): Promise<number> {
  const res = await fetch(`${stack.gatewayMetricsUrl}/metrics`);
  const text = await res.text();
  let total = 0;
  for (const line of text.split("\n")) {
    if (line.startsWith("aisix_usage_event_drops_total") && line.includes(`reason="${reason}"`)) {
      total += Number(line.slice(line.lastIndexOf(" ") + 1));
    }
  }
  return total;
}

test("对账：账实相符时显示无差异", async ({ page }) => {
  const world = await seedTenant(stack);
  for (let i = 0; i < 3; i++) expect(await chat(stack, world.key.plaintext, world.model.name)).toBe(200);
  const made = await (await tenantClient(stack, world)).post("/api/billing/topups", {
    amount_usd: "5.00",
    note: "recon-topup",
    reference: "ref-recon",
  });
  await approveTopup(stack, made.id ?? made.topup?.id);
  // The charges are posted a moment after the calls: the ledger holds the credit, the top-up and three charges.
  await expect
    .poll(async () => (await stack.admin.get(`/admin/tenants/${world.tenantId}/billing/ledger`)).items.length, { timeout: 60_000 })
    .toBeGreaterThanOrEqual(5);

  await open(page);
  await expect(region(page, "账实核对").getByText("账实相符")).toBeVisible();
  await expect(region(page, "账实核对").getByRole("table")).toHaveCount(0);
  await expect(region(page, "已计费但未入账")).toContainText("0");
});

test("网关报告的用量丢弃显示为疑似丢失", async ({ page }) => {
  test.setTimeout(300_000);
  const world = await seedTenant(stack);
  const key = await internalKey(stack);
  const first = randomUUID();
  expect(await chat(stack, key, world.model.name, { requestId: first })).toBe(200);
  await expect
    .poll(async () => (await stack.admin.get(`/admin/usage/events?request_id=${first}`)).items.length, { timeout: 60_000 })
    .toBe(1);

  await stack.relay!.pause();
  let sent = 0;
  while ((await dropped("sink_full")) === 0 && sent < 40_000) {
    let next = 0;
    const worker = async () => {
      while (next < 2_000) {
        next++;
        await chat(stack, key, world.model.name);
      }
    };
    await Promise.all(Array.from({ length: 64 }, worker));
    sent += 2_000;
  }
  expect(await dropped("sink_full")).toBeGreaterThan(0);
  await stack.relay!.resume();

  await open(page);
  const loss = region(page, "疑似用量丢失");
  await expect(async () => {
    await page.getByRole("button", { name: "刷新" }).click();
    const text = await loss.getByRole("row", { name: /已丢弃/ }).first().innerText();
    expect(Number(text.replace(/\D/g, ""))).toBeGreaterThan(0);
    await expect(loss.getByRole("table", { name: "各实例的丢弃明细" })).toContainText("stack-gw");
    await expect(loss.getByRole("table", { name: "各实例的丢弃明细" })).toContainText("队列已满");
  }).toPass({ timeout: 30_000 });
});
