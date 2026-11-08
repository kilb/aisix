import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "./stack";

/**
 * 价格 · 价格目录: the built-in public list prices with their sources.
 * Against the real control plane; the expected values are read through the
 * operator API, not copied from the page.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

async function openCatalogue(page: Page): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(stack.operator.email);
  await page.getByLabel("口令").fill(stack.operator.password);
  await page.getByRole("button", { name: "登录" }).click();
  await page.getByRole("tab", { name: "价格", exact: true }).click();
}

test("价格目录可浏览，带来源链接与日期", async ({ page }) => {
  const { items } = (await stack.admin.get("/admin/pricing/catalogue")) as { items: Json[] };
  expect(items.length).toBeGreaterThan(1);
  const first = items[0];
  const last = items[items.length - 1];

  await openCatalogue(page);
  const table = page.getByRole("table", { name: "价格目录" });
  await expect(table.getByRole("columnheader")).toContainText(["条目", "别名", "来源", "日期", "说明"]);
  // One row per entry, plus the header row.
  await expect(table.getByRole("row")).toHaveCount(items.length + 1);

  for (const entry of [first, last]) {
    const row = table.getByRole("row", { name: new RegExp(entry.key.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")) }).first();
    await expect(row.getByRole("link")).toHaveAttribute("href", entry.source_url);
    // The date column (the fourth cell), not the notes, which quote dates too.
    await expect(row.getByRole("cell").nth(3)).toContainText(entry.effective_from ?? entry.as_of);
  }
});

test("展开一条，以只读 JSON 看到它的价目卡", async ({ page }) => {
  const { items } = (await stack.admin.get("/admin/pricing/catalogue")) as { items: Json[] };
  const entry = items[0];

  await openCatalogue(page);
  await page.getByRole("button", { name: `展开 ${entry.key}` }).click();
  const doc = page.getByRole("textbox", { name: "资源文档" });
  await expect(doc).toHaveAttribute("readonly", "");
  const shown = JSON.parse(await doc.inputValue());
  expect(shown.card).toEqual(entry.card);
  expect(shown.source_url).toBe(entry.source_url);

  await page.getByRole("button", { name: `收起 ${entry.key}` }).click();
  await expect(doc).toHaveCount(0);
});
