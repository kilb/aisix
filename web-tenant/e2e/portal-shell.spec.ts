import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

/**
 * The tenant portal's shell: signing an organisation up, logging in and
 * out, accepting an invitation, and what a member sees while the
 * organisation waits for approval, once it is approved, and once it is
 * suspended. Against the real control plane through the built `dist/`;
 * operator steps go through the real `/admin`.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

/** Fills and submits the sign-up form on the page as it stands. */
async function signup(page: Page, org: string, email: string, password: string): Promise<void> {
  await page.getByRole("tab", { name: "注册组织" }).click();
  await page.getByLabel("组织名称").fill(org);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令", { exact: true }).fill(password);
  await page.getByRole("button", { name: "注册组织" }).click();
}

async function login(page: Page, email: string, password: string): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令", { exact: true }).fill(password);
  await page.getByRole("button", { name: "登录", exact: true }).click();
}

/** The id of the tenant named `name`, as the operators see it. */
async function tenantId(name: string): Promise<string> {
  const { items } = await stack.admin.get("/admin/tenants");
  const found = items.find((t: { name: string }) => t.name === name);
  if (!found) throw new Error(`no tenant named ${name}`);
  return found.id;
}

async function tenantNames(): Promise<string[]> {
  const { items } = await stack.admin.get("/admin/tenants");
  return items.map((t: { name: string }) => t.name);
}

test("注册组织后显示等待审核，没有密钥与成员入口", async ({ page }) => {
  await page.goto(stack.previewUrl);
  await signup(page, "Acme", "owner@acme.test", "correct horse battery");
  await expect(page.getByText("组织正在等待平台审核")).toBeVisible();
  await expect(page.getByRole("tab", { name: "密钥" })).toHaveCount(0);
  await expect(page.getByRole("tab", { name: "成员" })).toHaveCount(0);
});

test("口令过短在注册处就被挡下", async ({ page }) => {
  await page.goto(stack.previewUrl);
  await page.getByRole("tab", { name: "注册组织" }).click();
  // The minimum is stated before anything is typed.
  await expect(page.getByText(/12\s*个字符/)).toBeVisible();
  await signup(page, "Shortpw Org 4e1", "owner@shortpw-4e1.test", "eleven-char");
  await expect(page.getByRole("alert")).toContainText("12");
  await expect(page.getByRole("button", { name: "注册组织" })).toBeVisible();
  await expect(page.getByText("组织正在等待平台审核")).toHaveCount(0);
  // Nothing was created behind the refusal.
  expect(await tenantNames()).not.toContain("Shortpw Org 4e1");
});

test("审核通过后刷新即可看到密钥页签", async ({ page }) => {
  await page.goto(stack.previewUrl);
  await signup(page, "Approve Org 7a3", "owner@approve-7a3.test", "approve-pass-7a3x");
  await expect(page.getByText("组织正在等待平台审核")).toBeVisible();
  await expect(page.getByRole("tab", { name: "密钥" })).toHaveCount(0);

  await stack.admin.post(`/admin/tenants/${await tenantId("Approve Org 7a3")}/approve`);
  await page.reload();
  // The owner of an active organisation reaches every page.
  await expect(page.getByRole("tab")).toHaveText(["组织", "密钥", "成员"]);
  await expect(page.getByText("组织正在等待平台审核")).toHaveCount(0);
});

test("邀请链接：设置口令后以成员身份登录，同一链接再次提交被拒", async ({ page, browser }) => {
  // An operator creates an active organisation and invites a member to it.
  const tenant = await stack.admin.post("/admin/tenants", { name: "Invite Org 2c9" });
  const invite = await stack.admin.post(`/admin/tenants/${tenant.id}/invites`, {
    email: "member@invite-2c9.test",
    role: "member",
  });
  const token: string = invite.invite_token;
  expect(token).toMatch(/^[0-9a-f]{16,}$/);
  // The link the invitee gets carries the token in its fragment.
  const link = `${stack.previewUrl}/invite#token=${token}`;

  // Context B (this test's own): the invitee.
  const pb = page;
  const sentUrls: string[] = [];
  const acceptBodies: string[] = [];
  pb.on("request", (r) => {
    sentUrls.push(r.url());
    if (r.method() === "POST" && new URL(r.url()).pathname === "/api/invites/accept") {
      acceptBodies.push(r.postData() ?? "");
    }
  });
  await pb.goto(link);
  await expect(pb.getByRole("button", { name: "接受邀请" })).toBeVisible();
  // The token is out of the address bar as soon as the page is up.
  expect(pb.url()).not.toContain(token);
  expect(new URL(pb.url()).hash).toBe("");
  await pb.getByLabel("口令", { exact: true }).fill("member-pass-2c9x");
  await pb.getByLabel("显示名").fill("Mia 2c9");
  await pb.getByRole("button", { name: "接受邀请" }).click();

  // Signed in as a member of that organisation: no members page.
  await expect(pb.getByRole("banner")).toContainText("Invite Org 2c9");
  await expect(pb.getByRole("banner")).toContainText("Mia 2c9");
  await expect(pb.getByRole("banner")).toContainText("成员");
  await expect(pb.getByRole("tab")).toHaveText(["组织", "密钥"]);
  // The token went out once, in a request body, and nowhere else.
  expect(acceptBodies).toHaveLength(1);
  expect(acceptBodies[0]).toContain(token);
  expect(sentUrls.filter((u) => u.includes(token))).toEqual([]);
  const stored = await pb.evaluate(() => JSON.stringify([{ ...localStorage }, { ...sessionStorage }, document.cookie]));
  expect(stored).not.toContain(token);

  // Context C: the same link a second time.
  const c = await browser.newContext();
  const pc = await c.newPage();
  await pc.goto(link);
  await pc.getByLabel("口令", { exact: true }).fill("second-pass-2c9x");
  await pc.getByRole("button", { name: "接受邀请" }).click();
  await expect(pc.getByRole("alert")).toContainText("邀请无效或已过期");
  await expect(pc.getByText("Invite Org 2c9")).toHaveCount(0);
  await c.close();
});

test("登出后回到登录框，组织信息不再渲染", async ({ page }) => {
  await page.goto(stack.previewUrl);
  await signup(page, "Logout Org 5b8", "owner@logout-5b8.test", "logout-pass-5b8x");
  await expect(page.getByRole("banner")).toContainText("Logout Org 5b8");
  await page.getByRole("button", { name: "登出" }).click();
  await expect(page.getByLabel("邮箱")).toBeVisible();
  await expect(page.getByText("Logout Org 5b8")).toHaveCount(0);
  // The session itself is gone, not just the page's copy of it.
  await page.reload();
  await expect(page.getByLabel("邮箱")).toBeVisible();
  await expect(page.getByText("Logout Org 5b8")).toHaveCount(0);
});

test("不存在的账号与口令错误报同样的错，且不进入", async ({ page }) => {
  await stack.tenant().post("/api/signup", {
    org_name: "Login Org 3d6",
    email: "owner@login-3d6.test",
    password: "login-pass-3d6x",
  });
  await login(page, "owner@login-3d6.test", "wrong-password-3d6");
  await expect(page.getByRole("alert")).toBeVisible();
  const wrongPassword = (await page.getByRole("alert").textContent())?.trim() ?? "";
  expect(wrongPassword).not.toBe("");
  await expect(page.getByText("Login Org 3d6")).toHaveCount(0);

  await login(page, "nobody@login-3d6.test", "wrong-password-3d6");
  await expect(page.getByRole("alert")).toHaveText(wrongPassword);
  await expect(page.getByText("Login Org 3d6")).toHaveCount(0);

  // The account does exist: its own password lets it in.
  await login(page, "owner@login-3d6.test", "login-pass-3d6x");
  await expect(page.getByRole("banner")).toContainText("Login Org 3d6");
});

test("用已有账号的邮箱注册组织，提示不透露该邮箱已有账号", async ({ page }) => {
  await stack.tenant().post("/api/signup", {
    org_name: "Taken Org 8f2",
    email: "owner@taken-8f2.test",
    password: "taken-pass-8f2x",
  });
  await page.goto(stack.previewUrl);
  await signup(page, "Second Org 8f2", "owner@taken-8f2.test", "second-pass-8f2x");
  await expect(page.getByRole("alert")).toBeVisible();
  const text = (await page.getByRole("alert").textContent()) ?? "";
  for (const tell of ["已被使用", "已被注册", "已注册", "已存在", "email_taken"]) {
    expect(text).not.toContain(tell);
  }
  await expect(page.getByRole("button", { name: "注册组织" })).toBeVisible();
  expect(await tenantNames()).not.toContain("Second Org 8f2");
});

test("停用的组织登录后只看到停用说明", async ({ page }) => {
  await stack.tenant().post("/api/signup", {
    org_name: "Suspend Org 6c4",
    email: "owner@suspend-6c4.test",
    password: "suspend-pass-6c4x",
  });
  const id = await tenantId("Suspend Org 6c4");
  await stack.admin.post(`/admin/tenants/${id}/approve`);
  await stack.admin.post(`/admin/tenants/${id}/suspend`);

  await login(page, "owner@suspend-6c4.test", "suspend-pass-6c4x");
  await expect(page.getByText("组织已被停用，请联系平台")).toBeVisible();
  await expect(page.getByRole("tab", { name: "密钥" })).toHaveCount(0);
  await expect(page.getByRole("tab", { name: "成员" })).toHaveCount(0);
});

test("等待审核时也能修改口令，之后旧口令被拒、新口令可登录", async ({ page }) => {
  await page.goto(stack.previewUrl);
  await signup(page, "Password Org 9e5", "owner@password-9e5.test", "first-pass-9e5x");
  await expect(page.getByText("组织正在等待平台审核")).toBeVisible();
  await page.getByLabel("当前口令").fill("first-pass-9e5x");
  await page.getByLabel("新口令").fill("second-pass-9e5x");
  await page.getByRole("button", { name: "修改口令" }).click();
  await expect(page.getByRole("status")).toContainText("口令已修改");
  await page.getByRole("button", { name: "登出" }).click();

  await login(page, "owner@password-9e5.test", "first-pass-9e5x");
  await expect(page.getByRole("alert")).toBeVisible();
  await expect(page.getByText("Password Org 9e5")).toHaveCount(0);
  await login(page, "owner@password-9e5.test", "second-pass-9e5x");
  await expect(page.getByRole("banner")).toContainText("Password Org 9e5");
});

test("账单角色只看到组织页签", async ({ page }) => {
  const tenant = await stack.admin.post("/admin/tenants", { name: "Billing Org 1f7" });
  const invite = await stack.admin.post(`/admin/tenants/${tenant.id}/invites`, {
    email: "billing@billing-1f7.test",
    role: "billing",
  });
  await stack.tenant().post("/api/invites/accept", {
    token: invite.invite_token,
    password: "billing-pass-1f7x",
  });

  await login(page, "billing@billing-1f7.test", "billing-pass-1f7x");
  await expect(page.getByRole("banner")).toContainText("Billing Org 1f7");
  await expect(page.getByRole("tab")).toHaveText(["组织"]);
});
