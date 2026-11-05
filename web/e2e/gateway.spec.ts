import { test, expect, type Page } from "@playwright/test";
import { start, PASSWORD, CALLER_KEY, type Fixture } from "./fixture";

/**
 * 控制台对着**真网关**：它读同一份 resources.yaml，真的收 SIGHUP、真的写日志。
 *
 * 默认那套用例让管理 API 不可达（测的是读不到时的表现），所以「保存后网关到底
 * 有没有用上」「逐请求日志长什么样」只有这里量得到。
 */

let fx: Fixture;

test.beforeAll(async () => {
  fx = await start({ withGateway: true });
});
test.afterAll(() => fx?.stop());

async function signIn(page: Page): Promise<void> {
  await page.goto(fx.previewUrl);
  await page.getByLabel("口令").fill(PASSWORD);
  await page.getByRole("button", { name: "进入" }).click();
  await expect(page.getByRole("tab", { name: "概览" })).toBeVisible();
}

test("调用日志显示状态_耗时与模型", async ({ page }) => {
  const model = `no-such-model-${Date.now()}`;
  const r = await fetch(`${fx.proxyUrl}/v1/chat/completions`, {
    method: "POST",
    headers: { authorization: `Bearer ${CALLER_KEY}`, "content-type": "application/json" },
    body: JSON.stringify({ model, messages: [{ role: "user", content: "hi" }] }),
  });
  expect(r.status).toBe(404);

  await signIn(page);
  await page.getByRole("tab", { name: "调用日志" }).click();
  // 这一行在响应写完之后才记下，所以读到为止。
  await expect(async () => {
    await page.getByRole("button", { name: "读取" }).click();
    const row = page.getByRole("row", { name: new RegExp(model) });
    await expect(row).toContainText("404", { timeout: 1_000 });
    await expect(row).toContainText(/\d+ms/, { timeout: 1_000 });
  }).toPass({ timeout: 20_000 });
});
