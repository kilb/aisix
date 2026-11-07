import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "./stack";

/**
 * The Overview and Gateways tabs: the stack's gateway instance, issuing and
 * revoking gateway credentials, the one-time deployment bundle, and the
 * environment's rollout summary. Against the real control plane and the
 * real managed gateway, through the built `dist/`.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

/** A gateway reports by heartbeat every few seconds; the page refreshes on its own. */
const REPORTED_WITHIN_MS = 30_000;

async function login(page: Page, email: string, password: string): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令").fill(password);
  await page.getByRole("button", { name: "登录" }).click();
}

async function loginAsAdmin(page: Page): Promise<void> {
  await login(page, stack.operator.email, stack.operator.password);
}

async function openTab(page: Page, label: string): Promise<void> {
  await page.getByRole("tab", { name: label }).click();
}

async function credentialOf(name: string): Promise<Json> {
  const { items } = await stack.admin.get("/admin/gateways/credentials");
  const found = (items as Json[]).find((c) => c.name === name);
  expect(found, JSON.stringify(items)).toBeDefined();
  return found;
}

test("栈里的托管网关在线，并报告已应用的修订号", async ({ page }) => {
  await loginAsAdmin(page);
  await openTab(page, "网关");
  await expect(page.getByRole("row", { name: /stack-gw/ })).toContainText("在线");
  // The revision it reports, in the column of that name.
  const headers = await page.getByRole("table").getByRole("columnheader").allTextContents();
  const col = headers.findIndex((h) => h.trim() === "已应用修订");
  expect(col, headers.join(" | ")).toBeGreaterThanOrEqual(0);
  await expect(page.getByRole("row", { name: /stack-gw/ }).getByRole("cell").nth(col)).toHaveText(/^\d+$/);
});

test("签发凭据：部署包只显示一次，含镜像启动与平台模式变量，不含 etcd 用户", async ({ page }) => {
  await loginAsAdmin(page);
  await openTab(page, "网关");
  await page.getByRole("button", { name: "签发网关凭据" }).click();
  await page.getByLabel("名称").fill("gw-e2e");
  await page.getByRole("button", { name: "签发" }).click();
  const bundle = page.getByRole("region", { name: "部署包" });
  for (const k of ["AISIX_CONFIG_PATH=/etc/aisix/config.managed.yaml", "AISIX_ETCD__PREFIX=", "AISIX_MANAGED__CP_KEY_PEM=", "AISIX_PROXY__PLATFORM__ENABLED=true", "PLATFORM_ID_KEY="]) {
    await expect(bundle).toContainText(k);
  }
  await expect(bundle).not.toContainText("AISIX_ETCD__USER");
  // The key was on the page: what the check after the reload is measured against.
  await expect(bundle).toContainText("PRIVATE KEY");
  await expect(bundle).toContainText("私钥只显示这一次");
  await page.reload();
  await openTab(page, "网关");
  // The page did render the credentials, the new one among them.
  await expect(page.getByText("gw-e2e")).toBeVisible();
  await expect(page.getByText("BEGIN PRIVATE KEY")).toHaveCount(0);
  await expect(page.getByText("PRIVATE KEY")).toHaveCount(0);
});

test("复制全部拿到全部变量；私钥不进入浏览器存储、地址栏或控制台", async ({ page }) => {
  const consoleLines: string[] = [];
  page.on("console", (m) => consoleLines.push(m.text()));
  await page.context().grantPermissions(["clipboard-read", "clipboard-write"], { origin: stack.previewUrl });
  await loginAsAdmin(page);
  await openTab(page, "网关");
  await page.getByRole("button", { name: "签发网关凭据" }).click();
  await page.getByLabel("名称").fill("gw-leak-e2e");
  await page.getByRole("button", { name: "签发" }).click();
  const bundle = page.getByRole("region", { name: "部署包" });
  await expect(bundle).toContainText("PRIVATE KEY");
  // The key's own body, not just its armour.
  const shown = (await bundle.textContent()) ?? "";
  const body = /-----BEGIN [A-Z ]*PRIVATE KEY-----\s*([A-Za-z0-9+/=]{40})/.exec(shown)?.[1];
  expect(body, "the bundle shows a PEM private key").toBeDefined();

  await page.getByRole("button", { name: "复制全部" }).click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toContain("AISIX_PROXY__PLATFORM__ENABLED=true");
  const copied = await page.evaluate(() => navigator.clipboard.readText());
  for (const k of ["AISIX_CONFIG_PATH=/etc/aisix/config.managed.yaml", "AISIX_ETCD__PREFIX=", "AISIX_MANAGED__CP_KEY_PEM=", "PLATFORM_ID_KEY=", body!]) {
    expect(copied).toContain(k);
  }

  const stored = await page.evaluate(() => {
    const dump = (s: Storage) => {
      const out: string[] = [];
      for (let i = 0; i < s.length; i++) {
        const k = s.key(i) ?? "";
        out.push(`${k}=${s.getItem(k) ?? ""}`);
      }
      return out.join("\n");
    };
    return `${dump(localStorage)}\n${dump(sessionStorage)}`;
  });
  for (const needle of ["PRIVATE KEY", body!]) {
    expect(stored).not.toContain(needle);
    expect(page.url()).not.toContain(needle);
    expect(consoleLines.join("\n")).not.toContain(needle);
  }
});

test("吊销后凭据显示为已吊销", async ({ page }) => {
  await stack.admin.post("/admin/gateways/credentials", { name: "gw-revoke-e2e" });
  await loginAsAdmin(page);
  await openTab(page, "网关");
  const item = page.getByRole("listitem").filter({ hasText: "gw-revoke-e2e" });
  await expect(item).toBeVisible();
  await expect(item).not.toContainText("已吊销");

  // Declining the confirmation revokes nothing.
  let asked = "";
  page.once("dialog", (d) => {
    asked = d.message();
    void d.dismiss();
  });
  await item.getByRole("button", { name: "吊销" }).click();
  await expect.poll(() => asked).toContain("gw-revoke-e2e");
  expect((await credentialOf("gw-revoke-e2e")).revoked_at).toBeNull();
  await expect(item).not.toContainText("已吊销");

  page.once("dialog", (d) => void d.accept());
  await item.getByRole("button", { name: "吊销" }).click();
  await expect(item).toContainText("已吊销");
  expect((await credentialOf("gw-revoke-e2e")).revoked_at).not.toBeNull();
  // The control plane's record, not the page's memory.
  await page.reload();
  await openTab(page, "网关");
  await expect(page.getByRole("listitem").filter({ hasText: "gw-revoke-e2e" })).toContainText("已吊销");
});

test("被拒绝的资源在概览里按名字列出，并带网关给出的原因", async ({ page }) => {
  await loginAsAdmin(page);
  await openTab(page, "概览");
  // Attached to a model nobody calls, so only the guardrail's build is in question.
  const account = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: "acct-rej-e2e",
      provider: "openai",
      api_key: "sk-rej-upstream-e2e",
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups: ["shared"],
    },
  });
  const model = await stack.admin.post("/admin/resources/models", {
    doc: { display_name: "model-rej-e2e", provider: "openai", model_name: "gpt-4o-mini", provider_key_id: account.id },
  });
  const guardrail = await stack.admin.post("/admin/resources/guardrails", {
    doc: { name: "guard-bad-e2e", kind: "keyword", hook_point: "input", patterns: [{ kind: "regex", value: "(" }] },
  });
  await stack.admin.post("/admin/resources/guardrail_attachments", {
    doc: { guardrail_id: guardrail.id, scope_type: "model", scope_id: model.id, priority: 100 },
  });

  // The open page picks the rejection up without a reload.
  const item = page.getByRole("listitem").filter({ hasText: "guard-bad-e2e" });
  await expect(item).toBeVisible({ timeout: REPORTED_WITHIN_MS });
  const summary = await stack.admin.get("/admin/rollout");
  const rejected = (summary.rejected as Json[]).find((r) => r.id === guardrail.id);
  expect(rejected, JSON.stringify(summary)).toBeDefined();
  const reason: string = rejected.errors[0]?.error ?? "";
  expect(reason.length).toBeGreaterThan(0);
  await expect(item).toContainText(reason);

  // A guardrail has no form of its own: it is edited on the generic page.
  await item.getByRole("button").click();
  await expect(page.locator('[data-tab="resources"]')).toHaveAttribute("aria-selected", "true");
});

test("概览显示环境 ID 与控制面版本", async ({ page }) => {
  const env = await stack.admin.get("/admin/environment");
  await loginAsAdmin(page);
  await openTab(page, "概览");
  await expect(page.locator("#tab-overview")).toContainText(env.env_id);
  await expect(page.locator("#tab-overview")).toContainText(env.cp_version);
});

test("非管理员看得到凭据，但没有签发与吊销", async ({ page }) => {
  const email = "operator-gw-e2e@e2e.local";
  const password = "operator-gw-pass-4d1";
  await stack.admin.post("/admin/operators", { email, role: "operator", password });
  await login(page, email, password);
  await openTab(page, "网关");
  await expect(page.getByRole("listitem").filter({ hasText: "stack-gw" })).toBeVisible();
  await expect(page.getByRole("button", { name: "签发网关凭据" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "吊销" })).toHaveCount(0);
});
