import type { Page } from "@playwright/test";
import type { ApiClient } from "./stack";

type Client = Pick<ApiClient, "get" | "post" | "put" | "patch" | "createGroup">;

export async function groupModelWorld(stack: { admin: Client; tenant(): Pick<ApiClient, "post"> }) {
  const { admin } = stack;
  const owner = { email: "group-model-options@example.test", password: "Group-options-test-31" };
  const [first, second] = await Promise.all(["alpha", "beta"].map(async name => {
    const group = await admin.createGroup(name, name);
    const account = await admin.post("/admin/resources/provider_keys", {
      doc: { display_name: name, provider: "openai", api_key: "local-fixture", api_base: "http://127.0.0.1:1/v1", groups: [name] },
    });
    const model = await admin.post("/admin/public-models", { name: "chat-" + name });
    await admin.put(`/admin/provider-keys/${account.id}/channel-models/${model.id}`, { weight: 1 });
    return { group: group.name as string, groupId: group.id as string, account: account.id as string, model: model.name as string, id: model.id as string };
  }));
  if (!first || !second) throw Error("Two distinct groups required");
  const ungranted = await admin.post("/admin/public-models", { name: "chat-not-granted" });
  await admin.put(`/admin/provider-keys/${first.account}/channel-models/${ungranted.id}`, { weight: 1 });
  const signup = await stack.tenant().post("/api/signup", { org_name: "Group Models", ...owner });
  const tenantId = signup.tenant.id as string;
  const tenantPath = `/admin/tenants/${tenantId}`;
  await admin.post(tenantPath + "/approve");
  const tenant = await admin.get(tenantPath);
  await admin.patch(tenantPath, {
    model_scope: "selected", allowed_model_ids: [first.id, second.id], allowed_provider_groups: [first.group, second.group],
  }, tenant.version);
  return { first, second, ungranted: ungranted.name as string, tenantId, owner };
}

export async function loginForGroupModels(page: Page, url: string, login: { email: string; password: string }) {
  await page.goto(url);
  await page.getByLabel("邮箱", { exact: true }).fill(login.email);
  await page.getByLabel("口令", { exact: true }).fill(login.password);
  const loggedIn = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/login") && response.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  if ((await loggedIn).status() !== 200) throw Error("Login failed");
}
