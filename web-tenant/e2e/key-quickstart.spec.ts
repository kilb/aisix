import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

const GROUP = "quickstart-e2e";
const OTHER_GROUP = "quickstart-e2e-other";
const CHOSEN_MODEL = "first-call-chat";
const INHERITED_MODEL = "first-call-second";
const OTHER_GROUP_MODEL = "first-call-other-group";
const UNGRANTED_MODEL = "first-call-outside-grant";
const PASSWORD = "quickstart-e2e-portal-pass-11x";

let stack: Stack;
let grantedModels: string[];

test.use({ trace: "off" });

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
  await stack.admin.seedGroups([GROUP, OTHER_GROUP]);

  async function provider(group: string): Promise<string> {
    const account = await stack.admin.post("/admin/resources/provider_keys", { doc: {
      display_name: group,
      provider: "openai",
      api_key: "local-e2e-test-account",
      api_base: "https://api.example.test/v1",
      groups: [group],
    } });
    return account.id;
  }

  async function direct(name: string, account: string): Promise<string> {
    const model = await stack.admin.post("/admin/resources/models", { doc: {
      display_name: name,
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: account,
    } });
    return model.id;
  }

  const [account, otherAccount] = await Promise.all([provider(GROUP), provider(OTHER_GROUP)]);
  const publicModel = await stack.admin.post("/admin/public-models", { name: INHERITED_MODEL });
  await stack.admin.put(`/admin/provider-keys/${account}/channel-models/${publicModel.id}`, { upstream_model: "gpt-4o-mini" });
  grantedModels = await Promise.all([
    direct(CHOSEN_MODEL, account),
    direct(OTHER_GROUP_MODEL, otherAccount),
  ]);
  grantedModels.push(publicModel.id);
  await direct(UNGRANTED_MODEL, account);
});

test.afterAll(async () => {
  await stack?.stop();
});

async function login(page: Page, scenario: string) {
  const email = `owner@quickstart-${scenario}.test`;
  const { tenant } = await stack.tenant().post("/api/signup", { org_name: `Quickstart ${scenario}`, email, password: PASSWORD });
  await stack.admin.post(`/admin/tenants/${tenant.id}/approve`);
  const current = await stack.admin.get(`/admin/tenants/${tenant.id}`);
  await stack.admin.patch(`/admin/tenants/${tenant.id}`, {
    allowed_provider_groups: [GROUP],
    allowed_model_ids: grantedModels,
  }, current.version);
  await page.goto(`${stack.previewUrl}/login`);
  await page.getByLabel("邮箱", { exact: true }).fill(email);
  await page.getByLabel("口令", { exact: true }).fill(PASSWORD);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await expect(page).toHaveURL(/\/console(?:#|$)/);
  await page.goto(`${stack.previewUrl}/console#keys`);
}

async function mint(page: Page, label: string, model?: string) {
  await page.getByRole("button", { name: "新建密钥", exact: true }).click();
  const form = page.getByRole("dialog", { name: "新建密钥", exact: true });
  await form.getByLabel("名称", { exact: true }).fill(label);
  if (model) {
    await form.getByRole("radio", { name: "指定模型", exact: true }).check();
    await form.getByRole("checkbox", { name: model, exact: true }).check();
  }
  const result = page.waitForResponse(response => new URL(response.url()).pathname === "/api/keys" && response.request().method() === "POST");
  await form.getByRole("button", { name: "新建密钥", exact: true }).click();
  const response = await result;
  expect(response.status()).toBe(201);
  const created: { key: { id: string; version: number }; plaintext: string } = await response.json();
  const confirmation = page.getByRole("dialog", { name: "密钥已创建", exact: true });
  await expect(confirmation).toBeVisible();
  return { created, confirmation };
}

async function options(page: Page, guide: Locator, label: string): Promise<string[]> {
  await guide.getByRole("combobox", { name: label, exact: true }).click();
  const list = page.getByRole("listbox");
  await expect(list).toBeVisible();
  const names = await list.getByRole("option").allTextContents();
  await page.keyboard.press("Escape");
  return names;
}

test("新建密钥的首个 API 调用可复制并从列表和详情重新打开", async ({ page }) => {
  await page.context().grantPermissions(["clipboard-read", "clipboard-write"], { origin: stack.previewUrl });
  await login(page, "selected");

  const label = "调用指南验收密钥";
  const { created, confirmation } = await mint(page, label, CHOSEN_MODEL);
  const reveals: string[] = [];
  page.on("request", request => { if (new URL(request.url()).pathname.endsWith("/reveal")) reveals.push(request.url()); });
  await confirmation.getByRole("button", { name: "首个 API 调用", exact: true }).click();
  const guide = page.getByRole("dialog", { name: "首个 API 调用", exact: true });
  await expect(guide).toBeVisible();
  const curl = guide.getByRole("region", { name: "cURL 示例", exact: true });
  await expect(curl).toContainText("https://api.routio.ai/v1/chat/completions");
  await expect(curl).toContainText(CHOSEN_MODEL);
  await expect(curl).toContainText("$ROUTIO_API_KEY");
  expect(await options(page, guide, "模型")).toEqual([CHOSEN_MODEL]);
  await curl.getByRole("button", { name: "复制 cURL 示例", exact: true }).click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe(await curl.locator("code").innerText());
  const python = guide.getByRole("region", { name: "Python 示例", exact: true });
  await expect(python).toContainText("client.chat.completions.create");
  await expect(python).toContainText("https://api.routio.ai/v1");

  await guide.getByRole("combobox", { name: "协议", exact: true }).click();
  await page.getByRole("option", { name: "Anthropic Messages", exact: true }).click();
  await expect(curl).toContainText("https://api.routio.ai/v1/messages");
  await expect(python).toContainText("client.messages.create");
  await guide.getByRole("combobox", { name: "协议", exact: true }).click();
  await page.getByRole("option", { name: "OpenAI Responses", exact: true }).click();
  await expect(curl).toContainText("https://api.routio.ai/v1/responses");
  await expect(python).toContainText("client.responses.create");

  // 对比实际新发的完整密钥。断言只报告布尔结果，避免失败输出包含凭据。
  const leaked = await page.evaluate(secret => [document.body.innerText, document.body.innerHTML, location.href, JSON.stringify(localStorage), JSON.stringify(sessionStorage)].some(value => value.includes(secret)), created.plaintext);
  expect(leaked).toBe(false);
  expect(reveals).toEqual([]);

  await guide.getByRole("button", { name: "关闭首个 API 调用", exact: true }).click();
  const keyRow = page.getByRole("row").filter({ has: page.getByRole("link", { name: label, exact: true }) });
  await keyRow.getByRole("button", { name: `更多操作：${label}`, exact: true }).click();
  await page.getByRole("menuitem", { name: "首个 API 调用", exact: true }).click();
  await expect(curl).toContainText(CHOSEN_MODEL);
  await guide.getByRole("button", { name: "关闭首个 API 调用", exact: true }).click();
  await keyRow.getByRole("link", { name: label, exact: true }).click();
  await expect(page.getByRole("heading", { name: label, exact: true })).toBeVisible();
  await page.getByRole("button", { name: "首个 API 调用", exact: true }).click();
  await expect(curl).toContainText(CHOSEN_MODEL);
  expect(reveals).toEqual([]);
});

test("继承授权的指南仅列出本分组可调用的组织模型", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await login(page, "inherited");
  const { confirmation } = await mint(page, "继承模型验收密钥");
  await confirmation.getByRole("button", { name: "首个 API 调用", exact: true }).click();
  const guide = page.getByRole("dialog", { name: "首个 API 调用", exact: true });
  await expect(guide.getByRole("region", { name: "cURL 示例", exact: true })).toBeVisible();
  expect(await options(page, guide, "模型")).toEqual([CHOSEN_MODEL, INHERITED_MODEL]);
  expect(await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth)).toBeLessThanOrEqual(1);
});

test("指南模型接口隔离组织并拒绝为已停用的密钥返回示例", async ({ page }) => {
  await login(page, "authorization");
  const { created } = await mint(page, "指南权限验收密钥", CHOSEN_MODEL);
  const keyPath = `/api/keys/${created.key.id}`;
  const guidePath = `${keyPath}/quickstart`;

  const owner = stack.tenant();
  await owner.post("/api/login", { email: "owner@quickstart-authorization.test", password: PASSWORD });
  const available = await owner.get(guidePath);
  expect(available.items.map((item: { name: string }) => item.name)).toEqual([CHOSEN_MODEL]);
  expect((await stack.tenant().request("GET", guidePath)).status).toBe(401);

  const outsider = stack.tenant();
  const { tenant } = await outsider.post("/api/signup", {
    org_name: "Quickstart unrelated organization", email: "owner@quickstart-outsider.test", password: PASSWORD,
  });
  await stack.admin.post(`/admin/tenants/${tenant.id}/approve`);
  await outsider.post("/api/login", { email: "owner@quickstart-outsider.test", password: PASSWORD });
  expect((await outsider.request("GET", guidePath)).status).toBe(404);

  await owner.patch(keyPath, { disabled: true }, created.key.version);
  expect(await owner.get(guidePath)).toEqual({ items: [] });
});
