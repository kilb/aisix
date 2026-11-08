import { afterAll, beforeAll, describe, expect, test } from "vitest";

import {
  billingWorld,
  CALL_MICRO_USD,
  CHAT_MODEL,
  EMBED_MODEL,
  type BillingTenant,
  type BillingWorld,
} from "../harness/billing-world.js";
import { pollUntil, type Json } from "../harness/control-plane.js";
import { callGateway } from "../harness/cp-upstream.js";
import { scrapeMetrics, sumMetric } from "../harness/metrics.js";

// E2E: the money check, across both planes. An operator credits a prepaid
// tenant, and the tenant's key calls every first-phase endpoint, buffered
// and streamed, through a real gateway. Every call's usage reaches the
// tenant's ledger at exactly the price table's amount, and the operator's
// reconciliation finds no drift. Only the upstream model is a stand-in.

const CREDIT_USD = "5.00";
const CREDIT_MICRO_USD = 5_000_000;
/** How many times each endpoint is called, buffered and streamed. */
const ROUNDS = 3;
/** The gateway's own count of the usage events it could not deliver. */
const DROPS = "aisix_usage_event_drops_total";
/** How long a call's usage may take to reach the ledger. */
const WITHIN_MS = 30_000;

/** Every first-phase endpoint, buffered and streamed; `tag` keeps each body distinct. */
const firstPhaseCalls = (tag: string): Array<[string, unknown]> => [
  ["/v1/chat/completions", { model: CHAT_MODEL, messages: [{ role: "user", content: `chat ${tag}` }] }],
  [
    "/v1/chat/completions",
    { model: CHAT_MODEL, stream: true, messages: [{ role: "user", content: `streamed chat ${tag}` }] },
  ],
  ["/v1/messages", { model: CHAT_MODEL, max_tokens: 32, messages: [{ role: "user", content: `message ${tag}` }] }],
  [
    "/v1/messages",
    {
      model: CHAT_MODEL,
      max_tokens: 32,
      stream: true,
      messages: [{ role: "user", content: `streamed message ${tag}` }],
    },
  ],
  ["/v1/responses", { model: CHAT_MODEL, input: `response ${tag}` }],
  ["/v1/responses", { model: CHAT_MODEL, input: `streamed response ${tag}`, stream: true }],
  ["/v1/embeddings", { model: EMBED_MODEL, input: `embedding ${tag}` }],
];

describe("the ledger matches the price table", () => {
  let world: BillingWorld | undefined;
  let tenant: BillingTenant;

  /** Every page of `path`, a list paged on `before`. */
  const everyPage = async (path: string): Promise<Json[]> => {
    const items: Json[] = [];
    let before: number | null = null;
    for (;;) {
      const sep = path.includes("?") ? "&" : "?";
      const res = await world!.cp.admin.get(`${path}${before === null ? "" : `${sep}before=${before}`}`);
      expect(res.status, res.text).toBe(200);
      items.push(...(res.body.items as Json[]));
      before = res.body.next_before;
      if (before === null) return items;
    }
  };
  /** The gateway's dropped usage events so far, every reason summed. */
  const drops = async (): Promise<number> => sumMetric(await scrapeMetrics(world!.gateway.metricsUrl), DROPS);

  beforeAll(async () => {
    world = await billingWorld();
    tenant = await world.tenant("Money Check E2E Co", {
      setup: async (id) => {
        const credited = await world!.cp.admin.post(`/admin/tenants/${id}/credits`, {
          amount_usd: CREDIT_USD,
          reason: "对账核验 e2e",
        });
        expect(credited.status, credited.text).toBe(201);
      },
    });
  });

  afterAll(async () => {
    await world?.close();
  });

  test(
    "Every first-phase endpoint's usage, streaming and not, reaches the ledger at exactly the price-table amount",
    async () => {
      const dropsBefore = await drops();
      /** The path each request id was sent to, in sending order. */
      const sent = new Map<string, string>();
      const send = async (path: string, body: unknown): Promise<string> => {
        const answer = await callGateway(world!.gateway.proxyUrl, tenant.key, path, body);
        const what = `${path} ${JSON.stringify(body)}: ${answer.status} ${answer.text}`;
        expect(answer.status, what).toBe(200);
        const requestId = answer.headers.get("x-aisix-request-id");
        expect(requestId, what).toBeTruthy();
        expect(sent.has(requestId!), `request id ${requestId} answered twice`).toBe(false);
        sent.set(requestId!, path);
        return requestId!;
      };

      for (let round = 1; round <= ROUNDS; round++) {
        for (const [path, body] of firstPhaseCalls(`round ${round}`)) await send(path, body);
      }
      expect(sent.size).toBe(21);

      // The barrier. The gateway delivers usage in the order it was made, so
      // once the barrier's entry is in the ledger, every earlier call's usage
      // has been delivered or dropped.
      const barrier = await send("/v1/chat/completions", {
        model: CHAT_MODEL,
        messages: [{ role: "user", content: "the barrier" }],
      });
      const progress = async () => `drops so far ${(await drops()) - dropsBefore}`;
      let note = "";
      const barrierEvent: string = await pollUntil(
        `the usage row of the barrier ${barrier}`,
        async () => {
          const res = await world!.cp.admin.get(`/admin/usage/events?request_id=${encodeURIComponent(barrier)}`);
          expect(res.status, res.text).toBe(200);
          note = await progress();
          return (res.body.items as Json[])[0]?.event_id;
        },
        { timeoutMs: WITHIN_MS, describe: () => note },
      );
      await pollUntil(
        `the ledger entry of the barrier ${barrier}`,
        async () => {
          const entries = await everyPage(`/admin/tenants/${tenant.id}/billing/ledger?kind=usage&limit=500`);
          note = await progress();
          return entries.some((e) => e.event_id === barrierEvent) ? true : undefined;
        },
        { timeoutMs: WITHIN_MS, describe: () => note },
      );

      const dropped = (await drops()) - dropsBefore;
      if (dropped !== 0) {
        // A drop is a product signal, never an expected cost of the run.
        console.warn(`the gateway dropped ${dropped} usage event(s) during the money check`);
      }
      const expected = (sent.size - dropped) * CALL_MICRO_USD;

      // The key's stored usage rows, and the tenant's usage entries.
      const rows = await everyPage(`/admin/usage/events?limit=500&api_key_id=${tenant.keyId}`);
      const entries = await everyPage(`/admin/tenants/${tenant.id}/billing/ledger?kind=usage&limit=500`);
      const pathOf = new Map(rows.map((r) => [r.event_id as string, sent.get(r.request_id) ?? `unsent ${r.request_id}`]));
      const keyEntries = entries.filter((e) => pathOf.has(e.event_id));
      // What each endpoint was debited, for the message of any mismatch below.
      const byPath: Record<string, number[]> = {};
      for (const e of keyEntries) (byPath[pathOf.get(e.event_id)!] ??= []).push(e.amount_micro_usd);
      const books = `dropped ${dropped}, rows ${rows.length}, entries ${entries.length}, debits ${JSON.stringify(byPath)}`;

      expect.soft(rows.length, `every call's usage row is stored: ${books}`).toBe(sent.size - dropped);
      expect
        .soft(
          entries.map((e) => e.event_id).sort(),
          `one usage entry for each of the key's rows, and none other: ${books}`,
        )
        .toEqual(rows.map((r) => r.event_id).sort());
      expect
        .soft(
          keyEntries.reduce((sum, e) => sum + e.amount_micro_usd, 0),
          `the key's usage entries add up to the price table's amount: ${books}`,
        )
        .toBe(-expected);

      const account = await world!.cp.admin.get(`/admin/tenants/${tenant.id}/billing`);
      expect(account.status, account.text).toBe(200);
      expect.soft(account.body.balance_micro_usd, account.text).toBe(CREDIT_MICRO_USD - expected);

      const reconciled = await world!.cp.admin.get(`/admin/billing/reconciliation?tenant_id=${tenant.id}`);
      expect(reconciled.status, reconciled.text).toBe(200);
      expect.soft(reconciled.body.tenant_id, reconciled.text).toBe(tenant.id);
      expect.soft(reconciled.body.mismatches, reconciled.text).toEqual([]);
      expect.soft(reconciled.body.charged_without_entry, reconciled.text).toBe(0);
    },
    120_000,
  );
});
