import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { billingWorld, CHAT_MODEL, EMBED_MODEL, type BillingTenant, type BillingWorld } from "../harness/billing-world.js";
import { callGateway } from "../harness/cp-upstream.js";

// E2E: money blocks a tenant at the gateway, across both planes. A prepaid
// tenant an operator creates has a balance of 0, which is no money at all,
// so from its first request the gateway refuses it, on every endpoint
// family, streamed or not, with a billing error about its own organization,
// and nothing reaches the upstream.

/** What a prepaid tenant out of money is told. */
const PREPAID_MESSAGE =
  "Account balance exhausted. Ask an owner or billing member of your organization to top up.";

type Envelope = "openai" | "anthropic";

const chat = (stream: boolean) => ({
  model: CHAT_MODEL,
  stream,
  messages: [{ role: "user", content: "what does this cost?" }],
});
const messages = (stream: boolean) => ({
  model: CHAT_MODEL,
  max_tokens: 32,
  stream,
  messages: [{ role: "user", content: "what does this cost?" }],
});
const responses = (stream: boolean) => ({ model: CHAT_MODEL, input: "what does this cost?", stream });

describe("a tenant's balance blocks it at the gateway", () => {
  let world: BillingWorld | undefined;
  let tenant: BillingTenant;

  beforeAll(async () => {
    world = await billingWorld();
    tenant = await world.tenant("Balance E2E Co");
  });

  afterAll(async () => {
    await world?.close();
  });

  test("A prepaid tenant with no balance is refused with its own billing error on every first-phase endpoint", async () => {
    const calls: Array<[string, unknown, Envelope]> = [
      ["/v1/chat/completions", chat(false), "openai"],
      ["/v1/chat/completions", chat(true), "openai"],
      ["/v1/messages", messages(false), "anthropic"],
      ["/v1/messages", messages(true), "anthropic"],
      [
        "/v1/messages/count_tokens",
        { model: CHAT_MODEL, messages: [{ role: "user", content: "count these" }] },
        "anthropic",
      ],
      ["/v1/responses", responses(false), "openai"],
      ["/v1/responses", responses(true), "openai"],
      ["/v1/embeddings", { model: EMBED_MODEL, input: "embed this" }, "openai"],
    ];
    for (const [path, body, envelope] of calls) {
      const answer = await callGateway(world!.gateway.proxyUrl, tenant.key, path, body);
      const what = `${path} ${JSON.stringify(body)}: ${answer.status} ${answer.text}`;
      expect(answer.status, what).toBe(429);
      const error = answer.json?.error;
      expect(error, what).toBeDefined();
      expect(error.message, what).toBe(PREPAID_MESSAGE);
      if (envelope === "openai") {
        expect(error.type, what).toBe("billing_error");
        expect(error.code, what).toBe("budget_exceeded");
        // The caller's own organization, so its structured fields are shown.
        expect(error.scope, what).toBe("team");
        expect(error.scope_ref, what).toBe(tenant.id);
        // A balance is no period budget: no amounts, and no reset to wait for.
        for (const field of ["limit_usd", "spent_usd", "remaining_usd", "period_resets_at", "retry_after_seconds"]) {
          expect(error, `${field} in ${what}`).not.toHaveProperty(field);
        }
      } else {
        expect(answer.json.type, what).toBe("error");
      }
      expect(answer.headers.get("retry-after"), what).toBeNull();
    }
    expect(world!.upstream.requests, "a refused request reached the upstream").toEqual([]);
  });
});
