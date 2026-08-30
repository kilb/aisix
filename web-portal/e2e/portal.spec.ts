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
  // 这条横幅要给出**当前**可执行的下一步。它曾经写的是「请让管理员创建密钥」，
  // 而用户能自助建密钥之后，照着做只会白等。
  //
  // 指名那条横幅：密钥面板里也有一句「还没有密钥」，两处都匹配会撞上严格模式。
  const banner = page.locator(".note.warn", { hasText: "还没有密钥" });
  await expect(banner).toBeVisible();
  await expect(banner).toContainText("创建一把");
  await expect(page.getByText(/让管理员创建密钥/)).toHaveCount(0);
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

test("自助创建密钥_明文只显示一次_列表里只有遮蔽的散列", async ({ page }) => {
  const email = mail("mint");
  await signUp(page, email);

  await page.getByRole("button", { name: "创建密钥" }).click();
  const code = page.locator(".minted code");
  await expect(code).toBeVisible();
  const plaintext = (await code.textContent())!.trim();
  expect(plaintext).toMatch(/^sk-aisix-[0-9a-f]{64}$/);

  // 明文只此一次。收起之后界面上任何地方都不该再有它。
  await page.getByRole("button", { name: "我已保存" }).click();
  await expect(code).toHaveCount(0);
  await page.reload();
  expect(await page.textContent("body")).not.toContain(plaintext);

  // 列表里是遮蔽的散列。
  const row = page.locator("tbody tr", { hasText: "portal-" }).first();
  await expect(row).toContainText("…");
});

test("零余额时新密钥是停用态_发放额度后转为可用", async ({ page }) => {
  const email = mail("bornoff");
  await signUp(page, email);

  await page.getByRole("button", { name: "创建密钥" }).click();
  // 若建成可用的，它会在网关眼里活到对账环下一轮才被关掉 —— 那一段是白送的
  // 推理，每建一把密钥送一次。
  await expect(page.getByText(/当前余额为零/)).toBeVisible();
  await page.getByRole("button", { name: "我已保存" }).click();
  await expect(page.locator("tbody tr", { hasText: "portal-" }).first()).toContainText("已停用");

  await grant(email, 5_000_000, "开通");
  // 对账环会把它启用回来；这里等它跑一轮。
  await expect(async () => {
    await page.reload();
    await expect(
      page.locator("tbody tr", { hasText: "portal-" }).first(),
    ).toContainText("可用");
  }).toPass({ timeout: 60_000 });
});

test("可以创建多把_且吊销只影响自己那一把", async ({ page }) => {
  const email = mail("many");
  await signUp(page, email);

  for (const label of ["一", "二", "三"]) {
    await page.getByLabel("名称（可选）").fill(label);
    await page.getByRole("button", { name: "创建密钥" }).click();
    await page.getByRole("button", { name: "我已保存" }).click();
  }
  await expect(page.locator("tbody tr", { hasText: "portal-" })).toHaveCount(3);

  await page.locator("tbody tr", { hasText: "portal-" }).first().getByRole("button", { name: "吊销" }).click();
  await expect(page.locator("tbody tr", { hasText: "portal-" })).toHaveCount(2);
});

test("看不到也删不掉别人的密钥", async ({ page, browser }) => {
  const a = mail("keys-a");
  const b = mail("keys-b");
  await signUp(page, a);
  await page.getByLabel("名称（可选）").fill("A的密钥");
  await page.getByRole("button", { name: "创建密钥" }).click();
  await page.getByRole("button", { name: "我已保存" }).click();
  const aName = (await page
    .locator("tbody tr", { hasText: "portal-" })
    .first()
    .locator("td")
    .first()
    .textContent())!.split(" · ")[0]!;

  const ctx = await browser.newContext();
  const p2 = await ctx.newPage();
  await signUp(p2, b);
  // B 的列表里不该有 A 的任何东西。
  await expect(p2.locator("tbody tr", { hasText: "portal-" })).toHaveCount(0);
  expect(await p2.textContent("body")).not.toContain("A的密钥");

  // B 直接调接口删 A 的密钥 —— 少了主人校验，任何登录用户都能凭名字删掉别人的。
  const status = await p2.evaluate(async (n) => {
    const r = await fetch(`/api/keys/${encodeURIComponent(n)}`, {
      method: "DELETE",
      credentials: "same-origin",
    });
    return r.status;
  }, aName);
  expect(status).toBe(404);

  await page.reload();
  await expect(page.locator("tbody tr", { hasText: "portal-" })).toHaveCount(1);
  await ctx.close();
});

test("调用记录只按会话过滤_没有密钥时明说而不是空表", async ({ page }) => {
  const email = mail("logs");
  await signUp(page, email);

  // 刚注册、没有密钥：应当明说没有绑定的密钥，而不是显示一张空表 ——
  // 空表会被读成「有密钥但没流量」，那是两件不同的事。
  await expect(page.getByRole("heading", { name: "调用记录" })).toBeVisible();
  await expect(page.getByText(/还没有绑定的密钥/)).toBeVisible();

  // 端点不接受任何能改变过滤对象的参数。
  const probe = await page.evaluate(async () => {
    const r = await fetch("/api/logs?limit=50&user_id=someone-else&api_key_id=whatever", {
      credentials: "same-origin",
    });
    return { status: r.status, body: await r.text() };
  });
  expect(probe.status).toBe(200);
  expect(probe.body).not.toContain("someone-else");
  expect(probe.body).not.toContain("whatever");
});

test("提交充值单不改余额_确认后才入账_重复确认被拒", async ({ page }) => {
  const email = mail("topup");
  await signUp(page, email);

  await page.getByLabel("金额 USD").fill("20");
  await page.getByLabel("备注（转账单号等）").fill("单号 X1");
  await page.getByRole("button", { name: "提交充值单" }).click();
  await expect(page.getByText(/等待管理员确认/)).toBeVisible();
  await expect(page.locator("tbody tr", { hasText: "单号 X1" })).toContainText("待确认");

  // 提交 ≠ 到账。这一条是「线下」两个字的全部含义。
  await expect(balanceReading(page)).toHaveText("$0.00");

  // 管理员确认。
  const list = await fetch(`${fx.portalUrl}/admin/topups`, {
    headers: { authorization: `Bearer ${fx.adminToken}` },
  }).then((r) => r.json());
  const t = list.topups.find((x: { email: string }) => x.email === email);
  expect(t, "管理端看不到这笔充值单").toBeTruthy();

  const approve = () =>
    fetch(`${fx.portalUrl}/admin/topups/${t.id}/approve`, {
      method: "POST",
      headers: { authorization: `Bearer ${fx.adminToken}`, "content-type": "application/json" },
      body: JSON.stringify({ note: "已核对" }),
    });
  expect((await approve()).status).toBe(200);
  // 第二次必须是冲突 —— 假装成功会让管理员以为自己刚又入了一笔。
  expect((await approve()).status).toBe(409);

  await page.reload();
  await expect(balanceReading(page)).toHaveText("$20.00");
  await expect(page.locator("tbody tr", { hasText: "单号 X1" })).toContainText("已入账");

  // 流水里那一笔是**进账**。「除了发放就是消费」的写法会把它标成「消费」——
  // 一笔加钱的记录写着花钱，用户会以为自己被扣了。
  const row = page
    .locator(".panel", { hasText: "流水" })
    .locator("tbody tr")
    .filter({ hasText: "$20.00" })
    .first();
  await expect(row).toContainText("充值");
  await expect(row).not.toContainText("消费");
});

test("管理员设定的额度在流水里显示为进账_不是消费", async ({ page }) => {
  const email = mail("setquota-label");
  await signUp(page, email);
  await setQuota(email, 12_000_000);
  await page.reload();

  // `admin_set` 是管理员日常调额走的那条路，落在流水里必须是能看懂的进账。
  const row = page
    .locator(".panel", { hasText: "流水" })
    .locator("tbody tr")
    .filter({ hasText: "$12.00" })
    .first();
  await expect(row).toContainText("额度");
  await expect(row).not.toContainText("消费");
});

/** 把某个用户的总额度**设定**成某个数（绝对值）。 */
async function setQuota(email: string, micro: number): Promise<void> {
  const list = await fetch(`${fx.portalUrl}/admin/users`, {
    headers: { authorization: `Bearer ${fx.adminToken}` },
  }).then((r) => r.json());
  const u = list.users.find((x: { email: string }) => x.email === email);
  expect(u, `管理端列表里找不到 ${email}`).toBeTruthy();
  const r = await fetch(`${fx.portalUrl}/admin/users/${u.user_id}/quota`, {
    method: "POST",
    headers: { authorization: `Bearer ${fx.adminToken}`, "content-type": "application/json" },
    body: JSON.stringify({ micro_usd: micro, note: "e2e 设定" }),
  });
  expect(r.status).toBe(200);
}

/** 密钥表里第 `i` 行（0 起）。 */
function keyRow(page: Page, i: number) {
  return page.locator("tbody tr", { hasText: "portal-" }).nth(i);
}

test("给每把密钥设额度_界面上分不出去的那部分被明确挡住", async ({ page }) => {
  const email = mail("key-quota");
  await signUp(page, email);
  await setQuota(email, 10_000_000);

  for (const label of ["甲", "乙"]) {
    await page.getByLabel("名称（可选）").fill(label);
    await page.getByRole("button", { name: "创建密钥" }).click();
    await page.getByRole("button", { name: "我已保存" }).click();
  }
  await page.reload();

  const budget = page.locator(".budget");
  await expect(budget).toContainText("$10.00");
  // 还没分配，可再分配就该等于总额度。
  await expect(budget).toContainText("已分配 $0.00");

  // 给甲分 $6。
  await keyRow(page, 0).getByRole("button", { name: "不设限" }).click();
  await keyRow(page, 0).getByLabel(/的额度 USD$/).fill("6");
  await keyRow(page, 0).getByRole("button", { name: "保存" }).click();
  await expect(budget).toContainText("已分配 $6.00");
  await expect(budget).toContainText("可再分配 $4.00");

  // 再给乙分 $5 —— 6 + 5 > 10，必须被挡住，并说清还能分多少。
  await keyRow(page, 1).getByRole("button", { name: "不设限" }).click();
  await keyRow(page, 1).getByLabel(/的额度 USD$/).fill("5");
  await keyRow(page, 1).getByRole("button", { name: "保存" }).click();
  await expect(page.locator(".note.crit")).toContainText("不能超过你的总额度");
  // 被挡住之后账面不能变 —— 「先写再报错」会让用户莫名多出一道闸。
  await expect(budget).toContainText("已分配 $6.00");

  // 改成 $4 刚好用完，放行。
  await keyRow(page, 1).getByLabel(/的额度 USD$/).fill("4");
  await keyRow(page, 1).getByRole("button", { name: "保存" }).click();
  await expect(budget).toContainText("已分配 $10.00");
  await expect(budget).toContainText("可再分配 $0.00");

  // 调低甲到 $1：校验若拿「当前总和 + 新值」比会算成 11 > 10 而误拒。
  await keyRow(page, 0).getByRole("button", { name: "$6.00" }).click();
  await keyRow(page, 0).getByLabel(/的额度 USD$/).fill("1");
  await keyRow(page, 0).getByRole("button", { name: "保存" }).click();
  await expect(budget).toContainText("已分配 $5.00");
  await expect(page.locator(".note.crit")).toHaveCount(0);

  // 清成 0 就是「不设限」，那份额度回到可分配的池子。
  await keyRow(page, 0).getByRole("button", { name: "$1.00" }).click();
  await keyRow(page, 0).getByLabel(/的额度 USD$/).fill("0");
  await keyRow(page, 0).getByRole("button", { name: "保存" }).click();
  await expect(keyRow(page, 0).getByRole("button", { name: "不设限" })).toBeVisible();
  await expect(budget).toContainText("已分配 $4.00");
});

test("吊销一把密钥会把它占的额度还回来", async ({ page }) => {
  const email = mail("key-quota-revoke");
  await signUp(page, email);
  await setQuota(email, 8_000_000);
  await page.getByLabel("名称（可选）").fill("要吊销的");
  await page.getByRole("button", { name: "创建密钥" }).click();
  await page.getByRole("button", { name: "我已保存" }).click();
  await page.reload();

  await keyRow(page, 0).getByRole("button", { name: "不设限" }).click();
  await keyRow(page, 0).getByLabel(/的额度 USD$/).fill("8");
  await keyRow(page, 0).getByRole("button", { name: "保存" }).click();
  await expect(page.locator(".budget")).toContainText("可再分配 $0.00");

  await keyRow(page, 0).getByRole("button", { name: "吊销" }).click();
  // 不还回来的话，用户看着自己有额度却一分也分不出去。
  await expect(page.locator(".budget")).toContainText("可再分配 $8.00");
  await expect(page.locator(".budget")).toContainText("已分配 $0.00");
});
