import {
  pollUntil,
  spawnManagedGateway,
  startControlPlane,
  type ControlPlaneStack,
  type Json,
  type ManagedGateway,
  type ManagedGatewayOptions,
} from "./control-plane.js";
import { startPlatformUpstream, type PlatformUpstream } from "./cp-upstream.js";
import { ProxyClient } from "./proxy.js";

/**
 * The world every billing case starts from: one control-plane stack, one
 * managed gateway booted from its bundle, and one upstream account behind
 * two priced platform models.
 *
 * - The upstream reports 1000 prompt and 200 completion tokens on every
 *   chat, messages (bridged through chat) and responses answer, buffered or
 *   streamed, and 1000 prompt tokens on every embeddings answer.
 * - `chat-bill` has a baseline of input 6 and output 20 USD per million
 *   tokens; `embed-bill` of input 10 (output 0). `shared`, the group every
 *   seeded key names, charges 1 CNY per USD, so a call of either costs
 *   10 000 µ¥ (¥0.01).
 *
 * Everything is seeded through the control plane's API. A tenant is created
 * by [`BillingWorld.tenant`], granted both models, and its key is minted
 * last: the key authenticating is the gate that the tenant's documents,
 * its balance mark included, are in the gateway's snapshot.
 *
 * A harness module: importing it registers no test.
 */

/** What every chat-family answer of the upstream reports. */
export const BILLING_USAGE = { prompt: 1000, cached: 0, completion: 200 };
/** What `shared` charges: 1 CNY per USD, so a µ¥ figure is the µ$ one. */
export const SHARED_GROUP = "shared";
export const SHARED_RATE = "1";
/** A baseline card, in USD per million tokens. */
export const CHAT_CARD = { default: { base: { input: "6", output: "20" } } };
/** Embeddings report no output tokens; the card still names an output rate. */
export const EMBED_CARD = { default: { base: { input: "10", output: "0" } } };
/** What one call of either model costs, in µ¥. */
export const CALL_MICRO_CNY = 10_000;
/** @deprecated The same amount; the name predates the money path moving to CNY. */
export const CALL_MICRO_USD = CALL_MICRO_CNY;
export const CHAT_MODEL = "chat-bill";
export const EMBED_MODEL = "embed-bill";

export interface BillingTenant {
  id: string;
  /** Its key's plaintext. */
  key: string;
  keyId: string;
}

export interface TenantOptions {
  billingMode?: "prepaid" | "postpaid";
  /** Runs once the tenant is created and granted, before its key exists. */
  setup?: (tenantId: string) => Promise<void>;
}

export interface BillingWorld {
  cp: ControlPlaneStack;
  gateway: ManagedGateway;
  upstream: PlatformUpstream;
  models: { chat: Json; embed: Json };
  /** An active tenant as an operator creates one, granted both models, with one key. */
  tenant(name: string, opts?: TenantOptions): Promise<BillingTenant>;
  close(): Promise<void>;
}

function check(what: string, res: { status: number; text: string }, status: number): void {
  if (res.status !== status) throw new Error(`${what} answered ${res.status}: ${res.text}`);
}

export interface BillingWorldOptions {
  /**
   * How the gateway is booted, decided once the control plane runs: a case
   * that puts a relay in front of the control plane's gateway listener
   * starts it here and points `cp_base_url` at it.
   */
  gateway?: (cp: ControlPlaneStack) => Promise<ManagedGatewayOptions>;
}

export async function billingWorld(opts: BillingWorldOptions = {}): Promise<BillingWorld> {
  const upstream = await startPlatformUpstream("billing-up-e2e", {
    usage: BILLING_USAGE,
    embeddingTokens: 1000,
  });
  let cp: ControlPlaneStack | undefined;
  let gateway: ManagedGateway | undefined;
  try {
    cp = await startControlPlane();
    const gw = await cp.issueGateway();
    gateway = await spawnManagedGateway(gw, await opts.gateway?.(cp));
    await cp.admin.waitRegistered(gw.dpId);

    const account = await cp.admin.create("provider_keys", {
      display_name: "billing-account-e2e",
      provider: "openai",
      api_key: "sk-billing-upstream-e2e",
      api_base: `${upstream.baseUrl}/v1`,
      groups: ["shared"],
    });
    const chat = await cp.admin.create("models", {
      display_name: CHAT_MODEL,
      provider: "openai",
      model_name: "gpt-4o-mini",
      provider_key_id: account.id,
    });
    const embed = await cp.admin.create("models", {
      display_name: EMBED_MODEL,
      provider: "openai",
      model_name: "text-embedding-3-small",
      provider_key_id: account.id,
      embedding: { dimensions: 2 },
    });
    for (const [name, model, card] of [
      [CHAT_MODEL, chat, CHAT_CARD],
      [EMBED_MODEL, embed, EMBED_CARD],
    ] as const) {
      const priced = await cp.admin.post("/admin/pricing/baselines", {
        model_id: model.id,
        effective_from: new Date().toISOString(),
        card,
        note: "the billing world's card",
      });
      check(`pricing ${name}`, priced, 201);
    }
    // `shared`'s default rate, which every seeded key is charged at.
    await cp.admin.ensureSharedRate(SHARED_RATE);

    const stack = cp;
    const app = gateway;
    return {
      cp: stack,
      gateway: app,
      upstream,
      models: { chat, embed },
      async tenant(name, opts = {}) {
        const created = await stack.admin.post("/admin/tenants", {
          name,
          ...(opts.billingMode ? { billing_mode: opts.billingMode } : {}),
        });
        check(`creating tenant ${name}`, created, 201);
        const id: string = created.body.id;
        const granted = await stack.admin.patch(
          `/admin/tenants/${id}`,
          { allowed_model_ids: [chat.id, embed.id] },
          created.body.version,
        );
        check(`granting tenant ${name}`, granted, 200);
        await opts.setup?.(id);
        // The tenant's one assigned group is `shared`, by the first boot's
        // defaults, so its key takes it without naming it.
        const minted = await stack.admin.post(`/admin/tenants/${id}/keys`, { label: `${name} key` });
        check(`minting a key of ${name}`, minted, 201);
        const key: string = minted.body.plaintext;
        const proxy = new ProxyClient(app.proxyUrl, key);
        await pollUntil(`the key of ${name} to authenticate`, async () =>
          (await proxy.listModels()).status === 200 ? true : undefined,
        );
        return { id, key, keyId: minted.body.key.id };
      },
      async close() {
        await app.exit();
        await stack.stop();
        await upstream.close();
      },
    };
  } catch (err) {
    await gateway?.exit();
    await cp?.stop();
    await upstream.close();
    throw err;
  }
}

/** Ticks public model `publicModelId` on upstream account `providerKeyId`, which writes its documents. */
export async function createChannelModel(cp: ControlPlaneStack, providerKeyId: string, publicModelId: string, body: Record<string, unknown> = {}): Promise<Json> {
  const res = await cp.admin.put(`/admin/provider-keys/${providerKeyId}/channel-models/${publicModelId}`, body);
  check(`ticking a public model on an account`, res, 200);
  return res.body;
}
