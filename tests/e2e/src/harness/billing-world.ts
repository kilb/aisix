import {
  pollUntil,
  spawnManagedGateway,
  startControlPlane,
  type ControlPlaneStack,
  type Json,
  type ManagedGateway,
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
 * - `chat-bill` is priced at input 6 and output 20 USD per million tokens,
 *   so one call costs 1000 × 6 + 200 × 20 = 10 000 µ$ ($0.01).
 * - `embed-bill` is priced at input 10 (output 0), so one call costs
 *   10 000 µ$ too.
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
/** USD per million tokens. */
export const CHAT_CARD = { default: { base: { input: "6", output: "20" } } };
/** Embeddings report no output tokens; the card still names an output rate. */
export const EMBED_CARD = { default: { base: { input: "10", output: "0" } } };
/** What one call of either model costs, in µ$. */
export const CALL_MICRO_USD = 10_000;
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

export async function billingWorld(): Promise<BillingWorld> {
  const upstream = await startPlatformUpstream("billing-up-e2e", {
    usage: BILLING_USAGE,
    embeddingTokens: 1000,
  });
  let cp: ControlPlaneStack | undefined;
  let gateway: ManagedGateway | undefined;
  try {
    cp = await startControlPlane();
    const gw = await cp.issueGateway();
    gateway = await spawnManagedGateway(gw);
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
      const priced = await cp.admin.post("/admin/pricing/sell", {
        model_id: model.id,
        effective_from: new Date().toISOString(),
        card,
      });
      check(`pricing ${name}`, priced, 201);
    }

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
