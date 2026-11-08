import { randomUUID } from "node:crypto";
import type { ApiClient, Json, Stack } from "./stack";

/**
 * Billing worlds: a funded organisation with real usage, set up only through
 * the real `/admin` and the organisation's own `/api`, with the real gateway
 * in front of the upstream stand-in. Nothing here writes to a database.
 *
 * The Dashboard's copy of the tenant portal's `world.ts`; `seedTenant` is its `seedBillingWorld`.
 *
 * The catalogue (one upstream account, the priced model and, on request, the
 * unpriced one and the cache policy) is created once per stack, so a spec
 * can seed several organisations; each call makes its own organisation.
 */

/**
 * What one non-streaming chat costs the organisation, in micro-dollars. The
 * stand-in reports 5 prompt and 3 completion tokens, and the model's price
 * card is `{"default":{"base":{"input":"1000","output":"2000"}}}` (USD per
 * million tokens, so 1000 and 2000 micro-dollars per token): 5 x 1000 + 3 x
 * 2000 = 11 000.
 */
export const CALL_MICRO_USD = 11_000;

const PRICE_CARD = { default: { base: { input: "1000", output: "2000" } } };
/** How far ahead of the clock the price starts: past the control plane's own reading of "now". */
const PRICE_LEAD_MS = 3_000;
const PASSWORD = "billing-world-pass-3k8";
/** How long the new key may take to reach the gateway. */
const KEY_READY_MS = 60_000;

export interface Login {
  email: string;
  password: string;
}

export interface BillingWorld {
  tenantId: string;
  tenantName: string;
  owner: Login;
  members: (Login & { role: string; id: string })[];
  model: { id: string; name: string };
  laterModel?: { id: string; name: string };
  key: { id: string; plaintext: string };
}

export interface WorldOptions {
  mode?: "prepaid" | "postpaid";
  creditLimitUsd?: string;
  /** Posted as an operator credit. Left out, a prepaid organisation gets 10.00 so the gateway admits it; `"0"` posts nothing. */
  creditUsd?: string;
  members?: ("member" | "billing")[];
  /** Also create `chat-ui-later`, granted but with no price. */
  unpricedModel?: boolean;
  /** Add a `team` cache policy on `chat-ui`. */
  cachePolicy?: boolean;
}

interface Catalogue {
  account: Promise<string>;
  priced: Promise<{ id: string; name: string }>;
  unpriced?: Promise<{ id: string; name: string }>;
  cache?: Promise<void>;
}

const catalogues = new WeakMap<Stack, Catalogue>();

function catalogueOf(stack: Stack, opts: WorldOptions): Catalogue {
  let c = catalogues.get(stack);
  if (!c) {
    const account = stack.admin
      .post("/admin/resources/provider_keys", {
        doc: {
          display_name: "acct-ui-7c1",
          provider: "openai",
          api_key: "sk-acct-ui-7c1-upstream",
          api_base: `${stack.mockUpstreamUrl}/v1`,
          groups: ["shared", "grp-ui-5d2"],
        },
      })
      .then((r: Json) => r.id as string);
    const priced = account.then(async (accountId) => {
      const model = await stack.admin.post("/admin/resources/models", {
        doc: { display_name: "chat-ui", provider: "openai", model_name: "upstream-mdl-ui-9f3", provider_key_id: accountId },
      });
      // A price cannot start in the past, so it starts a moment ahead, and the
      // world waits that moment out: no call may be made before it is in effect.
      const startsAt = Date.now() + PRICE_LEAD_MS;
      await stack.admin.post("/admin/pricing/sell", {
        model_id: model.id,
        effective_from: new Date(startsAt).toISOString(),
        card: PRICE_CARD,
      });
      await sleep(Math.max(0, startsAt - Date.now()) + 250);
      return { id: model.id as string, name: "chat-ui" };
    });
    c = { account, priced };
    catalogues.set(stack, c);
  }
  if (opts.unpricedModel && !c.unpriced) {
    const accountP = c.account;
    c.unpriced = accountP.then(async (accountId) => {
      const model = await stack.admin.post("/admin/resources/models", {
        doc: {
          display_name: "chat-ui-later",
          provider: "openai",
          model_name: "upstream-mdl-ui-later-4b8",
          provider_key_id: accountId,
        },
      });
      return { id: model.id as string, name: "chat-ui-later" };
    });
  }
  if (opts.cachePolicy && !c.cache) {
    const pricedP = c.priced;
    c.cache = pricedP.then(async (m) => {
      await stack.admin.post("/admin/resources/cache_policies", {
        doc: { name: "cache-ui-2e6", scope: "team", applies_to_model_id: m.id, ttl_seconds: 300 },
      });
    });
  }
  return c;
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

/**
 * The group the Dashboard's internal-key fixtures mint into, created once per
 * stack and cached: a key names one group by id, the platform's own tenant has
 * no assignment list to take it from, and the operator API's group list
 * answers names.
 */
const internalGroups = new WeakMap<Stack, Promise<string>>();
export function internalGroupId(stack: Stack): Promise<string> {
  let group = internalGroups.get(stack);
  if (!group) {
    group = stack.admin
      .post("/admin/provider-groups", {
        name: "grp-internal-ui-7c1",
        display_name: "平台自用 7c1",
        description: "e2e 内部租户的密钥",
        default_cny_per_usd: "7.2",
      })
      .then((created: Json) => created.id as string);
    internalGroups.set(stack, group);
  }
  return group;
}

/** A `/api` client signed in as `login`. */
async function signedIn(stack: Stack, login: Login): Promise<ApiClient> {
  const client = stack.tenant();
  await client.post("/api/login", login);
  return client;
}

/**
 * An organisation with an active owner, the granted models and, by default,
 * a balance; the owner's key last, once the gateway answers with it.
 */
export async function seedTenant(stack: Stack, opts: WorldOptions = {}): Promise<BillingWorld> {
  const suffix = randomUUID().slice(0, 8);
  const catalogue = catalogueOf(stack, opts);
  const model = await catalogue.priced;
  const laterModel = catalogue.unpriced ? await catalogue.unpriced : undefined;
  if (catalogue.cache) await catalogue.cache;

  const tenantName = `Billing UI Org ${suffix}`;
  const owner: Login = { email: `owner-${suffix}@billing-ui.test`, password: PASSWORD };
  const signedUp = await stack.tenant().post("/api/signup", { org_name: tenantName, email: owner.email, password: owner.password });
  const tenantId: string = signedUp.tenant.id;
  await stack.admin.post(`/admin/tenants/${tenantId}/approve`);
  const current = await stack.admin.get(`/admin/tenants/${tenantId}`);
  await stack.admin.patch(
    `/admin/tenants/${tenantId}`,
    {
      allowed_model_ids: [model.id, ...(laterModel ? [laterModel.id] : [])],
      ...(opts.mode ? { billing_mode: opts.mode } : {}),
    },
    current.version,
  );
  if (opts.creditLimitUsd !== undefined) {
    await stack.admin.patch(`/admin/tenants/${tenantId}/billing`, { credit_limit_usd: opts.creditLimitUsd });
  }
  const credit = opts.creditUsd ?? (opts.mode === "postpaid" ? "0" : "10.00");
  if (credit !== "0") {
    await stack.admin.post(`/admin/tenants/${tenantId}/credits`, { amount_usd: credit, reason: "billing world funding" });
  }

  const members: BillingWorld["members"] = [];
  for (const [i, role] of (opts.members ?? []).entries()) {
    const email = `${role}-${i}-${suffix}@billing-ui.test`;
    const invite = await stack.admin.post(`/admin/tenants/${tenantId}/invites`, { email, role });
    const session = await stack.tenant().post("/api/invites/accept", {
      token: invite.invite_token,
      password: PASSWORD,
    });
    members.push({ email, password: PASSWORD, role, id: session.member.id });
  }

  const ownerApi = await signedIn(stack, owner);
  const created = await ownerApi.post("/api/keys", { label: "world-owner-key" });
  const key = { id: created.key.id as string, plaintext: created.plaintext as string };
  await gatewayAdmits(stack, key.plaintext);
  return { tenantId, tenantName, owner, members, model, ...(laterModel ? { laterModel } : {}), key };
}

/** Waits until the gateway answers `GET /v1/models` 200 for `plaintext`: the key has reached it. */
async function gatewayAdmits(stack: Stack, plaintext: string): Promise<void> {
  const deadline = Date.now() + KEY_READY_MS;
  let last = 0;
  for (;;) {
    const res = await fetch(`${stack.proxyUrl}/v1/models`, { headers: { authorization: `Bearer ${plaintext}` } });
    await res.arrayBuffer();
    last = res.status;
    if (last === 200) return;
    if (Date.now() >= deadline) throw new Error(`the gateway still answers ${last} to the new key after ${KEY_READY_MS} ms`);
    await sleep(250);
  }
}

/** The status of one non-streaming chat through the gateway (the stand-in does not stream). */
export async function chat(
  stack: Stack,
  plaintext: string,
  model: string,
  opts: { requestId?: string } = {},
): Promise<number> {
  const headers: Record<string, string> = { authorization: `Bearer ${plaintext}`, "content-type": "application/json" };
  if (opts.requestId) headers["x-aisix-request-id"] = opts.requestId;
  const res = await fetch(`${stack.proxyUrl}/v1/chat/completions`, {
    method: "POST",
    headers,
    body: JSON.stringify({ model, messages: [{ role: "user", content: "hi" }] }),
  });
  await res.arrayBuffer();
  return res.status;
}

/** The organisation's own `/api` client, signed in as its owner, for setup steps the Dashboard test needs the tenant side of. */
export function tenantClient(stack: Stack, world: BillingWorld): Promise<ApiClient> {
  return signedIn(stack, world.owner);
}

/**
 * A key of the internal tenant (the platform's own), which the gateway admits
 * without a balance or a limit: for load that must not be refused or debited
 * to a customer.
 */
export async function internalKey(stack: Stack): Promise<string> {
  const { items } = await stack.admin.get("/admin/tenants");
  const internal = items.find((t: { is_internal?: boolean }) => t.is_internal);
  if (!internal) throw new Error("the stack has no internal tenant");
  // The platform's own keys name one group too, and the tenant has no
  // assignment list to take it from.
  const created = await stack.admin.post(`/admin/tenants/${internal.id}/keys`, {
    label: `internal-load-${randomUUID().slice(0, 6)}`,
    allowed_models: ["*"],
    group_id: await internalGroupId(stack),
  });
  await gatewayAdmits(stack, created.plaintext);
  return created.plaintext;
}

/** Mints a key as `login`, a member of the world's organisation. */
export async function memberKey(stack: Stack, world: BillingWorld, login: Login): Promise<{ id: string; plaintext: string }> {
  void world;
  const api = await signedIn(stack, login);
  const created = await api.post("/api/keys", { label: `member-key-${randomUUID().slice(0, 6)}` });
  return { id: created.key.id, plaintext: created.plaintext };
}

/** An operator approves top-up `id`. */
export async function approveTopup(stack: Stack, id: string): Promise<Json> {
  return stack.admin.post(`/admin/billing/topups/${id}/approve`);
}

/** An operator rejects top-up `id` with `reason`, which the organisation reads. */
export async function rejectTopup(stack: Stack, id: string, reason: string): Promise<Json> {
  return stack.admin.post(`/admin/billing/topups/${id}/reject`, { reason });
}

/**
 * When the clock is within `marginMs` of 00:00 UTC on either side, waits
 * until `marginMs` past it, so a test that names "today" never straddles two
 * days. It never skips.
 */
export async function waitClearOfUtcMidnight(marginMs = 120_000): Promise<void> {
  const DAY = 86_400_000;
  const sinceMidnight = Date.now() % DAY;
  if (sinceMidnight < marginMs) await sleep(marginMs - sinceMidnight);
  else if (DAY - sinceMidnight < marginMs) await sleep(DAY - sinceMidnight + marginMs);
}

/** Ticks public model `publicModelId` on upstream account `providerKeyId`, which writes its documents. */
export async function createChannelModel(stack: Stack, providerKeyId: string, publicModelId: string, body: Record<string, unknown> = {}): Promise<Json> {
  return stack.admin.put(`/admin/provider-keys/${providerKeyId}/channel-models/${publicModelId}`, body);
}
