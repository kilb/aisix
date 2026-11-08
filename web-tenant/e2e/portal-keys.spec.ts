import { randomUUID } from "node:crypto";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, fundTenant, startStack, type ApiClient, type Json, type Stack } from "./stack";

/**
 * The tenant portal's keys and members pages: who sees which keys, a new
 * key's secret shown once, what a key can call through the gateway, the
 * switches and the revocation, members, roles and invitations, what the
 * pages say while the organisation is suspended, and a session that ends
 * while a page is open. Against the real control plane and a real managed
 * gateway, through the built `dist/`; operator steps go through the real
 * `/admin`, and the gateway's upstream is the stack's stand-in.
 */

let stack: Stack;
/** The platform model every organisation here is granted. */
let chatModelId: string;

const PASSWORD = "portal-keys-pass-9x";
/** How long a change on the control plane may take to reach the gateway. */
const ROLLOUT_MS = 15_000;
/** The tests that wait on the gateway more than once. */
const GATEWAY_TEST_MS = 90_000;
/** The notice a new key's secret is shown in. */
const SECRET_NOTICE = "明文只显示这一次";

test.beforeAll(async () => {

  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
  // Every write that names a group names a live one: the accounts and the
  // tenant edits below included. A name a test here creates itself is
  // skipped.
  await stack.admin.seedGroups(["grp-eu-5d2"]);
  // The platform's catalog: one upstream account, in the default group the
  // tenants here reach and in a second, named group, and one model on it.
  const account = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: "acct-e2e-1",
      provider: "openai",
      api_key: "sk-acct-e2e-1-upstream",
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups: ["shared", "grp-eu-5d2"],
    },
  });
  const model = await stack.admin.post("/admin/resources/models", {
    doc: { display_name: "chat-e2e", provider: "openai", model_name: "gpt-4o-mini", provider_key_id: account.id },
  });
  chatModelId = model.id;
});

test.afterAll(async () => {
  await stack?.stop();
});

/**
 * An organisation signed up through `/api` by `ownerEmail`, approved, and
 * granted `chat-e2e`, with `limits` when given; its id.
 */
async function activeOrg(name: string, ownerEmail: string, limits?: Json): Promise<string> {
  const signedUp = await stack.tenant().post("/api/signup", { org_name: name, email: ownerEmail, password: PASSWORD });
  const id: string = signedUp.tenant.id;
  await stack.admin.post(`/admin/tenants/${id}/approve`);
  await fundTenant(stack.admin, id);
  const current = await stack.admin.get(`/admin/tenants/${id}`);
  await stack.admin.patch(
    `/admin/tenants/${id}`,
    { allowed_model_ids: [chatModelId], ...(limits ? { limits } : {}) },
    current.version,
  );
  return id;
}

/** `email` joins organisation `id` as `role`: an operator invites, the invitee accepts through `/api`. */
async function joined(id: string, email: string, role: string, displayName?: string): Promise<void> {
  const invite = await stack.admin.post(`/admin/tenants/${id}/invites`, { email, role });
  await stack.tenant().post("/api/invites/accept", {
    token: invite.invite_token,
    password: PASSWORD,
    display_name: displayName,
  });
}

/** A `/api` client signed in as `email`. */
async function signedIn(email: string, password = PASSWORD): Promise<ApiClient> {
  const client = stack.tenant();
  await client.post("/api/login", { email, password });
  return client;
}

async function login(page: Page, email: string): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令", { exact: true }).fill(PASSWORD);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await expect(page.getByRole("button", { name: "登出" })).toBeVisible();
}

async function openTab(page: Page, name: string): Promise<void> {
  await page.getByRole("tab", { name }).click();
}

/** The table row that mentions `text`. */
function row(page: Page, text: string): Locator {
  return page.getByRole("row").filter({ hasText: text });
}

/** The status the gateway answers `key` for `body` at `path`. */
async function gateway(key: string, path: string, body: unknown): Promise<number> {
  const res = await fetch(`${stack.proxyUrl}${path}`, {
    method: "POST",
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  await res.arrayBuffer();
  return res.status;
}

function chat(key: string): Promise<number> {
  return gateway(key, "/v1/chat/completions", { model: "chat-e2e", messages: [{ role: "user", content: "hi" }] });
}

/** Waits until the gateway answers a chat with `key` with `status`. */
async function chatSettles(key: string, status: number): Promise<void> {
  await expect.poll(() => chat(key), { timeout: ROLLOUT_MS, message: `chat with the key answers ${status}` }).toBe(status);
}

/** The text of the elements that describe `field` (its hint, and its refusal when there is one). */
function description(field: Locator): Promise<string> {
  return field.evaluate((el) =>
    (el.getAttribute("aria-describedby") ?? "")
      .split(/\s+/)
      .filter(Boolean)
      .map((id) => document.getElementById(id)?.textContent ?? "")
      .join(" "),
  );
}

test("新建密钥：明文只显示一次，能调用被授权的模型，不能调用图片接口", async ({ page }) => {
  test.setTimeout(GATEWAY_TEST_MS);
  await activeOrg("Keys Org 1a7", "owner@keys-1a7.test");
  const consoleLines: string[] = [];
  page.on("console", (m) => consoleLines.push(m.text()));
  // Chromium's permission names: the suite runs Chromium only (CI installs
  // it alone), and other engines name or grant clipboard access differently.
  await page.context().grantPermissions(["clipboard-read", "clipboard-write"], { origin: stack.previewUrl });
  await login(page, "owner@keys-1a7.test");
  await openTab(page, "密钥");

  // The picker offers the organisation's one grant, and nothing else.
  const picker = page.getByRole("group", { name: "可用模型" });
  await expect(picker.getByRole("checkbox")).toHaveCount(1);
  await expect(picker.getByRole("checkbox", { name: "chat-e2e" })).toBeVisible();

  await page.getByLabel("名称", { exact: true }).fill("first-key-1a7");
  await picker.getByRole("checkbox", { name: "chat-e2e" }).check();
  await page.getByRole("button", { name: "新建密钥" }).click();
  const notice = page.getByRole("region", { name: SECRET_NOTICE });
  await expect(notice).toBeVisible();
  const plaintext = await notice.getByRole("textbox").inputValue();
  expect(plaintext.length).toBeGreaterThan(20);
  await notice.getByRole("button", { name: "复制" }).click();
  // The write is done once the notice says so.
  await expect(notice.getByRole("status")).toHaveText("已复制");
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(plaintext);

  // The list names the key by the prefix the API keeps of it, never the
  // whole secret.
  const { items } = await (await signedIn("owner@keys-1a7.test")).get("/api/keys");
  const prefix: string = items[0].prefix;
  expect(plaintext.startsWith(prefix)).toBe(true);
  expect(prefix.length).toBeLessThan(plaintext.length);
  const listed = row(page, "first-key-1a7");
  await expect(listed).toContainText("chat-e2e");
  await expect(listed).toContainText(prefix);
  await expect(listed).not.toContainText(plaintext);

  // The key reaches its model through the gateway, and nothing outside chat.
  await chatSettles(plaintext, 200);
  expect(await gateway(plaintext, "/v1/images/generations", { model: "chat-e2e", prompt: "a cat" })).toBe(403);

  // Away to another page and back: the secret is gone.
  await openTab(page, "组织");
  await openTab(page, "密钥");
  await expect(row(page, "first-key-1a7")).toBeVisible();
  await expect(page.getByRole("region", { name: SECRET_NOTICE })).toHaveCount(0);

  // After a reload the secret is nowhere: not on the page, not in storage,
  // not in the address, not in the console.
  await page.reload();
  await openTab(page, "密钥");
  await expect(row(page, "first-key-1a7")).toBeVisible();
  await expect(page.getByRole("region", { name: SECRET_NOTICE })).toHaveCount(0);
  expect(await page.content()).not.toContain(plaintext);
  const kept = await page.evaluate(() =>
    JSON.stringify([
      { ...localStorage },
      { ...sessionStorage },
      document.cookie,
      [...document.querySelectorAll("input")].map((i) => i.value),
    ]),
  );
  expect(kept).not.toContain(plaintext);
  expect(page.url()).not.toContain(plaintext);
  expect(consoleLines.join("\n")).not.toContain(plaintext);
});

test("门户页面上看不到任何上游信息", async ({ page }) => {
  const id = await activeOrg("Upstream Org 2b8", "owner@upstream-2b8.test");
  await joined(id, "member@upstream-2b8.test", "member", "Uma 2b8");
  const owner = await signedIn("owner@upstream-2b8.test");
  await owner.post("/api/keys", { label: "scan-key-2b8", model_ids: [chatModelId] });
  await login(page, "owner@upstream-2b8.test");

  const seen: string[] = [];
  const look = async () => seen.push(await page.locator("body").innerText(), await page.content());
  await openTab(page, "密钥");
  // Each page is scanned once it shows what it holds.
  await expect(row(page, "scan-key-2b8")).toContainText("chat-e2e");
  await expect(page.getByRole("group", { name: "可用模型" }).getByRole("checkbox", { name: "chat-e2e" })).toBeVisible();
  await look();
  await openTab(page, "成员");
  await expect(row(page, "member@upstream-2b8.test")).toBeVisible();
  await look();
  await openTab(page, "组织");
  await expect(page.getByRole("main")).toContainText("Upstream Org 2b8");
  await look();

  for (const tell of ["gpt-4o-mini", "acct-e2e-1", "openai", "grp-eu-5d2"]) {
    for (const text of seen) expect.soft(text).not.toContain(tell);
  }
});

test("普通成员只看到自己的密钥", async ({ page, browser }) => {
  const id = await activeOrg("Own Keys Org 3c9", "owner@own-3c9.test");
  await joined(id, "member@own-3c9.test", "member", "Mo 3c9");
  const owner = await signedIn("owner@own-3c9.test");
  await owner.post("/api/keys", { label: "owner-key-3c9" });

  // The member, in a browser of their own, makes a key: theirs, without a
  // choice of whose.
  const mc = await browser.newContext();
  const mp = await mc.newPage();
  await login(mp, "member@own-3c9.test");
  await openTab(mp, "密钥");
  await expect(mp.getByLabel("归属成员")).toHaveCount(0);
  await mp.getByLabel("名称", { exact: true }).fill("member-key-3c9");
  await mp.getByRole("button", { name: "新建密钥" }).click();
  await expect(mp.getByRole("region", { name: SECRET_NOTICE })).toBeVisible();
  await expect(row(mp, "member-key-3c9")).toBeVisible();
  await expect(mp.getByText("owner-key-3c9")).toHaveCount(0);
  // The same after a fresh read of the page.
  await mp.reload();
  await openTab(mp, "密钥");
  await expect(row(mp, "member-key-3c9")).toBeVisible();
  await expect(mp.getByText("owner-key-3c9")).toHaveCount(0);
  await mc.close();

  // The owner sees both, the member's under the member's name, and may make
  // one for the member.
  await login(page, "owner@own-3c9.test");
  await openTab(page, "密钥");
  await expect(row(page, "owner-key-3c9")).toBeVisible();
  await expect(row(page, "member-key-3c9")).toContainText("Mo 3c9");
  await expect(page.getByLabel("归属成员").getByRole("option", { name: "Mo 3c9" })).toHaveCount(1);
});

test("账单角色看不到密钥页，直接请求也被拒", async ({ page }) => {
  const id = await activeOrg("Billing Keys Org 4d1", "owner@billing-4d1.test");
  await joined(id, "billing@billing-4d1.test", "billing");
  await login(page, "billing@billing-4d1.test");
  await expect(page.getByRole("tab")).toHaveText(["组织", "用量", "预算", "账户"]);
  await expect(page.getByRole("button", { name: "新建密钥" })).toHaveCount(0);

  const statuses = await page.evaluate(async () => {
    const list = await fetch("/api/keys");
    const mint = await fetch("/api/keys", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ label: "billing-key-4d1" }),
    });
    return [list.status, mint.status];
  });
  expect(statuses).toEqual([403, 403]);
  const owner = await signedIn("owner@billing-4d1.test");
  expect((await owner.get("/api/keys")).items).toEqual([]);
});

test("移除成员后，他的密钥在网关上被拒", async ({ page, browser }) => {
  test.setTimeout(GATEWAY_TEST_MS);
  const id = await activeOrg("Remove Org 5e2", "owner@remove-5e2.test");
  await joined(id, "member@remove-5e2.test", "member", "Rex 5e2");
  const member = await signedIn("member@remove-5e2.test");
  const key = await member.post("/api/keys", { label: "rex-key-5e2" });
  await chatSettles(key.plaintext, 200);
  // The member has the portal open meanwhile.
  const mc = await browser.newContext();
  const mp = await mc.newPage();
  await login(mp, "member@remove-5e2.test");
  await openTab(mp, "密钥");
  await expect(row(mp, "rex-key-5e2")).toBeVisible();

  await login(page, "owner@remove-5e2.test");
  await openTab(page, "成员");
  const target = row(page, "member@remove-5e2.test");
  await target.getByRole("button", { name: "移除" }).click();
  await expect(target).toContainText("该成员的所有密钥会被吊销");
  await target.getByRole("button", { name: "确认移除" }).click();
  await expect(row(page, "member@remove-5e2.test")).toHaveCount(0);
  await chatSettles(key.plaintext, 401);

  // The removed member's open page goes back to the login form at its next request.
  await mp.getByLabel("名称", { exact: true }).fill("after-removal-5e2");
  await mp.getByRole("button", { name: "新建密钥" }).click();
  await expect(mp.getByRole("button", { name: "登录", exact: true })).toBeVisible();
  await expect(mp.getByText("Remove Org 5e2")).toHaveCount(0);
  await mc.close();
  const owner = await signedIn("owner@remove-5e2.test");
  expect((await owner.get("/api/keys")).items).toEqual([]);
});

test("最后一个所有者不能被降级", async ({ page }) => {
  await activeOrg("Last Owner Org 6f3", "owner@last-6f3.test");
  await login(page, "owner@last-6f3.test");
  await openTab(page, "成员");
  const self = row(page, "owner@last-6f3.test");
  await self.getByRole("combobox").selectOption({ label: "管理员" });
  await self.getByRole("button", { name: "确认更改角色" }).click();
  await expect(page.getByRole("alert")).toContainText("组织至少要有一个所有者");
  // Removing the last owner is refused the same way, on a fresh page whose
  // only refusal can be this one.
  await page.reload();
  await openTab(page, "成员");
  await expect(self).toBeVisible();
  await expect(page.getByRole("alert")).toHaveCount(0);
  await self.getByRole("button", { name: "移除" }).click();
  await self.getByRole("button", { name: "确认移除" }).click();
  await expect(page.getByRole("alert")).toContainText("组织至少要有一个所有者");

  // Still the owner, as the control plane sees it.
  await page.reload();
  await expect(page.getByRole("banner")).toContainText("所有者");
  await expect(page.getByRole("tab")).toHaveText(["组织", "密钥", "成员", "用量", "预算", "账户"]);
  const session = await (await signedIn("owner@last-6f3.test")).get("/api/session");
  expect(session.member.role).toBe("owner");
});

test("别的组织的密钥 id 与不存在的 id 得到同样的 404", async ({ page }) => {
  await activeOrg("Scope Org A 7a4", "owner@scope-a-7a4.test");
  const other = await stack.admin.post("/admin/tenants", { name: "Scope Org B 7a4" });
  const theirs = await stack.admin.post(`/admin/tenants/${other.id}/keys`, { label: "b-key-7a4" });
  const theirId: string = theirs.key.id;
  await login(page, "owner@scope-a-7a4.test");

  const answers = await page.evaluate(async (ids) => {
    const out: { status: number; body: string }[] = [];
    for (const id of ids) {
      for (const method of ["PATCH", "DELETE"]) {
        const res = await fetch(`/api/keys/${id}`, {
          method,
          headers: { "content-type": "application/json", "if-match": '"1"' },
          body: method === "PATCH" ? JSON.stringify({ disabled: true }) : undefined,
        });
        out.push({ status: res.status, body: await res.text() });
      }
    }
    return out;
  }, [theirId, randomUUID()]);
  // [their PATCH, their DELETE, random PATCH, random DELETE]
  expect(answers.map((a) => a.status)).toEqual([404, 404, 404, 404]);
  const bodies = answers.map((a) => a.body);
  expect([bodies[0], bodies[1]]).toEqual([bodies[2], bodies[3]]);

  // The other organisation's key is as it was.
  const still = await stack.admin.request("GET", `/admin/keys/${theirId}`);
  expect(still.status).toBe(200);
  expect(still.body.disabled_by_tenant).toBe(false);
  expect(still.body.version).toBe(1);
});

test("停用后网关拒绝，启用后恢复；吊销要再确认一次", async ({ page }) => {
  test.setTimeout(GATEWAY_TEST_MS);
  await activeOrg("Switch Org 8b5", "owner@switch-8b5.test");
  const owner = await signedIn("owner@switch-8b5.test");
  const key = await owner.post("/api/keys", { label: "switch-key-8b5" });
  await chatSettles(key.plaintext, 200);
  await login(page, "owner@switch-8b5.test");
  await openTab(page, "密钥");
  const listed = row(page, "switch-key-8b5");

  await listed.getByRole("button", { name: "停用" }).click();
  await expect(listed.getByRole("button", { name: "启用" })).toBeVisible();
  await expect(listed).toContainText("已停用");
  await chatSettles(key.plaintext, 401);
  await listed.getByRole("button", { name: "启用" }).click();
  await expect(listed.getByRole("button", { name: "停用" })).toBeVisible();
  await chatSettles(key.plaintext, 200);

  // The first click on 吊销 only asks; 取消 leaves the key alone.
  await listed.getByRole("button", { name: "吊销" }).click();
  await expect(listed.getByRole("button", { name: "确认吊销" })).toBeVisible();
  await listed.getByRole("button", { name: "取消" }).click();
  await expect(listed.getByRole("button", { name: "确认吊销" })).toHaveCount(0);
  expect((await owner.get("/api/keys")).items.map((k: Json) => k.label)).toEqual(["switch-key-8b5"]);

  await listed.getByRole("button", { name: "吊销" }).click();
  await listed.getByRole("button", { name: "确认吊销" }).click();
  await expect(row(page, "switch-key-8b5")).toHaveCount(0);
  await chatSettles(key.plaintext, 401);
  expect((await owner.get("/api/keys")).items).toEqual([]);
});

test("平台停用的密钥，成员不能重新启用", async ({ page }) => {
  await activeOrg("Platform Off Org 9c6", "owner@platform-9c6.test");
  const owner = await signedIn("owner@platform-9c6.test");
  const key = await owner.post("/api/keys", { label: "platform-key-9c6" });
  const current = await stack.admin.get(`/admin/keys/${key.key.id}`);
  await stack.admin.patch(`/admin/keys/${key.key.id}`, { disabled: true }, current.version);

  await login(page, "owner@platform-9c6.test");
  await openTab(page, "密钥");
  const listed = row(page, "platform-key-9c6");
  await expect(listed).toContainText("平台");
  await expect(listed.getByRole("button", { name: "启用" })).toHaveCount(0);
  await expect(listed.getByRole("button", { name: "吊销" })).toBeVisible();
});

test("单个密钥的限制超出组织上限时，错误显示在该字段旁", async ({ page }) => {
  await activeOrg("Cap Org 0d7", "owner@cap-0d7.test", { key_concurrency_max: 4, key_rpm_max: 120 });
  await login(page, "owner@cap-0d7.test");
  await openTab(page, "密钥");
  const concurrency = page.getByLabel("并发", { exact: true });
  const rpm = page.getByLabel("每分钟请求", { exact: true });
  // Each field states the organisation's cap.
  await expect(concurrency).toHaveAccessibleDescription(/4/);
  await expect(rpm).toHaveAccessibleDescription(/120/);
  const concurrencyBefore = await description(concurrency);
  const rpmBefore = await description(rpm);

  await page.getByLabel("名称", { exact: true }).fill("cap-key-0d7");
  await concurrency.fill("9");
  await rpm.fill("60");
  await page.getByRole("button", { name: "新建密钥" }).click();
  // The refusal is the over-cap field's, and only its.
  await expect(concurrency).toHaveAttribute("aria-invalid", "true");
  await expect.poll(() => description(concurrency)).not.toBe(concurrencyBefore);
  await expect(rpm).not.toHaveAttribute("aria-invalid", "true");
  expect(await description(rpm)).toBe(rpmBefore);
  await expect(page.getByRole("region", { name: SECRET_NOTICE })).toHaveCount(0);
  const owner = await signedIn("owner@cap-0d7.test");
  expect((await owner.get("/api/keys")).items).toEqual([]);

  // Within the cap the same form goes through.
  await concurrency.fill("3");
  await page.getByRole("button", { name: "新建密钥" }).click();
  await expect(page.getByRole("region", { name: SECRET_NOTICE })).toBeVisible();
  await expect(concurrency).not.toHaveAttribute("aria-invalid", "true");
  expect((await owner.get("/api/keys")).items.map((k: Json) => k.rate_limit)).toEqual([{ concurrency: 3, rpm: 60 }]);
});

test("邀请成员：链接只显示一次，打开即可加入；管理员不能邀请所有者；撤销的邀请失效", async ({ page, browser }) => {
  const id = await activeOrg("Invite Org 1e8", "owner@invite-1e8.test");
  await joined(id, "admin@invite-1e8.test", "admin", "Ada 1e8");
  await login(page, "admin@invite-1e8.test");
  await openTab(page, "成员");
  const role = page.getByRole("combobox", { name: "角色", exact: true });
  // Waits for the form, then reads every option it offers.
  await expect(role.getByRole("option", { name: "成员", exact: true })).toHaveCount(1);
  expect(await role.getByRole("option").allTextContents()).not.toContain("所有者");

  /** Invites `email` as a 成员 through the form; the link shown. */
  const invite = async (email: string): Promise<string> => {
    await page.getByLabel("邮箱", { exact: true }).fill(email);
    await role.selectOption({ label: "成员" });
    await page.getByRole("button", { name: "邀请成员" }).click();
    // Listed as open once issued; the notice may still show an earlier link until then.
    await expect(row(page, email)).toBeVisible();
    const notice = page.getByRole("region", { name: /只显示这一次/ });
    await expect(notice).toBeVisible();
    await expect(notice.getByRole("button", { name: "复制" })).toBeVisible();
    return notice.getByRole("textbox").inputValue();
  };
  const link = await invite("new@invite-1e8.test");
  // The portal's own invitation address, the token in its fragment.
  expect(link.startsWith(`${stack.previewUrl}/invite#token=`)).toBe(true);
  const token = link.slice(link.indexOf("#token=") + "#token=".length);
  expect(token.length).toBeGreaterThan(15);

  const second = await invite("gone@invite-1e8.test");
  expect(second).not.toBe(link);
  await row(page, "gone@invite-1e8.test").getByRole("button", { name: "撤销" }).click();
  await expect(row(page, "gone@invite-1e8.test")).toHaveCount(0);

  // A reload shows the open invitation, not its link.
  await page.reload();
  await openTab(page, "成员");
  await expect(row(page, "new@invite-1e8.test")).toBeVisible();
  expect(await page.content()).not.toContain(token);

  // The revoked link opens nothing; the first one joins the organisation.
  const gc = await browser.newContext();
  const gp = await gc.newPage();
  await gp.goto(second);
  await gp.getByLabel("口令", { exact: true }).fill("revoked-pass-1e8x");
  await gp.getByRole("button", { name: "接受邀请" }).click();
  await expect(gp.getByRole("alert")).toContainText("邀请无效或已过期");
  await gc.close();
  const nc = await browser.newContext();
  const np = await nc.newPage();
  await np.goto(link);
  await np.getByLabel("口令", { exact: true }).fill("joined-pass-1e8x");
  await np.getByRole("button", { name: "接受邀请" }).click();
  await expect(np.getByRole("banner")).toContainText("Invite Org 1e8");
  await expect(np.getByRole("banner")).toContainText("成员");
  await nc.close();
});

test("在别处结束会话后，页面上的下一次操作回到登录框", async ({ page }) => {
  await activeOrg("Expiry Org 2f9", "owner@expiry-2f9.test");
  await login(page, "owner@expiry-2f9.test");
  await openTab(page, "成员");
  await expect(row(page, "owner@expiry-2f9.test")).toBeVisible();

  // The same owner, elsewhere, changes the password: every other session ends.
  const elsewhere = await signedIn("owner@expiry-2f9.test");
  await elsewhere.post("/api/password", { current_password: PASSWORD, new_password: "expiry-new-pass-2f9" });

  await page.getByLabel("邮箱", { exact: true }).fill("late@expiry-2f9.test");
  await page.getByRole("button", { name: "邀请成员" }).click();
  await expect(page.getByRole("button", { name: "登录", exact: true })).toBeVisible();
  await expect(page.getByText("登录已过期，请重新登录")).toBeVisible();
  await expect(page.getByText("Expiry Org 2f9")).toHaveCount(0);
  expect((await elsewhere.get("/api/members/invites")).items).toEqual([]);
});

test("管理员移除自己后回到登录框，提示已离开组织而不是登录过期", async ({ page }) => {
  const id = await activeOrg("Leave Org 5c3", "owner@leave-5c3.test");
  await joined(id, "admin@leave-5c3.test", "admin", "Lee 5c3");
  await login(page, "admin@leave-5c3.test");
  await openTab(page, "成员");
  const self = row(page, "admin@leave-5c3.test");
  await self.getByRole("button", { name: "移除" }).click();
  await self.getByRole("button", { name: "确认移除" }).click();

  await expect(page.getByRole("button", { name: "登录", exact: true })).toBeVisible();
  await expect(page.getByText("你已离开该组织")).toBeVisible();
  await expect(page.getByText("登录已过期，请重新登录")).toHaveCount(0);
  await expect(page.getByText("Leave Org 5c3")).toHaveCount(0);
  // Gone for good: no longer a member, and the password no longer signs in.
  const owner = await signedIn("owner@leave-5c3.test");
  const emails = (await owner.get("/api/members")).items.map((m: Json) => m.email);
  expect(emails).toEqual(["owner@leave-5c3.test"]);
  const again = await stack.tenant().request("POST", "/api/login", { email: "admin@leave-5c3.test", password: PASSWORD });
  expect(again.status).toBe(401);
});

test("页面打开期间组织被停用，下一次被拒的操作后页面随之变化", async ({ page }) => {
  const id = await activeOrg("Stale Org 6d4", "owner@stale-6d4.test");
  await login(page, "owner@stale-6d4.test");
  await openTab(page, "密钥");
  await expect(page.getByLabel("名称", { exact: true })).toBeVisible();

  // Suspended while the page still shows the form.
  await stack.admin.post(`/admin/tenants/${id}/suspend`);
  await page.getByLabel("名称", { exact: true }).fill("late-key-6d4");
  await page.getByRole("button", { name: "新建密钥" }).click();

  // The refusal brings the page to the state the organisation is in. The
  // note first: while the request is in flight the button reads otherwise,
  // so its absence alone proves nothing until the answer is in.
  await expect(page.getByText("组织已被停用", { exact: false })).toBeVisible();
  await expect(page.getByLabel("名称", { exact: true })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "新建密钥" })).toHaveCount(0);
  await expect(page.getByRole("tab")).toHaveText(["组织", "密钥", "成员"]);
  expect((await stack.admin.get(`/admin/tenants/${id}/keys`)).items).toEqual([]);
});

test("停用的组织：密钥与成员页只剩吊销与移除", async ({ page }) => {
  const id = await activeOrg("Suspended Org 3a1", "owner@suspended-3a1.test");
  await joined(id, "member@suspended-3a1.test", "member");
  const owner = await signedIn("owner@suspended-3a1.test");
  await owner.post("/api/keys", { label: "kept-key-3a1" });
  await stack.admin.post(`/admin/tenants/${id}/suspend`);

  await login(page, "owner@suspended-3a1.test");
  await expect(page.getByText("组织已被停用，请联系平台")).toBeVisible();
  await expect(page.getByRole("tab")).toHaveText(["组织", "密钥", "成员"]);

  // The pages list what the organisation has, and offer nothing that makes
  // or opens anything.
  await openTab(page, "密钥");
  await expect(row(page, "kept-key-3a1")).toBeVisible();
  await expect(page.getByRole("button", { name: "新建密钥" })).toHaveCount(0);
  await expect(page.getByLabel("名称", { exact: true })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "停用" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "启用" })).toHaveCount(0);
  await openTab(page, "成员");
  await expect(row(page, "member@suspended-3a1.test")).toBeVisible();
  await expect(page.getByRole("button", { name: "邀请成员" })).toHaveCount(0);
  await expect(page.getByRole("combobox")).toHaveCount(0);
});

// Needs a control plane that lists a suspended organisation's keys and
// members (GET /api/keys and GET /api/members for an inactive tenant) and
// lets it revoke and remove (the P1a final fix).
test("停用期间吊销的密钥与移除的成员，恢复后仍然无效", async ({ page }) => {
  test.setTimeout(GATEWAY_TEST_MS);
  const id = await activeOrg("Suspend Close Org 4b2", "owner@close-4b2.test");
  await joined(id, "member@close-4b2.test", "member", "Mel 4b2");
  const owner = await signedIn("owner@close-4b2.test");
  const member = await signedIn("member@close-4b2.test");
  const leaked = await owner.post("/api/keys", { label: "leaked-key-4b2" });
  const kept = await owner.post("/api/keys", { label: "kept-key-4b2" });
  const theirs = await member.post("/api/keys", { label: "mel-key-4b2" });
  for (const key of [leaked, kept, theirs]) await chatSettles(key.plaintext, 200);
  await stack.admin.post(`/admin/tenants/${id}/suspend`);
  await chatSettles(kept.plaintext, 401);

  await login(page, "owner@close-4b2.test");
  await openTab(page, "密钥");
  const listed = row(page, "leaked-key-4b2");
  await listed.getByRole("button", { name: "吊销" }).click();
  await listed.getByRole("button", { name: "确认吊销" }).click();
  await expect(row(page, "leaked-key-4b2")).toHaveCount(0);
  await openTab(page, "成员");
  const leaving = row(page, "member@close-4b2.test");
  await leaving.getByRole("button", { name: "移除" }).click();
  await leaving.getByRole("button", { name: "确认移除" }).click();
  await expect(row(page, "member@close-4b2.test")).toHaveCount(0);

  // Reactivated: the key nobody closed works again, and the two closed
  // while suspended stay closed.
  await stack.admin.post(`/admin/tenants/${id}/reactivate`);
  await chatSettles(kept.plaintext, 200);
  expect(await chat(leaked.plaintext)).toBe(401);
  expect(await chat(theirs.plaintext)).toBe(401);
});
