import assert from "node:assert/strict";
import { test } from "node:test";
import { draftOf, routingBody, sameRouting, statusesOf } from "./publicRouting.ts";

// `npm run test:unit`.

const DEFAULT = "round_robin";

test("a model that set nothing shows the gateway's strategy and empty fields", () => {
  assert.deepEqual(draftOf({}, DEFAULT), {
    strategy: DEFAULT,
    affinity: "default",
    retries: "",
    maxFallbacks: "",
    retryOn429: false,
    statuses: "",
    whenAllUnavailable: "",
  });
});

test("API key affinity is explicit and is removed when another strategy is selected", () => {
  const stored = { strategy: "consistent_hash", hash_on: [{ type: "header", name: "x-session" }] };
  const draft = draftOf(stored, DEFAULT);
  assert.equal(draft.affinity, "custom");
  draft.affinity = "api_key";
  assert.deepEqual(routingBody(draft, stored, DEFAULT, []), {
    strategy: "consistent_hash", hash_on: [{ type: "api_key" }],
  });
  draft.strategy = "failover";
  assert.deepEqual(routingBody(draft, stored, DEFAULT, []), { strategy: "failover" });
});

test("a stored setting is shown as it was stored", () => {
  const draft = draftOf(
    {
      strategy: "consistent_hash",
      retries: 2,
      max_fallbacks: 1,
      retry_on_429: true,
      fallback_on_statuses: [408, 429],
      when_all_unavailable: "try_anyway",
    },
    DEFAULT,
  );
  assert.equal(draft.strategy, "consistent_hash");
  assert.equal(draft.retries, "2");
  assert.equal(draft.maxFallbacks, "1");
  assert.equal(draft.retryOn429, true);
  assert.equal(draft.statuses, "408, 429");
  assert.equal(draft.whenAllUnavailable, "try_anyway");
});

test("only what the operator wrote is sent: the default strategy and empty fields are left out", () => {
  const body = routingBody(draftOf({}, DEFAULT), {}, DEFAULT, []);
  assert.deepEqual(body, {});
});

test("a chosen setting is sent, and a field cleared in the form is not carried over from what was stored", () => {
  const stored = { strategy: "failover", retries: 3, retry_on_429: true };
  const draft = draftOf(stored, DEFAULT);
  // Untouched, the form sends back exactly what it was given.
  assert.deepEqual(routingBody(draft, stored, DEFAULT, []), {
    strategy: "failover",
    retries: 3,
    retry_on_429: true,
  });
  draft.retries = "";
  draft.strategy = DEFAULT;
  assert.deepEqual(routingBody(draft, stored, DEFAULT, []), { retry_on_429: true });

  draft.statuses = "408";
  assert.deepEqual(routingBody(draft, stored, DEFAULT, statusesOf(draft.statuses)), {
    retry_on_429: true,
    fallback_on_statuses: [408],
  });
});

test("unchecking a stored switch clears it rather than leaving it in force", () => {
  const stored = { retry_on_429: true };
  const draft = draftOf(stored, DEFAULT);
  draft.retryOn429 = false;
  assert.deepEqual(routingBody(draft, stored, DEFAULT, []), {});
});

test("a setting the form has no field for is carried over as stored", () => {
  const stored = { strategy: "consistent_hash", hash_on: [{ type: "header", name: "x-tenant" }] };
  const body = routingBody(draftOf(stored, DEFAULT), stored, DEFAULT, []);
  assert.deepEqual(body, {
    strategy: "consistent_hash",
    hash_on: [{ type: "header", name: "x-tenant" }],
  });
});

test("a written zero is a value, not an empty field", () => {
  const draft = draftOf({}, DEFAULT);
  draft.retries = "0";
  draft.maxFallbacks = "0";
  assert.deepEqual(routingBody(draft, {}, DEFAULT, []), { retries: 0, max_fallbacks: 0 });
});

test("statuses are read from any separator, and anything that is not a number is refused", () => {
  assert.deepEqual(statusesOf("408, 429"), [408, 429]);
  assert.deepEqual(statusesOf("408、429 500"), [408, 429, 500]);
  assert.deepEqual(statusesOf(""), []);
  assert.equal(statusesOf("408, gone"), null);
  assert.equal(statusesOf("4o8"), null);
});

test("an untouched form says the same as what was stored, and a changed one does not", () => {
  const stored = { strategy: "failover", retries: 2, fallback_on_statuses: [408, 429], hash_on: [{ type: "header", name: "x" }] };
  const draft = draftOf(stored, DEFAULT);
  assert.equal(sameRouting(routingBody(draft, stored, DEFAULT, statusesOf(draft.statuses)), stored), true);
  // Key order is not a difference.
  assert.equal(sameRouting({ retries: 2, strategy: "failover" }, { strategy: "failover", retries: 2 }), true);
  draft.retries = "3";
  assert.equal(sameRouting(routingBody(draft, stored, DEFAULT, statusesOf(draft.statuses)), stored), false);
  assert.equal(sameRouting({ fallback_on_statuses: [429, 408] }, { fallback_on_statuses: [408, 429] }), false);
});
