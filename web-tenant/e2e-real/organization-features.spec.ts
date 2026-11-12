import { expect, test } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "../e2e/stack";
import { COMPLETE_RATES, DISCOUNT_BPS, expectedMicroCny, fullPriceModel, grantLaterModel, invitedMember, login, mintedKey, PASSWORD, realCall, recorded, seedRealWorld, signedIn } from "./organization-real-world";

let stack: Stack;
test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
});
test.afterAll(async () => { await stack?.stop(); });
test.afterEach(async ({ page }, info) => {
  if (info.status !== info.expectedStatus) await page.screenshot({ path: info.outputPath("organization-failure.png"), fullPage: true });
});

test("不限模型显式单选，指定区域折叠，既有密钥继承后续授权", async ({ page }) => {
  const world = await seedRealWorld(stack);
  const closed = await world.api.post("/api/keys", { label: "既有空选范围", model_ids: [] });
  await login(page, stack, world.owner);
  await page.getByRole("tab", { name: "密钥", exact: true }).click();
  const unrestricted = page.getByRole("radio", { name: "不限制模型（继承组织授权）", exact: true });
  await expect(unrestricted).toBeChecked();
  await expect(page.getByRole("checkbox", { name: world.model.name, exact: true })).toHaveCount(0);
  await page.getByLabel("名称", { exact: true }).fill("继承后续授权");
  const createdResponse = page.waitForResponse((r) => r.url().endsWith("/api/keys") && r.request().method() === "POST");
  await page.getByRole("button", { name: "新建密钥", exact: true }).click();
  const created = await createdResponse;
  expect(created.status()).toBe(201);
  const sent = created.request().postDataJSON();
  expect(sent.model_ids === undefined || sent.model_ids === null).toBe(true);
  const body = await created.json();
  const later = await grantLaterModel(stack, world);
  await expect.poll(async () => {
    const r = await fetch(`${stack.proxyUrl}/v1/models`, { headers: { authorization: `Bearer ${body.plaintext}` } });
    const j = await r.json(); return j.data?.some((m: { id: string }) => m.id === later.name) ?? false;
  }, { timeout: 60_000 }).toBe(true);
  await page.getByRole("radio", { name: "指定模型", exact: true }).check();
  await expect(page.getByRole("button", { name: "新建密钥", exact: true })).toBeDisabled();
  await page.getByRole("button", { name: "展开模型列表", exact: true }).click();
  await page.getByRole("searchbox", { name: /模型/ }).fill(world.model.name);
  await page.getByRole("checkbox", { name: world.model.name, exact: true }).check();
  await expect(page.getByText(/已选\s*1/)).toBeVisible();
  await page.getByLabel("名称", { exact: true }).fill("明确指定模型");
  const selectedResponse = page.waitForResponse((r) => r.url().endsWith("/api/keys") && r.request().method() === "POST");
  await page.getByRole("button", { name: "新建密钥", exact: true }).click();
  const selected = await selectedResponse;
  expect(selected.status()).toBe(201);
  expect(selected.request().postDataJSON().model_ids).toEqual([world.model.id]);
  const keys = await world.api.get("/api/keys");
  expect(keys.items.find((key: { id: string }) => key.id === closed.key.id).model_ids).toEqual([]);
  const tenant = await stack.admin.get(`/admin/tenants/${world.tenantId}`);
  await stack.admin.patch(`/admin/tenants/${world.tenantId}`, { allowed_model_ids: [world.model.id] }, tenant.version);
  await expect.poll(async () => {
    const res = await fetch(`${stack.proxyUrl}/v1/models`, { headers: { authorization: `Bearer ${body.plaintext}` } });
    const catalogue = await res.json();
    const names = catalogue.data?.map((model: { id: string }) => model.id) ?? [];
    return names.includes(world.model.name) && !names.includes(later.name);
  }, { timeout: 60_000 }).toBe(true);
});

test("有信用但无组织预算时明确区分可用额度与额外预算上限", async ({ page }) => {
  const world = await seedRealWorld(stack, { credit: "0", creditLimit: "12.34" });
  await mintedKey(stack, world.api, "信用可用");
  expect((await world.api.get("/api/budgets")).items).toEqual([]);
  await login(page, stack, world.owner);
  await page.getByRole("tab", { name: "预算", exact: true }).click();
  await expect(page.getByRole("region", { name: "成员额度", exact: true })).toBeVisible();
  await expect(page.getByText("未设置额外组织预算上限", { exact: true })).toBeVisible();
  await expect(page.getByText(/账户可用额度/)).toBeVisible();
  await expect(page.getByRole("region", { name: "账户额度", exact: true })).toContainText("12.34");
  await expect(page.getByText("没有额度", { exact: true })).toHaveCount(0);
});

test("组织直接创建新账号并真实登录，重复邮箱不改变原密码", async ({ page, browser }) => {
  const world = await seedRealWorld(stack);
  await login(page, stack, world.owner);
  await page.getByRole("tab", { name: "组织", exact: true }).click();
  const heading = page.getByRole("heading", { name: "创建账号", exact: true });
  await expect(heading).toBeVisible();
  const form = page.getByRole("region", { name: "创建账号", exact: true });
  const email = `created-${Date.now()}@organization-real.test`;
  await form.getByLabel("显示名", { exact: true }).fill("真实创建成员");
  await form.getByLabel("邮箱", { exact: true }).fill(email);
  await form.getByLabel("初始密码", { exact: true }).fill(PASSWORD);
  await form.getByLabel("角色", { exact: true }).selectOption("member");
  await form.getByRole("button", { name: "创建账号", exact: true }).click();
  await expect(page.getByRole("status").filter({ hasText: "账号已创建" })).toBeVisible();
  const createdApi = await signedIn(stack, { email, password: PASSWORD });
  const context = await browser.newContext();
  try {
    const memberPage = await context.newPage();
    await login(memberPage, stack, { email, password: PASSWORD });
    await expect(memberPage.getByRole("tab", { name: "成员", exact: true })).toHaveCount(0);
    await expect(memberPage.getByRole("tab", { name: "账户", exact: true })).toHaveCount(0);
    expect((await createdApi.request("POST", "/api/members", { email: `forbidden-${email}`, password: PASSWORD, role: "owner" })).status).toBe(403);
    await form.getByLabel("显示名", { exact: true }).fill("重复不重置");
    await form.getByLabel("邮箱", { exact: true }).fill(email);
    await form.getByLabel("初始密码", { exact: true }).fill("Changed-password-should-not-work");
    const duplicate = page.waitForResponse((r) => r.url().endsWith("/api/members") && r.request().method() === "POST");
    await form.getByRole("button", { name: "创建账号", exact: true }).click();
    expect((await duplicate).status()).toBe(409);
    await expect(form.getByText(/邮箱已被使用/)).toBeVisible();
    await signedIn(stack, { email, password: PASSWORD });
    const admin = await invitedMember(stack, world, "admin");
    const adminContext = await browser.newContext();
    try {
      const adminPage = await adminContext.newPage();
      await login(adminPage, stack, admin);
      await adminPage.getByRole("tab", { name: "组织", exact: true }).click();
      await expect(adminPage.getByLabel("角色", { exact: true }).getByRole("option", { name: "所有者", exact: true })).toHaveCount(0);
    } finally {
      await adminContext.close();
    }
  } finally {
    await context.close();
  }
});

test("完整组织价格含十一维、档位、长上下文和优惠，复制完整调用名", async ({ page, context }) => {
  const world = await seedRealWorld(stack);
  const model = await fullPriceModel(stack, world);
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  await login(page, stack, world.owner);
  await page.getByRole("tab", { name: "价格", exact: true }).click();
  await expect(page.getByText(/13\.75%/)).toBeVisible();
  const row = page.getByRole("row").filter({ hasText: model.name });
  await expect(row).toContainText(model.name);
  await row.getByRole("button", { name: "复制模型名", exact: true }).click();
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(model.name);
  const copiedName = await page.evaluate(() => navigator.clipboard.readText());
  const copiedKey = await mintedKey(stack, world.api, "复制调用名验收");
  const copiedCall = await realCall(stack, copiedKey.plaintext, copiedName);
  const copiedRecord = await recorded(world.api, copiedCall.requestId);
  expect(copiedRecord.status).toBe("succeeded");
  expect(copiedRecord.model.name).toBe(copiedName);
  await row.getByRole("button", { name: "查看全部价格", exact: true }).click();
  for (const dimension of ["输入", "输出", "缓存读", "缓存写", "1 小时缓存写", "推理", "音频输入", "音频输出", "图像输入", "图像输出", "联网搜索"]) {
    await expect(page.getByRole("cell", { name: dimension, exact: true }).first()).toBeVisible();
  }
  await expect(page.getByText("batch", { exact: true })).toBeVisible();
  await expect(page.getByText(/输入 > 200,000 token/).first()).toBeVisible();
  await expect(page.getByText(/元\/千次/).first()).toBeVisible();
  const quote = await world.api.get("/api/prices");
  const quoted = quote.groups.flatMap((g: { models: any[] }) => g.models).find((m: { model_id: string }) => m.model_id === model.id);
  expect(quote.discount_bps).toBe(DISCOUNT_BPS);
  const fraction = (text: string): [bigint, bigint] => {
    const [whole, decimals = ""] = text.split(".");
    return [BigInt(`${whole}${decimals}`), 10n ** BigInt(decimals.length)];
  };
  const exactDiscount = (shown: string | null, original: string): void => {
    expect(shown).not.toBeNull();
    const [actual, actualScale] = fraction(shown!);
    const [base, baseScale] = fraction(original);
    expect(actual * baseScale * 10000n).toBe(base * actualScale * BigInt(10000 - DISCOUNT_BPS));
  };
  for (const tier of ["default", "batch"] as const) {
    const base = { ...COMPLETE_RATES, ...(tier === "batch" ? { input: "5.5", output: "11" } : {}) };
    const band = { ...COMPLETE_RATES, input: tier === "batch" ? "55.5" : "111", output: tier === "batch" ? "111" : "222" };
    const card = quoted.prices_by_tier[tier];
    expect(card.base.map((r: { dimension: string }) => r.dimension).sort()).toEqual(Object.keys(COMPLETE_RATES).sort());
    expect(card.long_context).toHaveLength(1);
    expect(card.long_context[0].above_input_tokens).toBe(200000);
    for (const [expected, rates] of [[base, card.base], [band, card.long_context[0].rates]] as const) {
      expect(rates).toHaveLength(11);
      for (const rate of rates) {
        expect(rate.unit).toBe(rate.dimension === "web_search" ? "request" : "token");
        exactDiscount(rate.price_cny_per_million, expected[rate.dimension as keyof typeof expected]);
      }
    }
  }
  expect(quoted.prices_by_tier.default.base.find((r: { dimension: string }) => r.dimension === "cache_read").price_cny_per_million).toBe("0");
  const fallback = quote.groups.flatMap((g: { models: any[] }) => g.models).find((m: { model_id: string }) => m.model_id === world.model.id);
  expect(fallback.prices_by_tier.default.base.find((r: { dimension: string }) => r.dimension === "web_search")).toMatchObject({ price_cny_per_million: null });
  expect(fallback.prices_by_tier.default.base.find((r: { dimension: string }) => r.dimension === "cache_read")).toMatchObject({ fallback_from: "input" });
});

test("真实流式与非流式费用保留历史折扣，余额公式与首token分别展示", async ({ page }) => {
  test.setTimeout(180_000);
  const world = await seedRealWorld(stack);
  const key = await mintedKey(stack, world.api, "真实费用调用");
  const ordinary = await realCall(stack, key.plaintext, world.model.name);
  const streamed = await realCall(stack, key.plaintext, world.model.name, true);
  const ordinaryRow = await recorded(world.api, ordinary.requestId);
  const streamedRow = await recorded(world.api, streamed.requestId);
  expect(String(ordinaryRow.charge.amount_micro_cny)).toBe(expectedMicroCny(ordinary.usage));
  expect(String(streamedRow.charge.amount_micro_cny)).toBe(expectedMicroCny(streamed.usage));
  await login(page, stack, world.owner);
  const summaryResponse = page.waitForResponse((response) => new URL(response.url()).pathname.endsWith("/api/usage/summary") && response.status() === 200);
  await page.getByRole("tab", { name: "用量", exact: true }).click();
  const apiSummary = await (await summaryResponse).json();
  const summaryTable = page.getByRole("table", { name: "用量汇总", exact: true });
  await expect(summaryTable.getByRole("columnheader")).toHaveText(["期间", "模型", "调用次数", "费用", "计量"]);
  const summaryRows = summaryTable.locator("tbody > tr");
  await expect(summaryRows).toHaveCount(apiSummary.items.length);
  const shownFees: string[] = [];
  for (const summaryRow of await summaryRows.all()) {
    await expect(summaryRow.getByRole("cell")).toHaveCount(5);
    shownFees.push((await summaryRow.getByRole("cell").nth(3).textContent())?.trim() ?? "");
  }
  const expectedFees = apiSummary.items.map((item: { charged_micro_cny: number }) => {
    const micro = BigInt(item.charged_micro_cny);
    return `¥${micro / 1000000n}.${(micro % 1000000n).toString().padStart(6, "0")}`.replace(/0+$/, "").replace(/\.$/, ".00");
  });
  expect(shownFees.sort()).toEqual(expectedFees.sort());
  expect(apiSummary.items.reduce((total: bigint, item: { charged_micro_cny: number }) => total + BigInt(item.charged_micro_cny), 0n)).toBe(BigInt(expectedMicroCny(ordinary.usage)) + BigInt(expectedMicroCny(streamed.usage)));
  const log = page.getByRole("region", { name: "调用记录", exact: true });
  await expect(log.locator("tbody > tr:not(.usage-detail)")).toHaveCount(2);
  await log.getByRole("button", { name: "展开明细", exact: true }).first().click();
  await expect(log.getByText("费用计算", { exact: true })).toBeVisible();
  await expect(log.getByText(/13\.75%/)).toBeVisible();
  expect(streamedRow).toHaveProperty("first_token_latency_ms");
  expect(streamedRow.first_token_latency_ms).not.toBeNull();
  expect(streamedRow.first_token_latency_ms).toBeGreaterThanOrEqual(0);
  expect(ordinaryRow).toHaveProperty("first_token_latency_ms", null);
  await expect(log.getByText(/首 token/).first()).toBeVisible();
  await stack.admin.post("/admin/pricing/discounts", { tenant_id: world.tenantId, discount_bps: 5000, effective_from: new Date(Date.now() - 3000).toISOString() });
  const historical = await recorded(world.api, ordinary.requestId);
  expect(historical.charge.discount_bps).toBe(1375);
  expect(String(historical.charge.calculation.final_micro_cny)).toBe(expectedMicroCny(ordinary.usage));
  await page.getByRole("tab", { name: "账户", exact: true }).click();
  await expect(page.getByText("余额计算", { exact: true })).toBeVisible();
  const balance = (await world.api.get("/api/billing/balance")).calculation;
  for (const name of ["topups_micro_cny", "usage_micro_cny", "adjustments_micro_cny", "balance_micro_cny", "credit_limit_micro_cny", "available_micro_cny"]) expect(balance).toHaveProperty(name);
  expect(BigInt(balance.topups_micro_cny) - BigInt(balance.usage_micro_cny) + BigInt(balance.adjustments_micro_cny)).toBe(BigInt(balance.balance_micro_cny));
  expect(BigInt(balance.balance_micro_cny) + BigInt(balance.credit_limit_micro_cny)).toBe(BigInt(balance.available_micro_cny));
});

test("成员页汇总两名成员真实用量与各自预算，普通成员只见自己调用", async ({ page, browser }) => {
  test.setTimeout(180_000);
  const world = await seedRealWorld(stack);
  const alice = await invitedMember(stack, world);
  const bob = await invitedMember(stack, world);
  const aliceApi = await signedIn(stack, alice);
  const bobApi = await signedIn(stack, bob);
  const aliceKey = await mintedKey(stack, aliceApi, "成员甲");
  const bobKey = await mintedKey(stack, bobApi, "成员乙");
  await world.api.post("/api/budgets", { scope: "member", scope_ref: alice.id, kind: "periodic", period: "month", limit_cny: "1.23" });
  const aliceCall = await realCall(stack, aliceKey.plaintext, world.model.name);
  const bobCall = await realCall(stack, bobKey.plaintext, world.model.name);
  await recorded(aliceApi, aliceCall.requestId);
  await recorded(bobApi, bobCall.requestId);
  await login(page, stack, world.owner);
  await page.getByRole("tab", { name: "成员", exact: true }).click();
  await expect(page.getByRole("columnheader", { name: /本月用量/ })).toBeVisible();
  const aliceRow = page.getByRole("row").filter({ hasText: alice.email });
  const bobRow = page.getByRole("row").filter({ hasText: bob.email });
  await expect(aliceRow).toContainText("1.23");
  await expect(bobRow).toContainText(/未.*设置.*成员/);
  for (const [row, call] of [[aliceRow, aliceCall], [bobRow, bobCall]] as const) {
    await expect(row.getByText("1 次调用", { exact: true })).toBeVisible();
    const tokens = BigInt(call.usage.input_tokens) + BigInt(call.usage.output_tokens);
    const shownTokens = tokens.toString().replace(/\B(?=(\d{3})+(?!\d))/g, ",");
    await expect(row.getByText(`计费用量 ${shownTokens} token`, { exact: true })).toBeVisible();
    const micro = BigInt(expectedMicroCny(call.usage));
    const decimal = `${micro / 1000000n}.${(micro % 1000000n).toString().padStart(6, "0")}`.replace(/0+$/, "").replace(/\.$/, "");
    await expect(row.getByText(`已计费 ¥${decimal}`, { exact: true })).toBeVisible();
  }
  const usage = await world.api.get("/api/members/usage");
  for (const [member, call] of [[alice, aliceCall], [bob, bobCall]] as const) {
    const item = usage.items.find((r: { member_id: string }) => r.member_id === member.id);
    expect(item).toMatchObject({ requests: 1 });
    expect(String(item.charged_micro_cny)).toBe(expectedMicroCny(call.usage));
  }
  const privateCalls = (await aliceApi.get("/api/usage/calls?limit=200")).items;
  expect(privateCalls.map((c: { request_id: string }) => c.request_id)).toContain(aliceCall.requestId);
  expect(privateCalls.map((c: { request_id: string }) => c.request_id)).not.toContain(bobCall.requestId);
  expect((await aliceApi.request("GET", "/api/members/usage")).status).toBe(403);
  const context = await browser.newContext();
  const ownPage = await context.newPage();
  const financialReads: string[] = [];
  ownPage.on("request", (request) => {
    const path = new URL(request.url()).pathname;
    if (path.startsWith("/api/billing/")) financialReads.push(path);
  });
  await login(ownPage, stack, alice);
  await expect(ownPage.getByRole("tab", { name: "成员", exact: true })).toHaveCount(0);
  await ownPage.getByRole("tab", { name: "预算", exact: true }).click();
  await expect(ownPage.getByRole("region", { name: "成员额度", exact: true })).toBeVisible();
  await expect(ownPage.getByRole("region", { name: "账户额度", exact: true })).toHaveCount(0);
  expect(financialReads).toEqual([]);
  await context.close();
});
