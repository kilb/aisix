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

test("停用一个在用的公共模型需要确认并显示租户与密钥数量", async ({ page }) => {
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

  const panel = await openPublicModels(page);
  const row = listRow(panel, `pub-retire-${suffix}`);
  await row.getByRole("button", { name: "停用", exact: true }).click();
  // Asked once: the warning states the counts and what a retire would need
  // cleared first.
  await expect(row.getByRole("alert")).toContainText("1 个租户");
  await expect(row.getByRole("alert")).toContainText("1 个密钥");
  await row.getByRole("button", { name: "确认停用" }).click();

  // Refused with the counts, and the model is untouched.
  await expect(panel.getByRole("alert").filter({ hasText: "无法停用" })).toContainText("租户 1");
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
