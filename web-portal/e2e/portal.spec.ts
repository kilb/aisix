import { test, expect, type Page } from "@playwright/test";
import { start, PASSWORD, type Fixture } from "./fixture";

let fx: Fixture;

test.beforeAll(async () => {
  fx = await start();
});
test.afterAll(() => fx?.stop());

/** 余额面板里那个大号读数。金额也会出现在流水行里，所以断言必须指明是哪一处。 */
function balanceReading(page: Page) {
  return page.locator(".panel", { hasText: "当前余额" }).locator(".val").first();
}

/** 每个用例用独立邮箱：夹具的库在整个文件里是共享的。 */
function mail(tag: string): string {
  return `${tag}-${Date.now()}-${Math.floor(Math.random() * 1e6)}@e2e.test`;
}

async function signUp(page: Page, email: string): Promise<void> {
  await page.goto(fx.previewUrl);
  await page.getByRole("button", { name: "新用户" }).click();
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令").fill(PASSWORD);
  await page.getByRole("button", { name: "注册并登录" }).click();
  await expect(page.getByRole("heading", { name: "余额" })).toBeVisible();
}

async function grant(email: string, micro: number, note: string): Promise<string> {
  const list = await fetch(`${fx.portalUrl}/admin/users`, {
    headers: { authorization: `Bearer ${fx.adminToken}` },
  }).then((r) => r.json());
  const u = list.users.find((x: { email: string }) => x.email === email);
  expect(u, `管理端列表里找不到 ${email}`).toBeTruthy();
  const r = await fetch(`${fx.portalUrl}/admin/users/${u.user_id}/grant`, {
    method: "POST",
    headers: { authorization: `Bearer ${fx.adminToken}`, "content-type": "application/json" },
    body: JSON.stringify({ micro_usd: micro, note }),
  });
  expect(r.status).toBe(200);
  return u.user_id;
}

test("未登录时门户不渲染任何账目", async ({ page }) => {
  await page.goto(fx.previewUrl);
  await expect(page.getByRole("button", { name: "已有账号" })).toBeVisible();
  const body = await page.textContent("body");
  expect(body).not.toContain("流水");
  expect(body).not.toContain("当前余额");
});

test("口令过短在注册处就被挡下", async ({ page }) => {
  await page.goto(fx.previewUrl);
  await page.getByRole("button", { name: "新用户" }).click();
  await page.getByLabel("邮箱").fill(mail("short"));
  await page.getByLabel("口令").fill("tooshort");
  await page.getByRole("button", { name: "注册并登录" }).click();
  await expect(page.getByText(/至少 12 个字符/)).toBeVisible();
});

test("注册后余额为零_管理员发放后可见_流水带备注", async ({ page }) => {
  const email = mail("grant");
  await signUp(page, email);

  // 刚注册：余额为零，且明确说额度由管理员发放。
  await expect(balanceReading(page)).toHaveText("$0.00");

  await grant(email, 5_000_000, "首充赠送");
  await page.reload();

  await expect(balanceReading(page)).toHaveText("$5.00");
  // 「管理员发放」在余额面板的脚注和流水行里各出现一次，指到流水那一行。
  const row = page.locator("tbody tr", { hasText: "首充赠送" });
  await expect(row).toHaveCount(1);
  await expect(row).toContainText("管理员发放");
  await expect(row).toContainText("$5.00");
});

test("没有密钥绑到本人时门户明确说未绑定", async ({ page }) => {
  const email = mail("unbound");
  await signUp(page, email);
  // 配置里有密钥，但都不属于这个人。
  fx.writeResources(
    "api_keys:\n- display_name: other\n  key_hash: aa\n  user_id: someone-else\n",
  );
  await page.reload();

  // 这是管理员把 user_id 填错时唯一能被人看见的地方。没有它，
  // 「用量一直是 0」跟「还没开始用」在屏幕上没有区别 —— 而它实际意味着
  // 这个人在免费用。
  await expect(page.getByText(/未绑定任何密钥/)).toBeVisible();
  await expect(page.getByText("已绑定密钥")).toBeVisible();
});

test("密钥绑上之后不再报未绑定_并数出停用数", async ({ page }) => {
  const email = mail("bound");
  await signUp(page, email);
  const uid = await grant(email, 1_000_000, "额度");
  fx.writeResources(
    `api_keys:\n` +
      `- display_name: mine\n  key_hash: aa\n  user_id: ${uid}\n` +
      `- display_name: mine-off\n  key_hash: bb\n  user_id: ${uid}\n  disabled: true\n` +
      `- display_name: other\n  key_hash: cc\n  user_id: someone-else\n`,
  );
  await page.reload();

  await expect(page.getByText(/未绑定任何密钥/)).toHaveCount(0);
  await expect(page.getByText(/其中 1 把已停用/)).toBeVisible();
});

test("两个用户各自只看到自己的余额与流水", async ({ page, browser }) => {
  const a = mail("iso-a");
  const b = mail("iso-b");
  await signUp(page, a);
  await grant(a, 3_000_000, "给A的备注");

  const ctx = await browser.newContext();
  const p2 = await ctx.newPage();
  await signUp(p2, b);
  await grant(b, 9_000_000, "给B的备注");

  await page.reload();
  await p2.reload();

  // A 看到 A 的，且**任何形式的 B 的痕迹都不该出现**。
  await expect(balanceReading(page)).toHaveText("$3.00");
  expect(await page.textContent("body")).not.toContain("给B的备注");
  expect(await page.textContent("body")).not.toContain("$9.00");

  await expect(balanceReading(p2)).toHaveText("$9.00");
  expect(await p2.textContent("body")).not.toContain("给A的备注");
  await ctx.close();
});

test("登出后回到闸口_且账目不再渲染", async ({ page }) => {
  const email = mail("out");
  await signUp(page, email);
  await grant(email, 1_000_000, "额度");
  await page.reload();
  await expect(balanceReading(page)).toHaveText("$1.00");

  await page.getByRole("button", { name: "登出" }).click();
  await expect(page.getByRole("button", { name: "已有账号" })).toBeVisible();
  expect(await page.textContent("body")).not.toContain("当前余额");
});

test("用户会话拿不到管理端", async ({ page }) => {
  const email = mail("noadmin");
  await signUp(page, email);
  // 把浏览器拿到的会话 cookie 取出来，从 Node 侧直接敲管理端。
  //
  // 不在页面里 fetch：门户前端只代理 /api，`/admin` 压根不该从这个源可达
  // —— 那会被 CORS 挡住，测出来的是浏览器的同源策略，不是服务端的授权。
  const cookies = await page.context().cookies();
  const jar = cookies.map((c) => `${c.name}=${c.value}`).join("; ");
  expect(jar).toContain("aisix_portal=");

  const r = await fetch(`${fx.portalUrl}/admin/users`, { headers: { cookie: jar } });
  expect(r.status).toBe(401);
});
