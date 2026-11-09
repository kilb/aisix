import { randomUUID } from "node:crypto";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "./stack";

/**
 * 价格 · 分组 against the real control plane through the built `dist/`.
 *
 * Every figure asserted here comes from the baseline and the rate the test
 * itself configured, never from the product's own answers: a group at a rate
 * of 7.2 over a baseline of $4/M input and $20/M output is ¥28.80 and ¥144.00
 * per million, and an override of 6 makes it ¥24.00 and ¥120.00, written as
 * every ¥/M figure in the Dashboard is: at least two decimals, with thousands
 * separators. The realised rate is
 * the one case a figure is read back, because it is a measurement of what was
 * charged rather than a price: a cost price equal to the baseline leaves
 * exactly the configured rate, 7.2000.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

const PASSWORD = "groups-pass-5c1x";

/**
 * A group's rate over a baseline rate, both per million, as `¥`-prefixed
 * text: exact, never a float, with at least two decimals and thousands
 * separators.
 */
function cnyPerMillion(usd: string, cny: string): string {
  const micros = BigInt(Math.round(Number(usd) * 1_000_000));
  const e4 = BigInt(Math.round(Number(cny) * 10_000));
  const scale = 10n ** 10n;
  const value = micros * e4;
  const whole = (value / scale).toString().replace(/\B(?=(\d{3})+$)/g, ",");
  const frac = (value % scale).toString().padStart(10, "0").replace(/0+$/, "").padEnd(2, "0");
  return `¥${whole}.${frac}`;
}

async function login(page: Page, who = stack.operator): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(who.email);
  await page.getByLabel("口令", { exact: true }).fill(who.password);
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
}

/** Opens 价格 · 分组 and waits until the list under it is read. */
async function openGroups(page: Page): Promise<void> {
  await login(page);
  await page.locator('[data-tab="pricing"]').click();
  await page.getByRole("group", { name: "价格分区" }).getByRole("button", { name: "分组", exact: true }).click();
  // The page's own heading also names its 刷新 button, and the create form's
  // heading is a 分组 one too: match the page's, by level.
  await expect(page.getByRole("heading", { name: "分组", level: 2 })).toBeVisible();
  await expect(page.locator("main").getByText("载入中…")).toHaveCount(0);
}

/** The list's row for the group shown as `display`. */
function row(page: Page, display: string): Locator {
  return page
    .getByRole("table", { name: "分组" })
    .getByRole("row")
    .filter({ has: page.getByRole("button", { name: display, exact: true }) });
}

/** Opens a group's detail, and answers the region that holds it. */
async function openDetail(page: Page, display: string): Promise<Locator> {
  await row(page, display).getByRole("button", { name: display, exact: true }).click();
  const detail = page.getByRole("region", { name: `分组 ${display}` });
  await expect(detail).toBeVisible();
  await expect(page.locator("main").getByText("载入中…")).toHaveCount(0);
  return detail;
}

/** The detail table's row for `model`. */
function modelRow(page: Page, model: string): Locator {
  return page.getByRole("table", { name: "分组换算率" }).getByRole("row").filter({ hasText: model });
}

/** A live group at `rate`, named so two tests never collide. */
async function newGroup(rate = "7.2"): Promise<Json> {
  const suffix = randomUUID().slice(0, 8);
  return stack.admin.createGroup(`grp-gp-${suffix}`, `分组 ${suffix}`, rate);
}

/** An upstream account that serves only `group`. */
async function newAccount(group: Json): Promise<string> {
  const suffix = randomUUID().slice(0, 8);
  const key = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: `acct-gp-${suffix}`,
      provider: "openai",
      api_key: `sk-acct-gp-${suffix}-upstream`,
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups: [group.name],
    },
  });
  return key.id as string;
}

let modelSeq = 0;

/** A live model served by `account`, named so no two rows are ambiguous. */
async function newModel(prefix: string, accountId: string): Promise<{ id: string; name: string; upstream: string }> {
  const name = `${prefix}-${randomUUID().slice(0, 8)}`;
  const upstream = `upstream-${name}-${++modelSeq}`;
  const model = await stack.admin.post("/admin/resources/models", {
    doc: { display_name: name, provider: "openai", model_name: upstream, provider_key_id: accountId },
  });
  return { id: model.id as string, name, upstream };
}

/** A model's official USD list price from now. */
async function addBaseline(modelId: string, input: string, output: string): Promise<void> {
  await stack.admin.post("/admin/pricing/baselines", {
    model_id: modelId,
    effective_from: new Date().toISOString(),
    card: { default: { base: { input, output } } },
    note: "分组页用例的官方价",
  });
}

test("新建分组必须填写默认换算率；列表显示显示名、路由名与计数", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const routing = `grp-new-${suffix}`;
  const display = `新分组 ${suffix}`;
  await openGroups(page);

  const form = page.getByRole("group", { name: "新建分组" });
  // The operator-only warning about what tenants read.
  await expect(form).toContainText("显示名和说明对租户可见，不要写入上游渠道名称");
  await form.getByLabel("路由名").fill(routing);
  await form.getByLabel("显示名").fill(display);
  await form.getByLabel("说明", { exact: true }).fill("分组页用例");

  // No rate: refused here, and nothing was sent.
  await form.getByRole("button", { name: "新建分组" }).click();
  await expect(form.getByText("请填写默认换算率")).toBeVisible();
  const before = (await stack.admin.get("/admin/provider-groups")) as Json;
  expect(before.items.some((g: Json) => g.name === routing)).toBe(false);

  // A name the routing pattern refuses is refused at its own field.
  await form.getByLabel("默认换算率").fill("7.2");
  await form.getByLabel("路由名").fill("Bad Name");
  await form.getByRole("button", { name: "新建分组" }).click();
  await expect(form.getByText(/分组名称须为/)).toBeVisible();

  await form.getByLabel("路由名").fill(routing);
  await form.getByRole("button", { name: "新建分组" }).click();
  await expect(page.getByText("已新建")).toBeVisible();

  const r = row(page, display);
  await expect(r).toBeVisible();
  // The routing name is the operator's alone, in small mono text.
  await expect(r.locator(".mono")).toHaveText(routing);
  await expect(r).toContainText("分组页用例");
  await expect(r.locator("td").nth(3)).toContainText("7.2");
  // Nobody uses it yet: tenants, keys, upstream accounts.
  for (const col of [4, 5, 6]) await expect(r.locator("td").nth(col)).toHaveText("0");

  // The display name and the description are the two things an operator edits.
  await r.getByRole("button", { name: "编辑", exact: true }).click();
  const edit = page.getByRole("group", { name: "编辑分组" });
  // A wrapping label's name takes in the text it holds, so the prefilled
  // description is matched loosely.
  await edit.getByLabel("说明").fill("改过的说明");
  await edit.getByRole("button", { name: "保存", exact: true }).click();
  await expect(page.getByText("已保存")).toBeVisible();
  await expect(row(page, display)).toContainText("改过的说明");
});

test("为模型设置覆盖换算率后显示 ¥/百万 tokens，结束覆盖后回到默认", async ({ page }) => {
  const group = await newGroup("7.2");
  const account = await newAccount(group);
  const model = await newModel("gp-rate", account);
  await addBaseline(model.id, "4", "20");
  await openGroups(page);
  await openDetail(page, group.display_name as string);

  // The group's default prices the row until an override says otherwise.
  const priced = modelRow(page, model.name);
  await expect(priced.getByRole("cell").nth(4)).toHaveText(/默认\s*7\.2/);
  await expect(priced.getByRole("cell").nth(5)).toHaveText(cnyPerMillion("4", "7.2"));
  await expect(priced.getByRole("cell").nth(6)).toHaveText(cnyPerMillion("20", "7.2"));

  await priced.getByRole("button", { name: "设置覆盖", exact: true }).click();
  const set = page.getByRole("group", { name: "设置覆盖" });
  await expect(set).toContainText(model.name);
  await set.getByLabel("换算率").fill("6");
  await set.getByRole("button", { name: "保存覆盖" }).click();
  await expect(page.getByText("已保存")).toBeVisible();

  const overridden = modelRow(page, model.name);
  await expect(overridden.getByRole("cell").nth(4)).toHaveText(/覆盖\s*6/);
  await expect(overridden.getByRole("cell").nth(5)).toHaveText(cnyPerMillion("4", "6"));
  await expect(overridden.getByRole("cell").nth(6)).toHaveText(cnyPerMillion("20", "6"));

  // Ending the override gives the row back to the group's default. Only a
  // rate that buys nothing is confirmed first, so this one acts at once.
  await overridden.getByRole("button", { name: "结束覆盖", exact: true }).click();
  const end = page.getByRole("group", { name: "结束覆盖" });
  await end.getByRole("button", { name: "结束覆盖", exact: true }).click();
  await expect(page.getByText("已保存")).toBeVisible();

  const back = modelRow(page, model.name);
  await expect(back.getByRole("cell").nth(4)).toHaveText(/默认\s*7\.2/);
  await expect(back.getByRole("cell").nth(5)).toHaveText(cnyPerMillion("4", "7.2"));
  await expect(back.getByRole("cell").nth(6)).toHaveText(cnyPerMillion("20", "7.2"));
  await expect(back.getByRole("button", { name: "结束覆盖", exact: true })).toHaveCount(0);
});

test("换算率 0 需要二次确认", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const routing = `grp-free-${suffix}`;
  const display = `免费分组 ${suffix}`;
  await openGroups(page);

  const form = page.getByRole("group", { name: "新建分组" });
  await form.getByLabel("路由名").fill(routing);
  await form.getByLabel("显示名").fill(display);
  await form.getByLabel("默认换算率").fill("0");
  await form.getByRole("button", { name: "新建分组" }).click();

  // A free group is almost never meant: the first click only asks.
  await expect(form.getByRole("button", { name: "确认新建分组" })).toBeVisible();
  await form.getByRole("button", { name: "取消" }).click();
  await expect(form.getByRole("button", { name: "新建分组" })).toBeVisible();
  const listed = (await stack.admin.get("/admin/provider-groups")) as Json;
  expect(listed.items.some((g: Json) => g.name === routing)).toBe(false);

  await form.getByRole("button", { name: "新建分组" }).click();
  await form.getByRole("button", { name: "确认新建分组" }).click();
  await expect(page.getByText("已新建")).toBeVisible();
  await expect(row(page, display).locator("td").nth(3)).toContainText("0");

  // The same ask on a model's override, and no ask at a rate that bills.
  // The group was made through the API, after this page read its list.
  const group = await newGroup("7.2");
  const account = await newAccount(group);
  const model = await newModel("gp-zero", account);
  await addBaseline(model.id, "4", "20");
  // One tenant on the group, with one key in it: what a free rate would reach.
  const suffix2 = randomUUID().slice(0, 8);
  const owner = { email: `owner-zero-${suffix2}@groups.test`, password: PASSWORD };
  const signedUp = await stack.tenant().post("/api/signup", { org_name: `免费确认租户 ${suffix2}`, ...owner });
  await stack.admin.post(`/admin/tenants/${signedUp.tenant.id}/approve`);
  const current = await stack.admin.get(`/admin/tenants/${signedUp.tenant.id}`);
  await stack.admin.patch(`/admin/tenants/${signedUp.tenant.id}`, { allowed_provider_groups: [group.name] }, current.version);
  const api = stack.tenant();
  await api.post("/api/login", owner);
  await api.post("/api/keys", { label: "免费确认用例的密钥" });
  await page.getByRole("button", { name: "刷新", exact: true }).click();
  await openDetail(page, group.display_name as string);

  // The group's default at 0: asked, naming how much it reaches.
  const defaults = page.getByRole("group", { name: "修改默认换算率" });
  await defaults.getByLabel("新默认换算率").fill("0");
  await defaults.getByRole("button", { name: "保存默认换算率" }).click();
  await expect(defaults.getByRole("alert")).toContainText("影响 1 个租户、1 个密钥");
  await defaults.getByRole("button", { name: "取消" }).click();

  const priced = modelRow(page, model.name);
  await priced.getByRole("button", { name: "设置覆盖", exact: true }).click();
  const set = page.getByRole("group", { name: "设置覆盖" });
  // The field says what an empty one does not do, and where ending is.
  await expect(set).toContainText("要回到分组默认，请用「结束覆盖」");
  await set.getByLabel("换算率").fill("0");
  await set.getByRole("button", { name: "保存覆盖" }).click();
  await expect(set.getByRole("button", { name: "确认保存覆盖" })).toBeVisible();
  await expect(set.getByRole("alert")).toContainText(`「${model.name}」在这个分组下不计价`);
  await expect(set.getByRole("alert")).toContainText("影响这个分组的 1 个租户、1 个密钥");
  await set.getByRole("button", { name: "取消" }).click();
  await expect(modelRow(page, model.name).getByRole("cell").nth(4)).toHaveText(/默认\s*7\.2/);

  await set.getByLabel("换算率").fill("3");
  await set.getByRole("button", { name: "保存覆盖" }).click();
  await expect(page.getByText("已保存")).toBeVisible();
  await expect(modelRow(page, model.name).getByRole("cell").nth(4)).toHaveText(/覆盖\s*3/);
});

test("停用仍被使用的分组时显示各项计数", async ({ page }) => {
  const group = await newGroup("7.2");
  await newAccount(group);
  // A tenant on that group, with one key in it.
  const suffix = randomUUID().slice(0, 8);
  const owner = { email: `owner-grp-${suffix}@groups.test`, password: PASSWORD };
  const signedUp = await stack.tenant().post("/api/signup", {
    org_name: `分组租户 ${suffix}`,
    email: owner.email,
    password: owner.password,
  });
  const tenantId = signedUp.tenant.id as string;
  await stack.admin.post(`/admin/tenants/${tenantId}/approve`);
  const current = await stack.admin.get(`/admin/tenants/${tenantId}`);
  await stack.admin.patch(`/admin/tenants/${tenantId}`, { allowed_provider_groups: [group.name] }, current.version);
  const api = stack.tenant();
  await api.post("/api/login", owner);
  await api.post("/api/keys", { label: "分组页用例的密钥" });

  await openGroups(page);
  const r = row(page, group.display_name as string);
  await expect(r.locator("td").nth(4)).toHaveText("1");
  await expect(r.locator("td").nth(5)).toHaveText("1");
  await expect(r.locator("td").nth(6)).toHaveText("1");

  await r.getByRole("button", { name: "停用", exact: true }).click();
  await r.getByRole("button", { name: "确认停用" }).click();

  // Every count that still names it, by name: a generic sentence says nothing.
  const refusal = page.getByRole("alert").filter({ hasText: "无法停用" });
  await expect(refusal).toContainText("租户 1");
  await expect(refusal).toContainText("默认设置 0");
  await expect(refusal).toContainText("密钥 1");
  await expect(refusal).toContainText("上游账号 1");
  await expect(refusal).toContainText("默认分组 0");

  // Nothing was retired.
  const listed = (await stack.admin.get("/admin/provider-groups")) as Json;
  expect(listed.items.some((g: Json) => g.name === group.name)).toBe(true);
});

test("未定价的模型排在最前并标注未定价", async ({ page }) => {
  const group = await newGroup("7.2");
  const account = await newAccount(group);
  // Names chosen so the alphabet says the opposite of the order under test.
  const unpriced = await newModel("z9-gp", account);
  const pricedModel = await newModel("a0-gp", account);
  await addBaseline(pricedModel.id, "2", "8");
  await openGroups(page);
  await openDetail(page, group.display_name as string);

  const table = page.getByRole("table", { name: "分组换算率" });
  const states = await table
    .locator("tbody tr")
    .evaluateAll((trs) => trs.map((tr) => tr.getAttribute("data-state")));
  expect(states).toContain("unpriced");
  expect(states).toContain("priced");
  // Every unpriced model before every priced one.
  expect(states).toEqual([...states].sort((a, b) => (a === b ? 0 : a === "unpriced" ? -1 : 1)));

  const first = modelRow(page, unpriced.name);
  await expect(first).toHaveAttribute("data-state", "unpriced");
  await expect(first).toContainText("未定价");
  // No baseline means no ¥ figure, whatever the group's rate is.
  await expect(first.getByRole("cell").nth(5)).toHaveText("—");
  await expect(modelRow(page, pricedModel.name)).toHaveAttribute("data-state", "priced");
});

test("实际换算率显示近 30 天的结果", async ({ page }) => {
  const group = await newGroup("7.2");
  const account = await newAccount(group);
  const model = await newModel("gp-real", account);
  // A second model with the same list price and no cost price: its charge
  // counts in no ratio, and the page must say how much that leaves out.
  const uncosted = await newModel("gp-nocost", account);
  // One card, twice: the official list price and what the upstream costs. A
  // charge over a cost with the same card realises exactly the configured rate.
  const card = { default: { base: { input: "4", output: "20" } } };
  await stack.admin.post("/admin/pricing/baselines", {
    model_id: model.id,
    effective_from: new Date().toISOString(),
    card,
    note: "分组页用例的官方价",
  });
  await stack.admin.post("/admin/pricing/cost", {
    upstream_provider: "openai",
    upstream_model: model.upstream,
    effective_from: new Date().toISOString(),
    card,
  });
  await stack.admin.post("/admin/pricing/baselines", {
    model_id: uncosted.id,
    effective_from: new Date().toISOString(),
    card,
    note: "分组页用例的官方价",
  });

  // A funded tenant on that group, whose one call costs the mock upstream's
  // 5 prompt and 3 completion tokens: $0.08, charged at ¥0.576.
  const suffix = randomUUID().slice(0, 8);
  const owner = { email: `owner-real-${suffix}@groups.test`, password: PASSWORD };
  const signedUp = await stack.tenant().post("/api/signup", {
    org_name: `实际换算率租户 ${suffix}`,
    email: owner.email,
    password: owner.password,
  });
  const tenantId = signedUp.tenant.id as string;
  await stack.admin.post(`/admin/tenants/${tenantId}/approve`);
  const current = await stack.admin.get(`/admin/tenants/${tenantId}`);
  await stack.admin.patch(
    `/admin/tenants/${tenantId}`,
    { allowed_provider_groups: [group.name], allowed_model_ids: [model.id, uncosted.id] },
    current.version,
  );
  await stack.admin.post(`/admin/tenants/${tenantId}/credits`, { amount_cny: "10.00", reason: "分组页用例" });
  const api = stack.tenant();
  await api.post("/api/login", owner);
  const created = await api.post("/api/keys", { label: "实际换算率用例" });
  const plaintext = created.plaintext as string;

  // The key reaches the gateway, then one call goes through it.
  const deadline = Date.now() + 60_000;
  for (;;) {
    const res = await fetch(`${stack.proxyUrl}/v1/models`, { headers: { authorization: `Bearer ${plaintext}` } });
    await res.arrayBuffer();
    if (res.status === 200) break;
    if (Date.now() >= deadline) throw new Error(`the gateway still answers ${res.status} to the new key`);
    await new Promise((r) => setTimeout(r, 250));
  }
  for (const name of [model.name, uncosted.name]) {
    const chat = await fetch(`${stack.proxyUrl}/v1/chat/completions`, {
      method: "POST",
      headers: { authorization: `Bearer ${plaintext}`, "content-type": "application/json" },
      body: JSON.stringify({ model: name, messages: [{ role: "user", content: "hi" }] }),
    });
    await chat.arrayBuffer();
    expect(chat.status).toBe(200);
  }

  // The charge lands asynchronously; the page reads the margin route, so wait
  // for the row there rather than for a timer.
  const now = new Date();
  const from = new Date(now.getTime() - 30 * 86_400_000);
  const query = `from=${from.toISOString()}&to=${now.toISOString()}&group_id=${group.id}`;
  await expect
    .poll(
      async () => {
        const margin = (await stack.admin.get(`/admin/pricing/margin?${query}`)) as Json;
        return margin.items.find((i: Json) => i.model.id === model.id)?.realised_cny_per_usd ?? null;
      },
      { timeout: 60_000 },
    )
    .toBe("7.2000");
  await expect
    .poll(
      async () => {
        const margin = (await stack.admin.get(`/admin/pricing/margin?${query}`)) as Json;
        return margin.items.find((i: Json) => i.model.id === uncosted.id)?.rows_without_cost ?? 0;
      },
      { timeout: 60_000 },
    )
    .toBe(1);

  await openGroups(page);
  await openDetail(page, group.display_name as string);
  const r = modelRow(page, model.name);
  await expect(r.getByRole("cell").nth(7)).toHaveText("7.2000");
  // The same call with no cost: no ratio, and its ¥0.000576 named beside the dash.
  const without = modelRow(page, uncosted.name).getByRole("cell").nth(7);
  await expect(without).toContainText("—");
  await expect(without).toContainText("另有 1 笔无成本，计 ¥0.000576");
});

test("默认分组有标记，可改为另一个分组；停用默认分组时显示原因", async ({ page }) => {
  await openGroups(page);
  // A fresh database designates `default` / 默认, with no upstream account.
  const designated = row(page, "默认");
  await expect(designated).toContainText("默认分组");
  await expect(designated.locator("td").nth(6)).toHaveText("0");

  const suffix = randomUUID().slice(0, 8);
  const group = await stack.admin.createGroup(`grp-dflt-${suffix}`, `待定默认 ${suffix}`, "7.2");
  await page.getByRole("button", { name: "刷新", exact: true }).click();
  const r = row(page, group.display_name as string);
  await expect(r).toContainText("设为默认分组");

  await r.getByRole("button", { name: "设为默认分组", exact: true }).click();
  await r.getByRole("button", { name: "确认设为默认分组" }).click();
  // The tag, not the words: "设为默认分组" on another row contains them too.
  await expect(row(page, group.display_name as string).locator("[data-default]")).toHaveText("默认分组");
  await expect(row(page, "默认").locator("[data-default]")).toHaveCount(0);

  // A designated group cannot be retired, and the refusal says why.
  const now = row(page, group.display_name as string);
  await now.getByRole("button", { name: "停用", exact: true }).click();
  await now.getByRole("button", { name: "确认停用" }).click();
  const refusal = page.getByRole("alert").filter({ hasText: "无法停用" });
  await expect(refusal).toContainText("默认分组 1");
});
