import { test, expect, type Page } from "@playwright/test";
import { start, PASSWORD, PORTAL_TOKEN_SENTINEL, type Fixture } from "./fixture";

/**
 * 控制台的「门户用户」页，对着**真门户**。
 *
 * 默认那套用例把门户指向一个不存在的地址（测的是不可达时的表现），所以这个
 * 面板此前没有任何功能覆盖 —— 而它能把一个客户的额度直接清成 0。
 */

let fx: Fixture;

test.beforeAll(async () => {
  fx = await start({ withPortal: true });
});
test.afterAll(() => fx?.stop());

async function signIn(page: Page): Promise<void> {
  await page.goto(fx.previewUrl);
  await page.getByLabel("口令").fill(PASSWORD);
  await page.getByRole("button", { name: "进入" }).click();
  await expect(page.getByRole("tab", { name: "概览" })).toBeVisible();
}

/** 直接向门户注册一个用户 —— 控制台没有注册入口，用户是自助来的。 */
async function register(email: string): Promise<string> {
  const r = await fetch(`${fx.portalUrl}/api/register`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email, password: "correct horse battery staple" }),
  });
  // body 只能读一次：先取文本再自己解析，否则断言消费掉之后 json() 会炸。
  const body = await r.text();
  expect(r.status, body).toBe(201);
  return JSON.parse(body).user_id as string;
}

/** 从门户管理端读这个人的总额度。 */
async function granted(userId: string): Promise<number> {
  const list = await fetch(`${fx.portalUrl}/admin/users`, {
    headers: { authorization: `Bearer ${PORTAL_TOKEN_SENTINEL}` },
  }).then((r) => r.json());
  const u = list.users.find((x: { user_id: string }) => x.user_id === userId);
  expect(u, "门户列表里找不到这个人").toBeTruthy();
  return u.granted_micro_usd as number;
}

function mail(tag: string): string {
  return `${tag}-${Date.now()}-${Math.floor(Math.random() * 1e6)}@e2e.test`;
}

async function openPanel(page: Page): Promise<void> {
  await signIn(page);
  await page.getByRole("tab", { name: "门户用户" }).click();
  await expect(page.getByRole("heading", { name: "用户额度" })).toBeVisible();
}

test("金额留空时点设额度_不会把这个人的额度清成零", async ({ page }) => {
  const email = mail("blank");
  const uid = await register(email);
  // 先给他一笔额度，这样「被清零」才看得出来。
  await fetch(`${fx.portalUrl}/admin/users/${uid}/quota`, {
    method: "POST",
    headers: {
      authorization: `Bearer ${PORTAL_TOKEN_SENTINEL}`,
      "content-type": "application/json",
    },
    body: JSON.stringify({ micro_usd: 7_000_000 }),
  });

  await openPanel(page);
  await page.getByLabel("用户").selectOption(uid);
  // 金额栏留空 —— `Number("")` 是 0，而「设为 0」等于把这个人整个切断。
  await page.getByRole("button", { name: "设为该额度" }).click();
  await expect(page.getByText("请先填金额")).toBeVisible();
  expect(await granted(uid), "留空点按钮把额度清零了").toBe(7_000_000);
});

test("设为该额度是绝对值_追加是另一个按钮", async ({ page }) => {
  const email = mail("absolute");
  const uid = await register(email);
  await openPanel(page);
  await page.getByLabel("用户").selectOption(uid);

  await page.getByLabel("金额 USD").fill("20");
  await page.getByRole("button", { name: "设为该额度" }).click();
  await expect(page.getByText(/总额度已设为/)).toBeVisible();
  expect(await granted(uid)).toBe(20_000_000);

  // 再设一次更小的数：是设定就该落在 6，是追加就会变成 26。
  await page.getByLabel("金额 USD").fill("6");
  await page.getByRole("button", { name: "设为该额度" }).click();
  await expect(page.getByText(/总额度已设为/)).toBeVisible();
  expect(await granted(uid), "「设定」被当成了「追加」").toBe(6_000_000);

  // 追加按钮才是增量。
  await page.getByLabel("金额 USD").fill("4");
  await page.getByRole("button", { name: "改为追加" }).click();
  await expect(page.getByText(/已给.*发放/)).toBeVisible();
  expect(await granted(uid)).toBe(10_000_000);
});

test("已分配超过总额度时_面板明确说出来", async ({ page }) => {
  const email = mail("over");
  const uid = await register(email);
  const adminHeaders = {
    authorization: `Bearer ${PORTAL_TOKEN_SENTINEL}`,
    "content-type": "application/json",
  };
  await fetch(`${fx.portalUrl}/admin/users/${uid}/quota`, {
    method: "POST",
    headers: adminHeaders,
    body: JSON.stringify({ micro_usd: 10_000_000 }),
  });

  // 用户自己把 8 块分给一把密钥。
  const jar = await fetch(`${fx.portalUrl}/api/login`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email, password: "correct horse battery staple" }),
  });
  const cookie = (jar.headers.getSetCookie?.() ?? [])
    .map((c) => c.split(";")[0])
    .join("; ");
  const minted = await fetch(`${fx.portalUrl}/api/keys`, {
    method: "POST",
    headers: { cookie, "content-type": "application/json" },
    body: JSON.stringify({ label: "大头" }),
  }).then((r) => r.json());
  const q = await fetch(
    `${fx.portalUrl}/api/keys/${encodeURIComponent(minted.name)}/quota`,
    {
      method: "PUT",
      headers: { cookie, "content-type": "application/json" },
      body: JSON.stringify({ micro_usd: 8_000_000 }),
    },
  );
  expect(q.status, await q.text()).toBe(200);

  // 管理员把总额度调到 3 块 —— 已分配的 8 块反过来超过了总额。
  await openPanel(page);
  await page.getByLabel("用户").selectOption(uid);
  await page.getByLabel("金额 USD").fill("3");
  await page.getByRole("button", { name: "设为该额度" }).click();

  // 不拦这次设定（花费仍以总额度为准），但必须说出来 —— 不说的话管理员看不到
  // 分配已经对不上，用户那边只会看到一个负的「可再分配」而不知道原因。
  await expect(page.getByText(/超过了总额度/)).toBeVisible();
  expect(await granted(uid)).toBe(3_000_000);
});
