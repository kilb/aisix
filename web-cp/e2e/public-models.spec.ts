import { randomUUID } from "node:crypto";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "./stack";

/**
 * 价格 · 公共模型 against the real control plane through the built `dist/`:
 * creating a name (and the refusal a taken one gets), the detail view's tier
 * order, the served-nothing state, the retire confirmation and its counts,
 * and 重新生成 rewriting the documents without moving the table.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
  await stack.admin.seedGroups(["shared", "grp-pub-8e3"]);
});

test.afterAll(async () => {
  await stack?.stop();
});

const APPLIED_WITHIN_MS = 30_000;

async function newAccount(name: string, groups: string[] = ["shared", "grp-pub-8e3"]): Promise<string> {
  const created = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: name,
      provider: "openai",
      api_key: `sk-${name}-upstream`,
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups,
    },
  });
  return created.id as string;
}

async function modelDocs(): Promise<Record<string, Json>> {
  const { items } = await stack.admin.get("/admin/resources/models");
  const byName: Record<string, Json> = {};
  for (const row of items as Json[]) byName[row.doc.display_name ?? row.id] = row;
  return byName;
}

async function login(page: Page, email: string, password: string): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令", { exact: true }).fill(password);
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
}

/** Opens 价格 · 公共模型, and answers the page's region. */
async function openPublicModels(page: Page, who = stack.operator): Promise<Locator> {
  await login(page, who.email, who.password);
  await page.locator('[data-tab="pricing"]').click();
  await page.getByRole("group", { name: "价格分区" }).getByRole("button", { name: "公共模型", exact: true }).click();
  const panel = page.locator(".panel").filter({ has: page.getByRole("heading", { name: "公共模型", level: 2 }) });
  await expect(panel).toBeVisible();
  await expect(panel.getByText("载入中…")).toHaveCount(0);
  return panel;
}

/** `name` with every regex metacharacter escaped, for a role-name pattern. */
function escapeRe(name: string): string {
  return name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** The list's row for a model: its name is the row's own first cell. */
function listRow(panel: Locator, name: string): Locator {
  return panel.getByRole("row", { name: new RegExp(`^${escapeRe(name)}(?:\\s|$)`) });
}

test("新建公共模型，名称已存在时在名称字段上提示", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const name = `pub-new-${suffix}`;
  const page1 = await openPublicModels(page);
  const create = page.getByRole("form", { name: "新建公共模型" });
  await create.getByLabel("名称").fill(name);
  await create.getByRole("button", { name: "新建", exact: true }).click();
  // The toast renders at app level, outside the form.
  await expect(page.getByText(/已新建公共模型/)).toBeVisible();
  await expect(listRow(page1, name)).toBeVisible();

  // The same name again, typed into the same field: the refusal lands there,
  // and says the model it names already exists rather than adopting it.
  await create.getByLabel("名称").fill(name);
  await create.getByRole("button", { name: "新建", exact: true }).click();
  const field = create.getByLabel("名称");
  await expect(field).toHaveAttribute("aria-invalid", "true");
  await expect(create.getByText(/已有同名模型/)).toBeVisible();
  // It was not created twice: the list still has exactly one of that name.
  expect((await stack.admin.get("/admin/public-models")).items.filter((m: Json) => m.name === name)).toHaveLength(1);
});

test("公共模型详情按优先级分层展示账号", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const pub = await stack.admin.post("/admin/public-models", { name: `pub-tiers-${suffix}` });
  // Three accounts, two of them on one tier. The names are chosen so that
  // neither alphabetical order nor insertion order matches priority order:
  // `zulu` is the top tier, `alpha` the heavier of the two below it. A page
  // that sorted by name would list alpha, bravo, zulu.
  const high = await newAccount(`acct-zulu-${suffix}`);
  const midHeavy = await newAccount(`acct-alpha-${suffix}`);
  const midLight = await newAccount(`acct-bravo-${suffix}`);
  await stack.admin.put(`/admin/provider-keys/${high}/channel-models/${pub.id}`, { priority: 5, weight: 1 });
  await stack.admin.put(`/admin/provider-keys/${midHeavy}/channel-models/${pub.id}`, { priority: 2, weight: 9 });
  await stack.admin.put(`/admin/provider-keys/${midLight}/channel-models/${pub.id}`, { priority: 2, weight: 4 });

  const panel = await openPublicModels(page);
  await panel.getByRole("button", { name: `pub-tiers-${suffix}`, exact: true }).click();
  const detail = page.getByRole("region", { name: `公共模型 pub-tiers-${suffix}` });
  await expect(detail).toBeVisible();

  const rows = detail.getByRole("table", { name: "服务账号" }).getByRole("row");
  // The header, then the accounts in the document's own order. Read from the
  // page's visible text, not from the API: this is what an operator sees.
  await expect(rows.locator("td:nth-child(2)")).toHaveText([
    `acct-zulu-${suffix}`,
    `acct-alpha-${suffix}`,
    `acct-bravo-${suffix}`,
  ]);
  await expect(rows.locator("td:nth-child(1)")).toHaveText(["1", "2", ""]);
  await expect(rows.locator("td:nth-child(3)")).toHaveText(["5", "2", "2"]);
  await expect(rows.locator("td:nth-child(4)")).toHaveText(["1", "9", "4"]);
});

test("全部账号停用的公共模型显示无可用账号", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const pub = await stack.admin.post("/admin/public-models", { name: `pub-off-${suffix}` });
  const accountId = await newAccount(`acct-off-${suffix}`);
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${pub.id}`, { priority: 1, weight: 1 });
  // The row stays and only its document is withheld: the model is listed
  // with nothing serving it.
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${pub.id}`, { enabled: false });

  const panel = await openPublicModels(page);
  const row = listRow(panel, `pub-off-${suffix}`);
  await expect(row).toContainText("没有可用账号");
  await row.getByRole("button", { name: `pub-off-${suffix}`, exact: true }).click();
  const detail = page.getByRole("region", { name: `公共模型 pub-off-${suffix}` });
  await expect(detail.getByText("无可用账号")).toBeVisible();
  await expect(detail).toContainText("503");
  // The control plane's own view agrees: the model is live, with no target.
  const view = await stack.admin.get(`/admin/public-models/${pub.id}`);
  expect(view.retired_at).toBeNull();
  expect(view.served_by).toHaveLength(1);
  expect(view.served_by[0].enabled).toBe(false);
});

test("删除一个仍有租户授权的公共模型需要确认，被拒时显示租户与密钥数量", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const pub = await stack.admin.post("/admin/public-models", { name: `pub-retire-${suffix}` });
  const accountId = await newAccount(`acct-retire-${suffix}`);
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${pub.id}`, { priority: 1, weight: 1 });

  // A tenant granted the model and a key naming it: the counts the refusal
  // will carry.
  const signup = await stack.tenant().post("/api/signup", {
    org_name: `Retire Org ${suffix}`,
    email: `owner-${suffix}@retire-ui.test`,
    password: "retire-ui-pass-6c3",
  });
  const tenantId: string = signup.tenant.id;
  await stack.admin.post(`/admin/tenants/${tenantId}/approve`);
  const current = await stack.admin.get(`/admin/tenants/${tenantId}`);
  await stack.admin.patch(`/admin/tenants/${tenantId}`, { allowed_model_ids: [pub.id] }, current.version);
  const owner = stack.tenant();
  await owner.post("/api/login", { email: `owner-${suffix}@retire-ui.test`, password: "retire-ui-pass-6c3" });
  await owner.post("/api/keys", { label: `retire-key-${suffix}`, model_ids: [pub.id] });
  // And one that picks no models: it reaches every grant, this model included.
  await owner.post("/api/keys", { label: `retire-all-${suffix}` });

  const panel = await openPublicModels(page);
  const row = listRow(panel, `pub-retire-${suffix}`);
  // Both keys reach it, so both are counted, in the column and in the warning.
  await expect(row.getByRole("cell").nth(4)).toHaveText("2");
  await row.getByRole("button", { name: "删除", exact: true }).click();
  // Asked once: the warning states the counts and what has to be cleared
  // first.
  await expect(row.getByRole("alert")).toContainText("1 个租户");
  await expect(row.getByRole("alert")).toContainText("2 个密钥");
  await row.getByRole("button", { name: "确认删除" }).click();

  // Refused with the counts, and the model is untouched.
  await expect(panel.getByRole("alert").filter({ hasText: "仍在使用中" })).toContainText("1 个租户、2 个密钥");
  const view = await stack.admin.get(`/admin/public-models/${pub.id}`);
  expect(view.retired_at).toBeNull();
  expect(view.tenants).toBe(1);
});

test("重新生成改写文档而不改表格", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const pub = await stack.admin.post("/admin/public-models", { name: `pub-regen-${suffix}` });
  const accountId = await newAccount(`acct-regen-${suffix}`);
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${pub.id}`, {
    upstream_model: "upstream-regen-before",
    priority: 3,
    weight: 2,
  });
  await expect
    .poll(async () => (await modelDocs())[`pub-regen-${suffix}#acct-regen-${suffix}`]?.doc.model_name, {
      timeout: APPLIED_WITHIN_MS,
    })
    .toBe("upstream-regen-before");
  const before = await modelDocs();
  const beforeVersion = before[`pub-regen-${suffix}#acct-regen-${suffix}`].version;
  const beforeTable = await stack.admin.get(`/admin/provider-keys/${accountId}/channel-models`);

  // Divergence the regenerate path exists for: the account's own name is
  // changed, and only the generated document still carries the old one. The
  // credential comes back redacted, and the write keeps the stored one.
  const stored = await stack.admin.get(`/admin/resources/provider_keys/${accountId}`);
  await stack.admin.put(
    `/admin/resources/provider_keys/${accountId}`,
    { doc: { ...stored.doc, display_name: `acct-regen-renamed-${suffix}` } },
    stored.version,
  );

  const panel = await openPublicModels(page);
  await panel.getByRole("button", { name: `pub-regen-${suffix}`, exact: true }).click();
  const detail = page.getByRole("region", { name: `公共模型 pub-regen-${suffix}` });
  await detail.getByRole("button", { name: "重新生成" }).click();
  await expect(detail.getByText(/接入行，模型文档与表格一致|已按当前表格/)).toBeVisible();

  // The table did not move; the document was rewritten under the account's
  // new name, at a new version.
  const afterTable = await stack.admin.get(`/admin/provider-keys/${accountId}/channel-models`);
  const tableOf = (answer: Json) =>
    answer.items.find((i: Json) => i.public_model_id === pub.id);
  expect(tableOf(afterTable)).toMatchObject({
    upstream_model: tableOf(beforeTable).upstream_model,
    priority: 3,
    weight: 2,
    enabled: true,
  });
  await expect
    .poll(async () => (await modelDocs())[`pub-regen-${suffix}#acct-regen-renamed-${suffix}`] !== undefined, {
      timeout: APPLIED_WITHIN_MS,
    })
    .toBe(true);
  const after = await modelDocs();
  expect(after[`pub-regen-${suffix}#acct-regen-renamed-${suffix}`].doc.model_name).toBe("upstream-regen-before");
  expect(after[`pub-regen-${suffix}#acct-regen-renamed-${suffix}`].version).not.toBe(beforeVersion);
});

test("有分组没有换算率时显示未定价，每个分组都有换算率后显示已定价", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const name = `pub-priced-${suffix}`;
  const pub = await stack.admin.post("/admin/public-models", { name });
  const accountId = await newAccount(`acct-priced-${suffix}`);
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${pub.id}`, { priority: 1, weight: 1 });
  // A price may start at most a few seconds back, so each starts just now.
  const justNow = () => new Date(Date.now() - 2_000).toISOString();
  await stack.admin.post("/admin/pricing/baselines", {
    model_id: pub.id,
    effective_from: justNow(),
    card: { default: { base: { input: "4", output: "20" } } },
    note: "the public-model list's card",
  });
  // Every live group but `shared` gets a rate for this model; `shared`, the
  // first boot's group, is left with none.
  const groups: Json[] = (await stack.admin.get("/admin/provider-groups")).items.filter((g: Json) => !g.retired_at);
  const shared = groups.find((g) => g.name === "shared");
  expect(shared, "the first-boot group is live").toBeDefined();
  expect(groups.length, "a priced group beside the unpriced one").toBeGreaterThan(1);
  const rate = (groupId: string) =>
    stack.admin.post(`/admin/provider-groups/${groupId}/rates`, {
      model_id: pub.id,
      cny_per_usd: "7.2",
      effective_from: justNow(),
    });
  for (const g of groups.filter((g) => g.name !== "shared")) await rate(g.id);

  // One group that serves it without a rate: a call in it cannot be
  // charged, so the model is not priced, and the cell names that group.
  const panel = await openPublicModels(page);
  const cell = listRow(panel, name).getByRole("cell").nth(2);
  await expect(cell).toContainText("未定价");
  await expect(cell).toContainText(`${shared.display_name} 未设换算率`);
  await expect(cell).not.toContainText("已定价");

  // With `shared` rated too, every group prices it.
  await rate(shared.id);
  await panel.getByRole("button", { name: "刷新" }).click();
  await expect(listRow(panel, name).getByRole("cell").nth(2)).toHaveText("已定价");
});

test("两个账号都还在服务的公共模型可以一次删除：映射与两张文档全都没了", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const name = `pub-served-${suffix}`;
  const pub = await stack.admin.post("/admin/public-models", { name });
  const first = await newAccount(`acct-served-a-${suffix}`);
  const second = await newAccount(`acct-served-b-${suffix}`);
  for (const accountId of [first, second]) {
    await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${pub.id}`, { priority: 1, weight: 1 });
  }
  await expect
    .poll(async () => Object.keys(await modelDocs()), { timeout: APPLIED_WITHIN_MS })
    .toEqual(
      expect.arrayContaining([name, `${name}#acct-served-a-${suffix}`, `${name}#acct-served-b-${suffix}`]),
    );

  const panel = await openPublicModels(page);
  const row = listRow(panel, name);
  await row.getByRole("button", { name: "删除", exact: true }).click();
  // No grant holds it, so nothing refuses this: the warning says the
  // mappings go first, then the documents, and that it cannot be undone.
  const warning = row.getByRole("alert");
  await expect(warning).toContainText(`先删除 2 个账号对它的映射`);
  await expect(warning).toContainText("不能恢复");
  await row.getByRole("button", { name: "确认删除" }).click();

  await expect(page.getByText(`已删除「${name}」`)).toBeVisible();
  await expect(listRow(panel, name).getByRole("cell").nth(5)).toHaveText("已停用");
  const view = await stack.admin.get(`/admin/public-models/${pub.id}`);
  expect(view.retired_at).not.toBeNull();
  // Every mapping is gone from both accounts, and the documents with them.
  for (const accountId of [first, second]) {
    const table = await stack.admin.get(`/admin/provider-keys/${accountId}/channel-models`);
    expect(table.items.find((i: Json) => i.public_model_id === pub.id), "the mapping is gone").toBeUndefined();
  }
  // Every one of the three documents is gone, not just one of them: the
  // live names that are any of the three must be none at all.
  const generated = [name, `${name}#acct-served-a-${suffix}`, `${name}#acct-served-b-${suffix}`];
  await expect
    .poll(async () => Object.keys(await modelDocs()).filter((n) => generated.includes(n)), {
      timeout: APPLIED_WITHIN_MS,
    })
    .toEqual([]);
});

test("删除一个无人使用的公共模型也要先确认，并说明删除什么、不能恢复", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const name = `pub-unused-${suffix}`;
  const pub = await stack.admin.post("/admin/public-models", { name });
  // One account mapped it, and that row is off: nothing holds the model, so
  // the retire will go through.
  const accountId = await newAccount(`acct-unused-${suffix}`);
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${pub.id}`, { priority: 1, weight: 1 });
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${pub.id}`, { enabled: false });

  const panel = await openPublicModels(page);
  const row = listRow(panel, name);
  await row.getByRole("button", { name: "删除", exact: true }).click();
  // The first click only asks, and says what the delete removes for good.
  const warning = row.getByRole("alert");
  await expect(warning).toContainText("不能恢复");
  await expect(warning).toContainText("路由文档");
  await expect(warning).toContainText("直连文档");
  await expect(warning).toContainText(`名称「${name}」以后不能再用于公共模型`);
  await expect(row.getByRole("button", { name: "确认删除" })).toBeVisible();
  // Nothing has happened yet.
  expect((await stack.admin.get(`/admin/public-models/${pub.id}`)).retired_at).toBeNull();
  await expect(row.getByRole("cell").nth(5)).toHaveText("在用");

  // Taken back, still nothing; confirmed, it is retired.
  await row.getByRole("button", { name: "取消" }).click();
  expect((await stack.admin.get(`/admin/public-models/${pub.id}`)).retired_at).toBeNull();
  await row.getByRole("button", { name: "删除", exact: true }).click();
  await row.getByRole("button", { name: "确认删除" }).click();
  await expect(page.getByText(`已删除「${name}」`)).toBeVisible();
  await expect(listRow(panel, name).getByRole("cell").nth(5)).toHaveText("已停用");
  expect((await stack.admin.get(`/admin/public-models/${pub.id}`)).retired_at).not.toBeNull();
});

/** A baseline in force from just now, for `modelId`. */
async function baselineNow(modelId: string): Promise<void> {
  await stack.admin.post("/admin/pricing/baselines", {
    model_id: modelId,
    effective_from: new Date(Date.now() - 2_000).toISOString(),
    card: { default: { base: { input: "4", output: "20" } } },
    note: "the public-model list's card",
  });
}

test("定价只看服务它的分组：不服务它的分组没有换算率仍是已定价，缺的时候写明缺哪个分组或基准价", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const groups: Json[] = (await stack.admin.get("/admin/provider-groups")).items.filter((g: Json) => !g.retired_at);
  const shared = groups.find((g) => g.name === "shared");
  const rated = groups.find((g) => g.name === "grp-pub-8e3");
  expect(shared, "the first-boot group, which has no rate").toBeDefined();
  expect(rated, "the spec's group, created at a default rate").toBeDefined();
  expect(rated.default_rate.current?.cny_per_usd, "it prices every model by default").toBeTruthy();
  expect(shared.default_rate.current, "and `shared` prices none").toBeNull();

  // Served only from the rated group: `shared` serves nothing of it, so its
  // missing rate cannot leave a call unpriced.
  const onlyRated = await stack.admin.post("/admin/public-models", { name: `pub-g-rated-${suffix}` });
  const ratedAccount = await newAccount(`acct-g-rated-${suffix}`, ["grp-pub-8e3"]);
  await stack.admin.put(`/admin/provider-keys/${ratedAccount}/channel-models/${onlyRated.id}`, {});
  await baselineNow(onlyRated.id);
  // Served only from `shared`, which has no rate for it.
  const onlyShared = await stack.admin.post("/admin/public-models", { name: `pub-g-shared-${suffix}` });
  const sharedAccount = await newAccount(`acct-g-shared-${suffix}`, ["shared"]);
  await stack.admin.put(`/admin/provider-keys/${sharedAccount}/channel-models/${onlyShared.id}`, {});
  await baselineNow(onlyShared.id);
  // Served from the rated group, with no baseline at all.
  const noBaseline = await stack.admin.post("/admin/public-models", { name: `pub-g-bare-${suffix}` });
  await stack.admin.put(`/admin/provider-keys/${ratedAccount}/channel-models/${noBaseline.id}`, {});

  const panel = await openPublicModels(page);
  await expect(listRow(panel, onlyRated.name).getByRole("cell").nth(2)).toHaveText("已定价");
  const missing = listRow(panel, onlyShared.name).getByRole("cell").nth(2);
  await expect(missing).toContainText("未定价");
  await expect(missing).toContainText(`${shared.display_name} 未设换算率`);
  await expect(missing).not.toContainText(rated.display_name);
  const bare = listRow(panel, noBaseline.name).getByRole("cell").nth(2);
  await expect(bare).toContainText("未定价");
  await expect(bare).toContainText("无基准价");
});

test("授权租户与密钥数的是能调用它的租户和密钥：全部模型范围的租户也算", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const name = `pub-reach-${suffix}`;
  const pub = await stack.admin.post("/admin/public-models", { name });
  const accountId = await newAccount(`acct-reach-${suffix}`);
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${pub.id}`, {});
  // A tenant on every public model (the default scope), with a key that
  // picks no model of its own: it reaches this one without naming it.
  const tenant = await stack.admin.post("/admin/tenants", { name: `tenant-reach-${suffix}` });
  expect(tenant.model_scope).toBe("all");
  const shared = (await stack.admin.get("/admin/provider-groups")).items.find((g: Json) => g.name === "shared");
  await stack.admin.post(`/admin/tenants/${tenant.id}/keys`, { label: `key-reach-${suffix}`, group_id: shared.id });

  const panel = await openPublicModels(page);
  const row = listRow(panel, name);
  // The only all-scope tenant this spec creates, and its one key: every
  // other tenant here is narrowed to a list that does not name this model.
  await expect(row.getByRole("cell").nth(3)).toHaveText("1");
  await expect(row.getByRole("cell").nth(4)).toHaveText("1");
  // Nothing in a selected scope names it, so a delete is not refused: the
  // warning says what goes, not that it is held.
  await row.getByRole("button", { name: "删除", exact: true }).click();
  await expect(row.getByRole("alert")).not.toContainText("删除会被拒绝");
  await row.getByRole("button", { name: "取消" }).click();
});

test("已停用的公共模型可以恢复：恢复后在用、没有可用账号，再映射后重新可调用", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const name = `pub-restore-${suffix}`;
  const pub = await stack.admin.post("/admin/public-models", { name });
  const accountName = `acct-restore-${suffix}`;
  const accountId = await newAccount(accountName);
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${pub.id}`, { priority: 1, weight: 1 });
  await expect
    .poll(async () => (await modelDocs())[name] !== undefined, { timeout: APPLIED_WITHIN_MS })
    .toBe(true);

  const panel = await openPublicModels(page);
  const row = listRow(panel, name);
  await row.getByRole("button", { name: "删除", exact: true }).click();
  await row.getByRole("button", { name: "确认删除" }).click();
  await expect(page.getByText(`已删除「${name}」`)).toBeVisible();
  await expect(row.getByRole("cell").nth(5)).toHaveText("已停用");
  await expect(row.getByRole("button", { name: "删除", exact: true })).toHaveCount(0);
  await expect
    .poll(async () => (await modelDocs())[name] === undefined, { timeout: APPLIED_WITHIN_MS })
    .toBe(true);

  // Retired, 基准价 does not list it.
  const whileRetired = await stack.admin.get("/admin/pricing/baselines");
  expect((whileRetired.items as Json[]).some((i) => i.model_id === pub.id)).toBe(false);

  // 恢复 asks first, and says the mappings do not come back with it.
  await row.getByRole("button", { name: "恢复", exact: true }).click();
  await expect(row.getByRole("alert")).toContainText("原来的账号映射不会恢复");
  expect((await stack.admin.get(`/admin/public-models/${pub.id}`)).retired_at).not.toBeNull();
  await row.getByRole("button", { name: "确认恢复" }).click();
  await expect(page.getByText(`已恢复「${name}」`)).toBeVisible();
  await expect(row.getByRole("cell").nth(5)).toHaveText("在用");
  await expect(row.getByRole("cell").nth(1)).toHaveText("没有可用账号");
  await expect(row.getByRole("button", { name: "恢复", exact: true })).toHaveCount(0);
  const restored = await stack.admin.get(`/admin/public-models/${pub.id}`);
  expect(restored.retired_at).toBeNull();
  expect(restored.served_by).toEqual([]);
  // Its routing document is back, at its own id, with no target yet.
  await expect
    .poll(async () => (await modelDocs())[name]?.doc.routing.targets, { timeout: APPLIED_WITHIN_MS })
    .toEqual([]);
  expect((await modelDocs())[name].id).toBe(pub.id);
  // And it is a model 基准价 lists again, taking a card entered by hand (a
  // model the price library may not list): what 手动录入 sends.
  const listed = await stack.admin.get("/admin/pricing/baselines");
  expect((listed.items as Json[]).some((i) => i.model_id === pub.id)).toBe(true);
  const priced = await stack.admin.request("POST", "/admin/pricing/baselines", {
    model_id: pub.id,
    effective_from: new Date(Date.now() - 2_000).toISOString(),
    card: { default: { base: { input: "1.5", output: "6" } } },
    note: "restored model, priced by hand",
  });
  expect(priced.status, priced.text).toBe(201);

  // Mapped again the way 获取上游模型 → 写入映射 sends it (this stack's
  // control plane cannot read the stand-in's list, so the selection is posted
  // here): the restored public model is reused by name, under its own id.
  const applied = await stack.admin.post(`/admin/provider-keys/${accountId}/channel-models`, {
    upstream_models: [name],
  });
  expect((applied.items as Json[]).map((i) => [i.public_model_id, i.outcome])).toEqual([[pub.id, "added"]]);
  await panel.getByRole("button", { name: "刷新" }).click();
  await expect(row.getByRole("cell").nth(1)).toHaveText("1 个（优先级 0）");
  await expect
    .poll(async () => (await modelDocs())[`${name}#${accountName}`] !== undefined, { timeout: APPLIED_WITHIN_MS })
    .toBe(true);
  expect((await modelDocs())[name].doc.routing.targets).toHaveLength(1);
});
