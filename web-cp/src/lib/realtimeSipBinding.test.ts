import assert from "node:assert/strict";
import { test } from "node:test";
import { isRealtimeSipProjectPath, realtimeSipBody, realtimeSipDraft, realtimeSipErrors, realtimeSipProjectBody, realtimeSipProjectDraft, realtimeSipProjectError, realtimeSipWebhookPath } from "./realtimeSipBinding.ts";

const stored = {
  route_id: "tenant-one",
  webhook_secret: "[REDACTED]",
  api_key_id: "11111111-1111-1111-1111-111111111111",
  model_id: "22222222-2222-2222-2222-222222222222",
};

test("opening and saving a binding keeps its credential placeholder", () => {
  const draft = realtimeSipDraft(stored);
  assert.deepEqual(realtimeSipErrors(draft, stored, false), {});
  assert.deepEqual(realtimeSipBody(draft, stored), stored);
  assert.equal(realtimeSipDraft({ ...stored, webhook_secret: "whsec-do-not-display" }).webhookSecret, "[REDACTED]");
});

test("each changed binding target requires a newly entered credential", () => {
  const draft = realtimeSipDraft(stored);
  for (const changed of [
    { ...draft, routeId: "tenant-two" },
    { ...draft, apiKeyId: "33333333-3333-3333-3333-333333333333" },
    { ...draft, modelId: "44444444-4444-4444-4444-444444444444" },
  ]) {
    assert.equal(typeof realtimeSipErrors(changed, stored, false).webhook_secret, "string");
    assert.deepEqual(realtimeSipErrors({ ...changed, webhookSecret: "replacement" }, stored, false), {});
  }
  assert.equal(typeof realtimeSipErrors(draft, stored, true).webhook_secret, "string");
  assert.deepEqual(realtimeSipErrors({ ...draft, webhookSecret: "replacement" }, stored, true), {});
});

test("disabling removes the binding and a new binding cannot reuse a placeholder", () => {
  const disabled = { ...realtimeSipDraft(stored), enabled: false };
  assert.equal(realtimeSipBody(disabled, stored), undefined);
  assert.deepEqual(realtimeSipErrors(disabled, stored, true), {});
  assert.equal(typeof realtimeSipErrors(realtimeSipDraft(stored), undefined, false).webhook_secret, "string");
  assert.deepEqual(realtimeSipDraft(undefined), { enabled: false, routeId: "", webhookSecret: "", apiKeyId: "", modelId: "" });
});

test("new bindings retain exact credential bytes and trim only identifiers", () => {
  const draft = { enabled: true, routeId: " tenant_1 ", apiKeyId: ` ${stored.api_key_id} `, modelId: ` ${stored.model_id} `, webhookSecret: " whsec_exact-bytes " };
  assert.deepEqual(realtimeSipErrors(draft, undefined, false), {});
  assert.deepEqual(realtimeSipBody(draft, undefined), { ...stored, route_id: "tenant_1", webhook_secret: " whsec_exact-bytes " });
});

test("invalid routes, UUIDs and missing credentials are rejected before saving", () => {
  for (const routeId of ["", "../tenant", "a/b", "x?owner=two", "a".repeat(65)]) {
    assert.equal(typeof realtimeSipErrors({ ...realtimeSipDraft(stored), routeId }, stored, false).route_id, "string");
    assert.equal(realtimeSipWebhookPath(routeId), null);
  }
  for (const webhookSecret of ["", " ", undefined]) assert.equal(typeof realtimeSipErrors({ ...realtimeSipDraft(stored), webhookSecret }, stored, false).webhook_secret, "string");
  assert.equal(typeof realtimeSipErrors({ ...realtimeSipDraft(stored), apiKeyId: "not-a-key" }, stored, false).api_key_id, "string");
  assert.equal(typeof realtimeSipErrors({ ...realtimeSipDraft(stored), modelId: "22222222-2222-2222-2222-22222222222A" }, stored, false).model_id, "string");
  assert.equal(realtimeSipWebhookPath(" tenant_1 "), "/internal/native/realtime/sip/tenant_1");
});

test("project IDs use the stored header state and an enabled binding requires one", () => {
  assert.equal(realtimeSipProjectDraft({ default_headers: { "OpEnAi-PrOjEcT": "proj_never-display" } }), "[REDACTED]");
  assert.equal(realtimeSipProjectDraft({ default_headers: { "x-other": "kept" } }), "");
  assert.equal(realtimeSipProjectDraft(undefined), "");
  assert.equal(typeof realtimeSipProjectError(true, ""), "string");
  assert.equal(typeof realtimeSipProjectError(true, undefined), "string");
  assert.equal(realtimeSipProjectError(true, "[REDACTED]"), null);
  assert.equal(realtimeSipProjectError(false, ""), null);
});

test("writing a project preserves every other request field and header", () => {
  const request = { default_headers: { "x-tenant-secret": "[REDACTED]", "openai-project": "[REDACTED]" }, default_body_fields: { stream: true }, custom: { nested: "kept" } };
  assert.strictEqual(realtimeSipProjectBody(request, "[REDACTED]"), request);
  const saved = realtimeSipProjectBody(request, " proj-new ");
  assert.deepEqual(saved, { ...request, default_headers: { "x-tenant-secret": "[REDACTED]", "OpenAI-Project": "proj-new" } });
  assert.deepEqual(request.default_headers, { "x-tenant-secret": "[REDACTED]", "openai-project": "[REDACTED]" });
  assert.deepEqual(realtimeSipProjectBody(undefined, "proj-new"), { default_headers: { "OpenAI-Project": "proj-new" } });
  assert.equal(isRealtimeSipProjectPath("default header OpEnAi-PrOjEcT"), true);
  assert.equal(isRealtimeSipProjectPath("/request/default_headers/openai-project"), true);
  assert.equal(isRealtimeSipProjectPath("default header x-other"), false);
});
