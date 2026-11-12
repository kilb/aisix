import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

/**
 * The portal's prices page: one parent per granted model, listing each assigned
 * group and what that group charges for the model,
 * in ¥ per million tokens, at the price the organisation is actually charged
 * — its baseline × the group's rate, less its discount.
 *
 * Against the real control plane through the built `dist/`; the groups, the
 * models, the baseline, the rate and the discount are all set up through the
 * real `/admin`, and every expected figure is computed here from those
 * numbers rather than read off the page.
 */

let stack: Stack;

const PASSWORD = "portal-prices-pass-3f7";

/** The baseline's USD per million tokens for each billed dimension. */
const CARD = { input: "4", output: "20", cache_read: "0.8", cache_write: "5" };
/** The organisation's discount, in basis points. */
const DISCOUNT_BPS = 1_000;
/** The rate each group is created at, in CNY per USD. */
const RATE_A = "3";
const RATE_B = "2";

/** The groups a test assigns to an organisation. */
const GROUP_A = { name: "grp-price-a-3f7", display: "价 A 3f7", description: "分组 A 的说明 3f7" };
const GROUP_B = { name: "grp-price-b-3f7", display: "价 B 3f7", description: "分组 B 的说明 3f7" };
/** A group whose routing name must never reach the page. */
const GROUP_ROUTE = { name: "grp-price-route-3f7", display: "只看显示名 3f7", description: "路由名不可见 3f7" };

/** The model every organisation here is granted; it has the baseline above. */
let pricedModelId: string;
/** A granted model with no baseline: its price is unknown, so it is not listed. */
let unpricedModelId: string;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
  expect(stack.mockUpstreamUrl).toBe("");
  for (const [group, rate] of [
    [GROUP_A, RATE_A],
    [GROUP_B, RATE_B],
    [GROUP_ROUTE, RATE_A],
  ] as const) {
    await stack.admin.post("/admin/provider-groups", {
      name: group.name,
      display_name: group.display,
      description: group.description,
      default_cny_per_usd: rate,
    });
  }
  const account = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: "acct-price-3f7",
      provider: "openai",
      api_key: "sk-acct-price-3f7-upstream",
      api_base: "https://api.example.test/v1",
      groups: [GROUP_A.name, GROUP_B.name, GROUP_ROUTE.name],
    },
  });
  const model = await stack.admin.post("/admin/resources/models", {
    doc: { display_name: "chat-price-3f7", provider: "openai", model_name: "gpt-4o-mini", provider_key_id: account.id },
  });
  pricedModelId = model.id;
  const unpriced = await stack.admin.post("/admin/resources/models", {
    doc: { display_name: "chat-noprice-3f7", provider: "openai", model_name: "gpt-4o-mini", provider_key_id: account.id },
  });
  unpricedModelId = unpriced.id;
  // A price cannot start in the past, so it starts at the earliest instant
  // the control plane allows, which is already in force.
  await stack.admin.post("/admin/pricing/baselines", {
    model_id: pricedModelId,
    effective_from: new Date(Date.now() - 4_000).toISOString(),
    card: { default: { base: CARD } },
    note: "价格页 e2e 的价目卡 3f7",
  });
});

test.afterAll(async () => {
  await stack?.stop();
});

/**
 * The ¥/M the page must show for one dimension: the baseline's USD per
 * million × the group's rate × what the discount leaves, computed exactly in
 * integer arithmetic and then written to two decimals.
 */
function yuanPerMillion(usdPerMillion: string, cnyPerUsd: string, discountBps: number): string {
  const scaled = (text: string, decimals: number): bigint => {
    const [whole = "0", fraction = ""] = text.split(".");
    return BigInt(whole) * 10n ** BigInt(decimals) + BigInt(fraction.padEnd(decimals, "0").slice(0, decimals));
  };
  // µ$ per million × (rate × 10⁴) × basis points: units of 10⁻¹⁴ ¥.
  const exact = scaled(usdPerMillion, 6) * scaled(cnyPerUsd, 4) * BigInt(10_000 - discountBps);
  const hundredths = (exact * 100n) / 10n ** 14n;
  return `¥${hundredths / 100n}.${(hundredths % 100n).toString().padStart(2, "0")}`;
}

/**
 * An organisation signed up by `ownerEmail`, approved, granted both models,
 * assigned `groups` and given the discount every figure here assumes; its id.
 */
async function activeOrg(name: string, ownerEmail: string, groups: string[]): Promise<string> {
  const signedUp = await stack.tenant().post("/api/signup", { org_name: name, email: ownerEmail, password: PASSWORD });
  const id: string = signedUp.tenant.id;
  await stack.admin.post(`/admin/tenants/${id}/approve`);
  const current = await stack.admin.get(`/admin/tenants/${id}`);
  await stack.admin.patch(
    `/admin/tenants/${id}`,
    { allowed_model_ids: [pricedModelId, unpricedModelId], allowed_provider_groups: groups },
    current.version,
  );
  await stack.admin.post("/admin/pricing/discounts", {
    tenant_id: id,
    effective_from: new Date(Date.now() - 4_000).toISOString(),
    discount_bps: DISCOUNT_BPS,
  });
  return id;
}

/** `email` joins organisation `id` as `role`. */
async function joined(id: string, email: string, role: string): Promise<void> {
  const invite = await stack.admin.post(`/admin/tenants/${id}/invites`, { email, role });
  await stack.tenant().post("/api/invites/accept", { token: invite.invite_token, password: PASSWORD });
}

async function login(page: Page, email: string): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令", { exact: true }).fill(PASSWORD);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await expect(page.getByRole("button", { name: "登出" })).toBeVisible();
}

/** 每个模型只出现一次；分组行属于该模型，不随分页分离。 */
function modelEntry(page: Page, model: string): Locator {
  return page.getByRole("table", { name: "模型单价", exact: true }).locator("tbody[data-price-model]")
    .filter({ has: page.getByText(model, { exact: true }) });
}

function groupRow(page: Page, model: string, group: string): Locator {
  return modelEntry(page, model).locator(":scope > tr[data-price-group]").filter({ hasText: group });
}

/** 数值仍来自独立的基准价、分组换算率与组织优惠。 */
async function rowPrices(page: Page, model: string, group: string, rate: string): Promise<void> {
  const row = groupRow(page, model, group);
  for (const [index, expected] of [
    yuanPerMillion(CARD.input, rate, DISCOUNT_BPS),
    yuanPerMillion(CARD.output, rate, DISCOUNT_BPS),
    yuanPerMillion(CARD.cache_read, rate, DISCOUNT_BPS),
    yuanPerMillion(CARD.cache_write, rate, DISCOUNT_BPS),
  ].entries()) await expect(row.getByRole("cell").nth(index + 1)).toHaveText(expected);
}

test("模型下同时列出各授权分组的人民币单价（含组织折扣）", async ({ page }) => {
  await activeOrg("Prices Org 1b9", "owner@prices-1b9.test", [GROUP_A.name, GROUP_B.name]);
  await login(page, "owner@prices-1b9.test");
  await page.getByRole("tab", { name: "价格", exact: true }).click();
  const model = modelEntry(page, "chat-price-3f7");
  await expect(model).toHaveCount(1);
  await expect(model.locator(":scope > tr[data-price-group]")).toHaveCount(2);
  await expect(model).toContainText(GROUP_A.description);
  await expect(model).toContainText(GROUP_B.description);
  await expect(page.getByText(/价格以人民币计，按实际用量计费/)).toBeVisible();
  await rowPrices(page, "chat-price-3f7", GROUP_A.display, RATE_A);
  await rowPrices(page, "chat-price-3f7", GROUP_B.display, RATE_B);
  await expect(modelEntry(page, "chat-noprice-3f7")).toHaveCount(0);
  const shown = await page.locator("body").innerText();
  for (const wrong of [
    yuanPerMillion(CARD.input, RATE_A, 0),
    yuanPerMillion(CARD.output, RATE_A, 0),
    yuanPerMillion(CARD.input, RATE_A, DISCOUNT_BPS * 2),
  ]) expect(shown).not.toContain(wrong);
});

test("成员角色也能看到价格页", async ({ page }) => {
  const id = await activeOrg("Prices Member Org 2c4", "owner@prices-2c4.test", [GROUP_A.name]);
  await joined(id, "member@prices-2c4.test", "member");
  await login(page, "member@prices-2c4.test");

  // The tab is there for a member, and the page answers with the prices.
  await page.getByRole("tab", { name: "价格", exact: true }).click();
  await expect(page.getByLabel("分组筛选").getByRole("option")).toHaveText(["全部分组", GROUP_A.display]);
  await rowPrices(page, "chat-price-3f7", GROUP_A.display, RATE_A);
});

test("创建密钥时的查看价格链接打开所选分组", async ({ page }) => {
  await activeOrg("Prices Link Org 3d5", "owner@prices-3d5.test", [GROUP_A.name, GROUP_B.name]);
  await login(page, "owner@prices-3d5.test");
  await page.getByRole("tab", { name: "密钥", exact: true }).click();

  // The second group, the one that is not preselected and not the page's
  // first tab: the link must open that one and not A.
  const picker = page.getByRole("group", { name: "分组" });
  await picker.getByRole("radio", { name: new RegExp(GROUP_B.display) }).check();
  await page.getByRole("link", { name: "查看价格" }).click();

  await expect(page.getByRole("tab", { name: "价格", exact: true })).toHaveAttribute("aria-selected", "true");
  await expect(page.getByLabel("分组筛选").locator("option:checked")).toHaveText(GROUP_B.display);
  await rowPrices(page, "chat-price-3f7", GROUP_B.display, RATE_B);
  await expect(groupRow(page, "chat-price-3f7", GROUP_A.display)).toHaveCount(0);

  // The address says which group it shows, so it can be shared or reloaded.
  expect(page.url()).toContain("prices");
  await page.reload();
  await expect(page.getByLabel("分组筛选").locator("option:checked")).toHaveText(GROUP_B.display);

  // Back to 密钥 through the nav, and the same link again: it opens the same
  // group a second time, and the address follows the nav in between.
  await page.getByRole("tab", { name: "密钥", exact: true }).click();
  await expect.soft(page).toHaveURL(/#keys$/);
  await page.getByRole("group", { name: "分组" }).getByRole("radio", { name: new RegExp(GROUP_B.display) }).check();
  await page.getByRole("link", { name: "查看价格" }).click();
  await expect(page.getByRole("tab", { name: "价格", exact: true })).toHaveAttribute("aria-selected", "true");
  await expect(page.getByLabel("分组筛选").locator("option:checked")).toHaveText(GROUP_B.display);
});

test("价格页不出现分组的路由名", async ({ page }) => {
  await activeOrg("Prices Route Org 4e6", "owner@prices-4e6.test", [GROUP_ROUTE.name]);
  await login(page, "owner@prices-4e6.test");
  await page.getByRole("tab", { name: "价格", exact: true }).click();

  // The display name and description are what the page shows.
  await expect(page.getByLabel("分组筛选").getByRole("option")).toHaveText(["全部分组", GROUP_ROUTE.display]);
  await expect(groupRow(page, "chat-price-3f7", GROUP_ROUTE.display)).toContainText(GROUP_ROUTE.description);
  // The routing name, which decides what the group reaches, appears nowhere.
  const seen = [await page.locator("body").innerText(), await page.content()];
  for (const text of seen) expect.soft(text).not.toContain(GROUP_ROUTE.name);
});

/**
 * A one-hour cache write is billed at its own rate when the baseline gives
 * one, so the page shows it beside the default write rather than leaving
 * that rate for a reader to guess. A model with no such rate shows nothing
 * extra (the existing cases pin that row to exactly its five cells).
 */
test("基准价单列 1 小时缓存写时，价格页在缓存写下另列这一价", async ({ page }) => {
  const card = { input: "3", output: "15", cache_read: "0.3", cache_write: "3.5", cache_write_1h: "6" };
  const account = (await stack.admin.get("/admin/resources/provider_keys")).items.find(
    (k: { doc: { display_name?: string } }) => k.doc.display_name === "acct-price-3f7",
  );
  const model = await stack.admin.post("/admin/resources/models", {
    doc: { display_name: "chat-1h-3f7", provider: "openai", model_name: "gpt-4o-mini", provider_key_id: account.id },
  });
  await stack.admin.post("/admin/pricing/baselines", {
    model_id: model.id,
    effective_from: new Date(Date.now() - 4_000).toISOString(),
    card: { default: { base: card } },
    note: "价格页 e2e 的 1 小时缓存写 3f7",
  });
  const id = await activeOrg("Prices 1h Org 5f8", "owner@prices-5f8.test", [GROUP_A.name]);
  const current = await stack.admin.get(`/admin/tenants/${id}`);
  await stack.admin.patch(`/admin/tenants/${id}`, { allowed_model_ids: [pricedModelId, model.id] }, current.version);

  await login(page, "owner@prices-5f8.test");
  await page.getByRole("tab", { name: "价格", exact: true }).click();
  const row = groupRow(page, "chat-1h-3f7", GROUP_A.display);
  await expect(row.getByRole("cell").nth(0)).toContainText("2.7×");
  await expect(row.getByRole("cell").filter({ hasNot: page.locator(".price-multiplier-value") })).toHaveText([
    yuanPerMillion(card.input, RATE_A, DISCOUNT_BPS),
    yuanPerMillion(card.output, RATE_A, DISCOUNT_BPS),
    yuanPerMillion(card.cache_read, RATE_A, DISCOUNT_BPS),
    `${yuanPerMillion(card.cache_write, RATE_A, DISCOUNT_BPS)}1 小时缓存写 ${yuanPerMillion(card.cache_write_1h, RATE_A, DISCOUNT_BPS)}`,
  ]);
  // The model without a one-hour rate in the same table keeps its five plain cells.
  await rowPrices(page, "chat-price-3f7", GROUP_A.display, RATE_A);
});

/**
 * A baseline with no cache rates of its own bills cached tokens at its input
 * rate, so the page shows that price in both cache columns and says it is
 * the input price, rather than a dash a reader takes for "free" or "unknown".
 */
test("基准价没有缓存价时，价格页的缓存读写按输入价显示并注明", async ({ page }) => {
  const card = { input: "2", output: "8" };
  const account = (await stack.admin.get("/admin/resources/provider_keys")).items.find(
    (k: { doc: { display_name?: string } }) => k.doc.display_name === "acct-price-3f7",
  );
  const model = await stack.admin.post("/admin/resources/models", {
    doc: { display_name: "chat-nocache-3f7", provider: "openai", model_name: "gpt-4o-mini", provider_key_id: account.id },
  });
  await stack.admin.post("/admin/pricing/baselines", {
    model_id: model.id,
    effective_from: new Date(Date.now() - 4_000).toISOString(),
    card: { default: { base: card } },
    note: "价格页 e2e 的无缓存价 3f7",
  });
  const id = await activeOrg("Prices NoCache Org 6a1", "owner@prices-6a1.test", [GROUP_A.name]);
  const current = await stack.admin.get(`/admin/tenants/${id}`);
  await stack.admin.patch(`/admin/tenants/${id}`, { allowed_model_ids: [pricedModelId, model.id] }, current.version);

  await login(page, "owner@prices-6a1.test");
  await page.getByRole("tab", { name: "价格", exact: true }).click();
  const row = groupRow(page, "chat-nocache-3f7", GROUP_A.display);
  const input = yuanPerMillion(card.input, RATE_A, DISCOUNT_BPS);
  await expect(row.getByRole("cell").filter({ hasNot: page.locator(".price-multiplier-value") })).toHaveText([
    input,
    yuanPerMillion(card.output, RATE_A, DISCOUNT_BPS),
    `${input}按输入价计费`,
    `${input}按输入价计费`,
  ]);
  // A model with its own cache rates keeps its plain cells.
  await rowPrices(page, "chat-price-3f7", GROUP_A.display, RATE_A);
});
