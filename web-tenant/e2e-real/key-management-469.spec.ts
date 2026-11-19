import { expect, test, type Page, type Locator } from "@playwright/test";
import { mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

// 断言来自用户创建密钥时可设置额度的操作合同，使用真实隔离后端。
const session = resolve(process.env.KEY_MANAGEMENT_QA_SESSION!);
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
const world = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const origin = process.env.KEY_MANAGEMENT_TENANT_URL!;
const output = resolve(process.env.KEY_MANAGEMENT_QA_OUTPUT!);
if (world.ready !== true || world.realBackend !== true || world.mockUpstream !== false || !["127.0.0.1", "localhost"].includes(new URL(origin).hostname)) {
  throw new Error("Key management requires an explicit real isolated loopback world");
}
mkdirSync(output, { recursive: true });

interface CreatedKey { id: string; label: string; model_ids: string[] | null }
interface Budget { id: string; scope: string; target: { id: string }; set_by: string; kind: string; period: string | null; limit_micro_cny: number; spent_micro_cny: number; remaining_micro_cny: number }
async function jsonGet<T>(page: Page, path: string): Promise<T> {
  const answer = await page.evaluate(async target => {
    const response = await fetch(target, { credentials: "include" });
    return { status: response.status, body: await response.json() };
  }, path);
  expect(answer.status, path).toBe(200);
  return answer.body as T;
}
function writable(): void {
  if (process.env.KEY_MANAGEMENT_ALLOW_WRITES !== "isolated") throw new Error("The real creation journey requires an explicitly writable isolated world");
}
async function newKey(page: Page, label: string): Promise<Locator> {
  await page.getByRole("button", { name: "新建密钥", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "新建密钥", exact: true });
  await dialog.getByLabel("名称", { exact: true }).fill(label);
  await dialog.getByRole("group", { name: "分组", exact: true }).getByRole("radio").first().check();
  return dialog;
}
async function submit(page: Page, dialog: Locator): Promise<{ key: CreatedKey; body: Record<string, unknown> }> {
  const pending = page.waitForResponse(response => new URL(response.url()).pathname === "/api/keys" && response.request().method() === "POST");
  await dialog.getByRole("button", { name: "新建密钥", exact: true }).click();
  const answer = await pending;
  expect(answer.status()).toBe(201);
  const body = answer.request().postDataJSON() as Record<string, unknown>;
  const result = await answer.json() as { key: CreatedKey };
  await expect(page.getByRole("dialog", { name: "密钥已创建", exact: true })).toBeVisible();
  return { key: result.key, body };
}

async function login(page: Page): Promise<void> {
  const owner = auth.metricsOwner ?? auth.owner;
  await page.goto(`${origin.replace(/\/$/, "")}/login`);
  await page.getByLabel("邮箱", { exact: true }).fill(owner.email);
  await page.getByLabel("口令", { exact: true }).fill(owner.password);
  const answer = page.waitForResponse(response => new URL(response.url()).pathname === "/api/login" && response.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await answer).status()).toBe(200);
  await page.goto(origin + "/#keys");
  await expect(page.getByRole("heading", { name: "API 密钥", exact: true })).toBeVisible();
}

test("tenant key creation exposes an optional exact spending quota", async ({ page }) => {
  await login(page);
  const writes: string[] = [];
  page.on("request", request => { if (!["GET", "HEAD", "OPTIONS"].includes(request.method())) writes.push(request.method() + " " + new URL(request.url()).pathname); });
  await page.getByRole("button", { name: "新建密钥", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "新建密钥", exact: true });
  await expect(dialog.getByLabel("费用额度（元）", { exact: true })).toBeVisible();
  await expect(dialog.getByLabel("费用额度（元）", { exact: true })).toHaveValue("");
  await expect(dialog.getByText("留空表示不限制", { exact: false })).toBeVisible();
  await dialog.getByRole("button", { name: "取消", exact: true }).click();
  expect(writes).toEqual([]);
});

for (const scenario of [{ width: 1440, cycle: "累计", kind: "cumulative", period: null }, { width: 390, cycle: "每月", kind: "periodic", period: "month" }]) {
  test(`tenant creates a key with a precise enforced quota ${scenario.width}`, async ({ page }) => {
    writable();
    await page.setViewportSize({ width: scenario.width, height: 1000 });
    await login(page);
    const catalog = await jsonGet<{ items: { id: string; name: string }[] }>(page, "/api/models");
    expect(catalog.items.length, "real authorized models exist").toBeGreaterThan(0);
    const model = catalog.items[0]!;
    const label = `tenant-key-469-${scenario.width}-${Date.now()}`;
    const dialog = await newKey(page, label);
    await dialog.getByRole("radio", { name: "指定模型", exact: true }).check();
    await dialog.getByLabel("搜索模型", { exact: true }).fill(model.name);
    await dialog.getByRole("checkbox", { name: model.name, exact: true }).check();
    await dialog.getByLabel("费用额度（元）", { exact: true }).fill("12.34");
    await dialog.getByRole("combobox", { name: "额度周期", exact: true }).click();
    await page.getByRole("option", { name: scenario.cycle, exact: true }).click();
    const result = await submit(page, dialog);
    expect(result.body.budget).toEqual({ kind: scenario.kind, ...(scenario.period === null ? {} : { period: scenario.period }), limit_cny: "12.34" });
    expect(result.key.model_ids).toEqual([model.id]);
    const budgets = (await jsonGet<{ items: Budget[] }>(page, "/api/budgets")).items.filter(row => row.scope === "api_key" && row.target.id === result.key.id);
    expect(budgets).toHaveLength(1);
    const budget = budgets[0]!;
    for (const field of ["limit_micro_cny", "spent_micro_cny", "remaining_micro_cny"]) expect(Object.hasOwn(budget, field), `explicit ${field}`).toBe(true);
    expect(budget).toMatchObject({ set_by: "org_admin", kind: scenario.kind, period: scenario.period, limit_micro_cny: 12_340_000, spent_micro_cny: 0, remaining_micro_cny: 12_340_000 });
    const quota = await jsonGet<{ key_id: string; quotas: Budget[] }>(page, `/api/keys/${result.key.id}/quota`);
    expect(quota.key_id).toBe(result.key.id);
    expect(quota.quotas.find(row => row.id === budget.id)).toMatchObject({ scope: "api_key", limit_micro_cny: 12_340_000 });
    await page.reload();
    await page.getByLabel("搜索密钥", { exact: true }).fill(label);
    await page.getByRole("link", { name: label, exact: true }).click();
    await expect(page.getByRole("heading", { name: label, exact: true })).toBeVisible();
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
    await page.screenshot({ path: join(output, `tenant-created-key-${scenario.width}.png`) });
  });
}

test("a zero quota is refused before minting and a blank quota stays unlimited", async ({ page }) => {
  writable();
  await login(page);
  const label = `tenant-unlimited-469-${Date.now()}`;
  const dialog = await newKey(page, label);
  const posts: string[] = [];
  page.on("request", request => { if (request.method() === "POST" && new URL(request.url()).pathname === "/api/keys") posts.push(request.postData() ?? ""); });
  const amount = dialog.getByLabel("费用额度（元）", { exact: true });
  await amount.fill("0");
  await dialog.getByRole("button", { name: "新建密钥", exact: true }).click();
  await expect(amount).toHaveAttribute("aria-invalid", "true");
  await expect(amount).toBeFocused();
  expect(posts).toEqual([]);
  expect((await jsonGet<{ items: CreatedKey[] }>(page, "/api/keys")).items.some(key => key.label === label)).toBe(false);
  await amount.fill("");
  const created = await submit(page, dialog);
  expect(Object.hasOwn(created.body, "budget")).toBe(false);
  const rows = (await jsonGet<{ items: Budget[] }>(page, "/api/budgets")).items.filter(row => row.scope === "api_key" && row.target.id === created.key.id);
  expect(rows).toEqual([]);
});

test("现有密钥可设置、调整和移除费用额度，并恢复无独立并发和 RPM 限制", async ({ page }) => {
  writable();
  await login(page);
  const dialog = await newKey(page, `费用和速率 ${crypto.randomUUID()}`);
  const { key } = await submit(page, dialog);
  const path = `/api/keys/${key.id}`;
  const openEdit = async () => {
    await page.goto(`${origin}/#keys?key=${key.id}&edit=1`);
    await expect(page.getByRole("heading", { name: "修改密钥", exact: true })).toBeVisible();
  };
  await openEdit();
  const editor = page.getByRole("region", { name: "密钥费用额度", exact: true });
  await expect(editor.getByText("未设置密钥费用额度", { exact: true })).toBeVisible();
  const readCaps = async () => (await jsonGet<{ quotas: Budget[] }>(page, path + "/quota")).quotas.filter(row => row.scope === "api_key");
  expect(await readCaps()).toEqual([]);
  await editor.getByRole("button", { name: "设置密钥额度", exact: true }).click();
  const form = page.getByRole("dialog");
  await form.getByLabel("额度（元）", { exact: true }).fill("12.34");
  await form.getByRole("button", { name: "保存额度", exact: true }).click();
  await expect(editor.getByText(/¥12\.34/)).toBeVisible();
  const [first] = await readCaps();
  expect(first).toMatchObject({ limit_micro_cny: 12_340_000 });
  await editor.getByRole("button", { name: "调整额度", exact: true }).click();
  await form.getByLabel("额度（元）", { exact: true }).fill("23.45");
  await form.getByRole("button", { name: "保存额度", exact: true }).click();
  await expect(editor.getByText(/¥23\.45/)).toBeVisible();
  expect((await readCaps()).map(row => [row.id, row.limit_micro_cny])).toEqual([[first!.id, 23_450_000]]);
  await page.reload();
  await editor.getByRole("button", { name: "移除额度", exact: true }).click();
  await editor.getByRole("button", { name: "取消", exact: true }).click();
  expect(await readCaps()).toHaveLength(1);
  await editor.getByRole("button", { name: "移除额度", exact: true }).click();
  await editor.getByRole("button", { name: "确认移除额度", exact: true }).click();
  await expect(editor.getByText("未设置密钥费用额度", { exact: true })).toBeVisible();
  expect(await readCaps()).toEqual([]);
  for (const limits of [{ concurrency: 2, rpm: 15 }, {}] as const) {
    await openEdit();
    await page.getByLabel("并发", { exact: true }).fill("concurrency" in limits ? String(limits.concurrency) : "");
    await page.getByLabel("每分钟请求", { exact: true }).fill("rpm" in limits ? String(limits.rpm) : "");
    const saved = page.waitForResponse(response => new URL(response.url()).pathname === path && response.request().method() === "PATCH");
    await page.getByRole("button", { name: "保存修改", exact: true }).click();
    expect((await saved).status()).toBe(200);
    expect((await jsonGet<{ rate_limit: object }>(page, path)).rate_limit).toEqual(limits);
  }
});

test("直接展开的模型卡片可通配选择和取消，保存当前指定集合", async ({ page }) => {
  writable();
  await login(page);
  const dialog = await newKey(page, `指定模型 ${crypto.randomUUID()}`);
  await dialog.getByRole("radio", { name: "指定模型", exact: true }).check();
  const options = dialog.getByRole("checkbox");
  await expect(options.first()).toBeVisible();
  const models = (await jsonGet<{ items: { id: string; name: string }[] }>(page, "/api/models")).items;
  // 以当前可选项为准；该分组中未开放的组织模型不能进入匹配结果。
  const available = [];
  for (const model of models) {
    if (await dialog.getByRole("checkbox", { name: model.name, exact: true }).count()) available.push(model);
  }
  expect(available.length, "隔离分组至少两个已授权模型").toBeGreaterThan(1);
  const search = dialog.getByLabel("搜索模型", { exact: true });
  const longest = available.reduce((a, b) => a.name.length >= b.name.length ? a : b).name;
  await search.fill(longest + "*");
  await expect(options).toHaveCount(available.filter(model => model.name.toLowerCase().startsWith(longest.toLowerCase())).length);
  await expect(dialog.getByRole("checkbox", { name: longest, exact: true })).toBeVisible();
  await search.fill("*");
  await dialog.getByRole("button", { name: "选择全部匹配模型", exact: true }).click();
  for (const model of available) await expect(dialog.getByRole("checkbox", { name: model.name, exact: true })).toBeChecked();
  await dialog.getByRole("checkbox", { name: available[0]!.name, exact: true }).uncheck();
  const { key } = await submit(page, dialog);
  const selected = available.slice(1).map(model => model.id).sort();
  expect(key.model_ids?.slice().sort()).toEqual(selected);
  const stored = await jsonGet<CreatedKey>(page, `/api/keys/${key.id}`);
  expect(stored.model_ids?.slice().sort()).toEqual(selected);
});
