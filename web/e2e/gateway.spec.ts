import { test, expect, type Page } from "@playwright/test";
import {
  start,
  PASSWORD,
  CALLER_KEY,
  CONSOLE_ONLY_VAR,
  GATEWAY_ONLY_VAR,
  type Fixture,
} from "./fixture";

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

test("引用只有网关才有的环境变量_控制台照样能保存_网关确实用上了", async ({ page }) => {
  // 现行配置里的上游密钥写成 ${变量}，这个变量只在网关的环境里。
  expect(fx.read()).toContain(GATEWAY_ONLY_VAR);

  await signIn(page);
  await page.getByRole("tab", { name: "供应商" }).click();
  await page.getByLabel("名称").fill("applied-provider");
  await page.getByLabel("API 密钥").fill("sk-applied");
  await page.getByRole("button", { name: /保存并重载网关/ }).click();

  await expect(page.getByText("已保存，网关已重载。")).toBeVisible();
  expect(fx.read()).toContain("applied-provider");
  expect(fx.read()).toContain(GATEWAY_ONLY_VAR);
});

test("新写进去的变量两边都没有_在校验这一步就被挡下", async ({ page }) => {
  await signIn(page);
  await page.getByRole("tab", { name: "配置原文" }).click();
  const box = page.getByRole("textbox").first();
  await expect(box).not.toHaveValue("");
  const before = fx.read();
  await box.fill((await box.inputValue()).replace("sk-applied", "${E2E_NOBODY_HAS_THIS}"));
  await page.getByRole("button", { name: "校验并保存" }).click();

  await expect(page.locator(".note.crit", { hasText: "E2E_NOBODY_HAS_THIS" })).toBeVisible();
  expect(fx.read()).toBe(before);
});

test("网关拒收时如实说_不说已重载", async ({ page }) => {
  await signIn(page);
  await page.getByRole("tab", { name: "配置原文" }).click();
  const box = page.getByRole("textbox").first();
  await expect(box).not.toHaveValue("");
  const good = await box.inputValue();
  // 这个变量控制台有，校验过得去；网关没有，它会拒收。
  await box.fill(good.replace("sk-applied", `\${${CONSOLE_ONLY_VAR}}`));
  await page.getByRole("button", { name: "校验并保存" }).click();

  const note = page.locator(".note.crit", { hasText: "网关拒绝了这份配置" });
  await expect(note).toBeVisible();
  await expect(note).toContainText(CONSOLE_ONLY_VAR);
  await expect(page.getByText("已保存，网关已重载。")).toHaveCount(0);

  // 改回来，网关恢复。
  await box.fill(good);
  await page.getByRole("button", { name: "校验并保存" }).click();
  await expect(page.getByText("已保存，网关已重载。")).toBeVisible();
});
