import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

/**
 * The portal's prices page: one tab per group the organisation is assigned,
 * each showing what that group charges for every granted model it can price,
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
  stack = await startStack();
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
      api_base: `${stack.mockUpstreamUrl}/v1`,
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

/** The prices page's own tab list: one stop per assigned group. */
function groupTabs(page: Page) {
  return page.getByRole("tablist", { name: "分组" });
}

/** One model's row: its name, then input, output, cache read and cache write in that column order. */
async function rowPrices(panel: Locator, model: string, rate: string): Promise<void> {
  await expect(panel.getByRole("row", { name: new RegExp(model) }).getByRole("cell")).toHaveText([
    model,
    yuanPerMillion(CARD.input, rate, DISCOUNT_BPS),
    yuanPerMillion(CARD.output, rate, DISCOUNT_BPS),
    yuanPerMillion(CARD.cache_read, rate, DISCOUNT_BPS),
    yuanPerMillion(CARD.cache_write, rate, DISCOUNT_BPS),
  ]);
}

test("每个分组一个页签，列出已授权模型的人民币单价（含组织折扣）", async ({ page }) => {
  await activeOrg("Prices Org 1b9", "owner@prices-1b9.test", [GROUP_A.name, GROUP_B.name]);
  await login(page, "owner@prices-1b9.test");
  await page.getByRole("tab", { name: "价格", exact: true }).click();

  // One tab per assigned group, by display name, and nothing else offered.
  const tabs = groupTabs(page);
  await expect(tabs.getByRole("tab")).toHaveText([GROUP_A.display, GROUP_B.display]);
  await expect(tabs.getByRole("tab", { name: GROUP_A.display })).toHaveAttribute("aria-selected", "true");

  // The first group's tab: its description, the note, and one row per model
  // it can price, at baseline × rate less the discount.
  const panel = page.getByRole("tabpanel", { name: GROUP_A.display });
  await expect(panel).toContainText(GROUP_A.description);
  await expect(panel).toContainText("价格以人民币计，按实际用量计费");
  await rowPrices(panel, "chat-price-3f7", RATE_A);
  // The unpriced model is unknown rather than free, so it is not a row at all.
  await expect(panel).not.toContainText("chat-noprice-3f7");

  // The second group, at its own rate: the same baseline, a different price.
  await tabs.getByRole("tab", { name: GROUP_B.display }).click();
  await expect(tabs.getByRole("tab", { name: GROUP_B.display })).toHaveAttribute("aria-selected", "true");
  const second = page.getByRole("tabpanel", { name: GROUP_B.display });
  await expect(second).toContainText(GROUP_B.description);
  await rowPrices(second, "chat-price-3f7", RATE_B);

  // The discount is the API's and appears once: neither the undiscounted
  // price nor the same price discounted a second time may be shown.
  const shown = await page.locator("body").innerText();
  for (const wrong of [
    yuanPerMillion(CARD.input, RATE_A, 0),
    yuanPerMillion(CARD.output, RATE_A, 0),
    yuanPerMillion(CARD.input, RATE_A, DISCOUNT_BPS * 2),
  ]) {
    expect(shown).not.toContain(wrong);
  }
});

test("成员角色也能看到价格页", async ({ page }) => {
  const id = await activeOrg("Prices Member Org 2c4", "owner@prices-2c4.test", [GROUP_A.name]);
  await joined(id, "member@prices-2c4.test", "member");
  await login(page, "member@prices-2c4.test");

  // The tab is there for a member, and the page answers with the prices.
  await page.getByRole("tab", { name: "价格", exact: true }).click();
  await expect(groupTabs(page).getByRole("tab")).toHaveText([GROUP_A.display]);
  await rowPrices(page.getByRole("tabpanel", { name: GROUP_A.display }), "chat-price-3f7", RATE_A);
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
  await expect(groupTabs(page).getByRole("tab", { name: GROUP_B.display })).toHaveAttribute("aria-selected", "true");
  await rowPrices(page.getByRole("tabpanel", { name: GROUP_B.display }), "chat-price-3f7", RATE_B);

  // The address says which group it shows, so it can be shared or reloaded.
  expect(page.url()).toContain("prices");
  await page.reload();
  await expect(groupTabs(page).getByRole("tab", { name: GROUP_B.display })).toHaveAttribute("aria-selected", "true");

  // Back to 密钥 through the nav, and the same link again: it opens the same
  // group a second time, and the address follows the nav in between.
  await page.getByRole("tab", { name: "密钥", exact: true }).click();
  await expect.soft(page).toHaveURL(/#keys$/);
  await page.getByRole("group", { name: "分组" }).getByRole("radio", { name: new RegExp(GROUP_B.display) }).check();
  await page.getByRole("link", { name: "查看价格" }).click();
  await expect(page.getByRole("tab", { name: "价格", exact: true })).toHaveAttribute("aria-selected", "true");
  await expect(groupTabs(page).getByRole("tab", { name: GROUP_B.display })).toHaveAttribute("aria-selected", "true");
});

test("价格页不出现分组的路由名", async ({ page }) => {
  await activeOrg("Prices Route Org 4e6", "owner@prices-4e6.test", [GROUP_ROUTE.name]);
  await login(page, "owner@prices-4e6.test");
  await page.getByRole("tab", { name: "价格", exact: true }).click();

  // The display name and description are what the page shows.
  await expect(groupTabs(page).getByRole("tab")).toHaveText([GROUP_ROUTE.display]);
  await expect(page.getByRole("tabpanel", { name: GROUP_ROUTE.display })).toContainText(GROUP_ROUTE.description);
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
  const panel = page.getByRole("tabpanel", { name: GROUP_A.display });
  await expect(panel.getByRole("row", { name: /chat-1h-3f7/ }).getByRole("cell")).toHaveText([
    "chat-1h-3f7",
    yuanPerMillion(card.input, RATE_A, DISCOUNT_BPS),
    yuanPerMillion(card.output, RATE_A, DISCOUNT_BPS),
    yuanPerMillion(card.cache_read, RATE_A, DISCOUNT_BPS),
    `${yuanPerMillion(card.cache_write, RATE_A, DISCOUNT_BPS)}1 小时缓存写 ${yuanPerMillion(card.cache_write_1h, RATE_A, DISCOUNT_BPS)}`,
  ]);
  // The model without a one-hour rate in the same table keeps its five plain cells.
  await rowPrices(panel, "chat-price-3f7", RATE_A);
});
