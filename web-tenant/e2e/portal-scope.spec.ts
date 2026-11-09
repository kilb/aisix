import { expect, test } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, fundTenant, startStack, type Json, type Stack } from "./stack";

/**
 * An organisation's model scope, as the tenant portal sees it: a new
 * organisation reaches every public model, a public model published after it
 * signed up is reachable with no edit on either side, and narrowing to a
 * selected list that names nothing leaves the gateway refusing every model.
 *
 * Against the real stack: the public models, their accounts, their prices and
 * the narrowing all go through the real `/admin`, the organisation's own
 * requests through its real `/api`, and the last judgement is made by the
 * real gateway.
 */

let stack: Stack;

const PASSWORD = "portal-scope-pass-5a1";
/** How long a grant change may take to reach the gateway. */
const ROLLOUT_MS = 30_000;
const KEY_READY_MS = 60_000;
const PRICE_CARD = { default: { base: { input: "4", output: "20" } } };

/** The public model every organisation here reaches, and the one published later. */
const FIRST = "scope-pub-first-5a1";
const LATER = "scope-pub-later-5a1";

/** The upstream account the public models are served by. */
let accountId: string;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
  const account = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: "acct-scope-5a1",
      provider: "openai",
      api_key: "sk-acct-scope-5a1-upstream",
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups: ["shared"],
    },
  });
  accountId = account.id as string;
  await publish(FIRST);
});

test.afterAll(async () => {
  await stack?.stop();
});

/** Creates public model `name`, serves it from the account, and prices it. */
async function publish(name: string): Promise<string> {
  const made = await stack.admin.post("/admin/public-models", { name });
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${made.id}`, {});
  await stack.admin.post("/admin/pricing/baselines", {
    model_id: made.id,
    effective_from: new Date(Date.now() - 4_000).toISOString(),
    card: PRICE_CARD,
    note: "模型范围 e2e 的价目卡 5a1",
  });
  // The group an organisation lands in charges nothing until a rate names it,
  // and an unpriced model is left out of the list rather than shown as free.
  await stack.admin.post(`/admin/provider-groups/${await groupIdOf("shared")}/rates`, {
    model_id: made.id,
    cny_per_usd: "1",
    effective_from: new Date(Date.now() - 4_000).toISOString(),
    note: "模型范围 e2e 的倍率 5a1",
  });
  return made.id as string;
}

/** The id of the live provider group whose routing name is `name`. */
async function groupIdOf(name: string): Promise<string> {
  const listed = await stack.admin.get("/admin/provider-groups");
  const found = listed.items.find((g: { name: string }) => g.name === name);
  if (!found) throw new Error(`no provider group named ${name}`);
  return found.id as string;
}

/** The id of the live public model named `name`, as the operators see it. */
async function idOf(name: string): Promise<string> {
  const listed = await stack.admin.get("/admin/public-models");
  const found = listed.items.find((m: { name: string }) => m.name === name);
  if (!found) throw new Error(`no public model named ${name}`);
  return found.id as string;
}

/** An organisation signed up by `email`, approved and funded; its id. */
async function activeOrg(name: string, email: string): Promise<string> {
  const signedUp = await stack
    .tenant()
    .post("/api/signup", { org_name: name, email, password: PASSWORD });
  const id: string = signedUp.tenant.id;
  await stack.admin.post(`/admin/tenants/${id}/approve`);
  await fundTenant(stack.admin, id);
  return id;
}

/** The organisation's own model ids, from `/api/models`, sorted. */
async function modelsOf(email: string): Promise<string[]> {
  const client = stack.tenant();
  await client.post("/api/login", { email, password: PASSWORD });
  const body: Json = await client.get("/api/models");
  return (body.items as { id: string }[]).map((m) => m.id).sort();
}

/** The status the gateway answers a chat to `model` on `key`. */
async function chatStatus(key: string, model: string): Promise<number> {
  const res = await fetch(`${stack.proxyUrl}/v1/chat/completions`, {
    method: "POST",
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    body: JSON.stringify({ model, messages: [{ role: "user", content: "hi" }] }),
  });
  await res.arrayBuffer();
  return res.status;
}

/** A key of the organisation signed up as `email`, by label; its plaintext. */
async function mintKey(email: string, label: string): Promise<string> {
  const client = stack.tenant();
  await client.post("/api/login", { email, password: PASSWORD });
  const minted: Json = await client.post("/api/keys", { label });
  return minted.plaintext as string;
}

/** Waits until the gateway lists `name` for `key`, or says what it answered. */
async function serves(key: string, name: string): Promise<void> {
  let last = "";
  const until = Date.now() + KEY_READY_MS;
  while (Date.now() < until) {
    // The proxy listener may not be bound yet: that is a wait, not a failure.
    const answer = await fetch(`${stack.proxyUrl}/v1/models`, {
      headers: { authorization: `Bearer ${key}` },
    }).then(
      async (res) => `${res.status} ${await res.text()}`,
      (e: unknown) => `unreachable (${String(e)})`,
    );
    last = answer;
    if (answer.startsWith("200")) {
      const body = JSON.parse(answer.slice(4)) as { data?: { id: string }[] };
      if ((body.data ?? []).some((m) => m.id === name)) return;
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  throw new Error(`the gateway never listed ${name} for ${key}: ${last}`);
}

async function login(page: import("@playwright/test").Page, email: string): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令", { exact: true }).fill(PASSWORD);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await expect(page.getByRole("button", { name: "登出" })).toBeVisible();
}

test("新组织默认使用全部公共模型，签到时之后发布的公共模型也自动可用", async ({ page }) => {
  // The first case of the file pays for the stack's own start on this
  // worker, as every spec here does.
  test.setTimeout(STACK_START_TIMEOUT_MS);
  const email = "owner@scope-first-5a1.test";
  const tenant = await activeOrg("范围一 5a1", email);
  // Published after the organisation signed up, with no edit on either side.
  const later = await publish(LATER);
  const first = await idOf(FIRST);

  expect(await modelsOf(email)).toEqual([first, later].sort());
  // The portal's prices page names both, since both are priced.
  await login(page, email);
  await page.getByRole("tab", { name: "价格", exact: true }).click();
  const table = page.getByRole("table", { name: "模型单价" });
  await expect(table).toContainText(FIRST);
  await expect(table).toContainText(LATER);

  // The gateway serves both, on a key minted before the second one existed.
  const key = await mintKey(email, "scope-first-5a1");
  await serves(key, FIRST);
  await serves(key, LATER);
  expect(await chatStatus(key, FIRST)).toBe(200);
  expect(await chatStatus(key, LATER)).toBe(200);

  // The operator's view says why: the scope is every public model.
  const view = await stack.admin.get(`/admin/tenants/${tenant}`);
  expect(view.model_scope).toBe("all");
});

test("改为指定模型并取消全部勾选后，网关拒绝所有模型", async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  const email = "owner@scope-none-5a1.test";
  const tenant = await activeOrg("范围二 5a1", email);
  // Minted while the scope is still `all`, so its document is the concrete
  // list an all-scope tenant gets.
  const key = await mintKey(email, "scope-none-5a1");
  await serves(key, FIRST);
  expect(await chatStatus(key, FIRST)).toBe(200);

  // Selected, with nothing listed: the organisation reaches no model at all.
  const current = await stack.admin.get(`/admin/tenants/${tenant}`);
  const narrowed = await stack.admin.patch(
    `/admin/tenants/${tenant}`,
    { model_scope: "selected", allowed_model_ids: [] },
    current.version,
  );
  expect(narrowed.model_scope).toBe("selected");
  expect(narrowed.allowed_model_ids).toEqual([]);

  expect(await modelsOf(email)).toEqual([]);
  // The same key, and the gateway refuses the model it just served.
  await expect
    .poll(
      async () => {
        const status = await chatStatus(key, FIRST);
        // 403 "forbidden" when the name still resolves to a model the key
        // may not use, 404 when the key's list no longer reaches it at all:
        // both are the refusal. Anything else is not, and is reported.
        return status === 403 || status === 404 ? "refused" : String(status);
      },
      {
        timeout: ROLLOUT_MS,
        message: "the gateway refuses a model the organisation no longer reaches",
      },
    )
    .toBe("refused");
});
