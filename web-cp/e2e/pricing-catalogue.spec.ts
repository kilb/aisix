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
  // With every section registered, 价格 opens on its first; this page is one switch away.
  await page.getByRole("button", { name: "价格目录" }).click();
}

test("价格目录可浏览，带来源链接与日期", async ({ page }) => {
  const { items } = (await stack.admin.get("/admin/pricing/catalogue")) as { items: Json[] };
  expect(items.length).toBeGreaterThan(1);
  await openCatalogue(page);
  const table = page.getByRole("table", { name: "价格目录" });
  await expect(table.getByRole("columnheader")).toContainText(["条目", "别名", "来源", "日期", "说明"]);
  const pager = page.getByRole("navigation", { name: "价格目录分页" });
  await pager.getByLabel("价格目录分页每页条数").selectOption("10");
  // 逐页核对完整目录，不能把第一页当成全部，也不能漏掉同一标识的后续版本。
  for (let offset = 0; offset < items.length; offset += 10) {
    const segment = items.slice(offset, offset + 10);
    const rows = table.locator("tbody > tr");
    await expect(rows).toHaveCount(segment.length);
    for (let index = 0; index < segment.length; index += 1) {
      const entry = segment[index];
      const row = rows.nth(index);
      await expect(row.getByRole("cell").nth(0)).toHaveText(entry.key);
      if (entry.source_url) await expect(row.getByRole("link")).toHaveAttribute("href", entry.source_url);
      else await expect(row.getByRole("link")).toHaveCount(0);
      const date = entry.effective_from
        ? `${new Date(Date.parse(entry.effective_from) + 8 * 3_600_000).toISOString().slice(0, 19).replace("T", " ")}`
        : entry.as_of;
      if (date) await expect(row.getByRole("cell").nth(3)).toContainText(date);
    }
    if (offset + 10 < items.length) await pager.getByRole("button", { name: "下一页" }).click();
  }
  await expect(pager.getByRole("button", { name: "下一页" })).toBeDisabled();
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
