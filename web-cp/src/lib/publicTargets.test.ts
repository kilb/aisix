import assert from "node:assert/strict";
import { test } from "node:test";
import type { PublicModel, ServedBy } from "./api.ts";
import { addedBody, changedBody, draftOf, shownDraft, targetsOf, unsendable, withSaved } from "./publicTargets.ts";

// `npm run test:unit`.

function served(over: Partial<ServedBy> = {}): ServedBy {
  return {
    provider_key_id: "acct-a",
    display_name: "账号 A",
    groups: ["shared"],
    upstream_model: null,
    priority: 1,
    weight: 1,
    enabled: true,
    ...over,
  };
}

test("cooldown changes are saved without rewriting an untouched channel field", () => {
  const stored = served({ cooldown: { enabled: true, default_seconds: 45, trigger_statuses: [429] } });
  const draft = shownDraft(stored, { cooldown: { ...stored.cooldown, enabled: false } });
  assert.deepEqual(changedBody(draft, stored), { cooldown: { enabled: false, default_seconds: 45, trigger_statuses: [429] } });
});

test("an unsafe stored duration cannot be rounded and resent when cooldown is toggled", () => {
  const stored = served({ cooldown: { enabled: true, default_seconds: 9_007_199_254_740_992 } });
  const changed = shownDraft(stored, { cooldown: { ...stored.cooldown, enabled: false } });
  assert.equal(typeof unsendable(changed, stored).cooldown, "string");
  assert.deepEqual(unsendable(shownDraft(stored, { priority: "2" }), stored), {});
});

function view(servedBy: ServedBy[]): PublicModel {
  return {
    id: "pub-1",
    name: "pub-x",
    enabled: true,
    served_by: servedBy,
    routing: {},
    tenants: 0,
    keys: 0,
    reachable_tenants: 0,
    reachable_keys: 0,
    created_at: "2026-11-10T00:00:00Z",
    retired_at: null,
  };
}

test("a disabled mapping is not a target", () => {
  const on = served({ provider_key_id: "on" });
  const off = served({ provider_key_id: "off", enabled: false });
  assert.deepEqual(targetsOf(view([on, off])), [on]);
});

test("a stored mapping starts as its own values, and an unset upstream name as an empty field", () => {
  assert.deepEqual(draftOf(served({ priority: -3, weight: 0 })), { upstream: "", priority: "-3", weight: "0" });
  assert.equal(draftOf(served({ upstream_model: "gpt-up" })).upstream, "gpt-up");
});

test("an untouched target sends nothing", () => {
  const s = served({ upstream_model: "gpt-up", priority: 2, weight: 5 });
  assert.equal(changedBody(draftOf(s), s), null);
  // Surrounding blanks are not a change either.
  assert.equal(changedBody({ upstream: " gpt-up ", priority: " 2", weight: "5 " }, s), null);
});

test("a field the operator did not touch follows a later read, so a save never sends it back", () => {
  // The operator edited the weight while the mapping held priority 1; another
  // operator then set the priority to 9 and this page read it again. The form
  // still holds only the typed weight, so what it sends is the weight alone —
  // priority 9 is left as the other operator wrote it.
  const typed = { weight: "3" };
  const afterReload = served({ upstream_model: "gpt-up", priority: 9, weight: 1 });
  const shown = shownDraft(afterReload, typed);
  assert.deepEqual(shown, { upstream: "gpt-up", priority: "9", weight: "3" });
  assert.deepEqual(changedBody(shown, afterReload), { weight: 3 });
  // A row the operator never edited shows what was last read and sends nothing.
  assert.equal(changedBody(shownDraft(afterReload, undefined), afterReload), null);
  // An upstream name typed on its own is the only field sent, even when the
  // stored priority changed under it.
  assert.deepEqual(changedBody(shownDraft(served({ priority: 4 }), { upstream: "typed" }), served({ priority: 4 })), {
    upstream_model: "typed",
  });
});

test("only the fields that changed are sent", () => {
  const s = served({ upstream_model: "gpt-up", priority: 2, weight: 5 });
  assert.deepEqual(changedBody({ ...draftOf(s), weight: "3" }, s), { weight: 3 });
  assert.deepEqual(changedBody({ ...draftOf(s), priority: "-1" }, s), { priority: -1 });
  assert.deepEqual(changedBody({ ...draftOf(s), upstream: "gpt-other" }, s), { upstream_model: "gpt-other" });
});

test("clearing the upstream name sends null, which means the public name; a refused number is still sent", () => {
  const s = served({ upstream_model: "gpt-up" });
  assert.deepEqual(changedBody({ ...draftOf(s), upstream: "  " }, s), { upstream_model: null });
  // The control plane owns the weight's range: a negative one goes to it and its refusal is shown.
  assert.deepEqual(changedBody({ ...draftOf(s), weight: "-1" }, s), { weight: -1 });
});

test("an added target sends every field and asks for the mapping enabled", () => {
  assert.deepEqual(addedBody({ upstream: "", priority: "0", weight: "1" }), {
    upstream_model: null,
    priority: 0,
    weight: 1,
    enabled: true,
  });
  assert.deepEqual(addedBody({ upstream: " gpt-up ", priority: "9", weight: "4" }), {
    upstream_model: "gpt-up",
    priority: 9,
    weight: 4,
    enabled: true,
  });
});

test("only what cannot be sent at all is refused before the write", () => {
  assert.deepEqual(unsendable({ upstream: "", priority: "1", weight: "-1" }), {});
  assert.deepEqual(unsendable({ upstream: "", priority: "", weight: "1" }), { priority: "请填写优先级" });
  assert.deepEqual(unsendable({ upstream: "", priority: "1.5", weight: "1" }), { priority: "优先级必须是整数" });
  assert.deepEqual(unsendable({ upstream: "", priority: "1", weight: "99999999999" }), {
    weight: "权重超出可以保存的范围",
  });
});

test("a written row replaces the stored one as the write answered it, in the control plane's order", () => {
  const a = served({ provider_key_id: "a", priority: 1, weight: 1 });
  const b = served({ provider_key_id: "b", display_name: "账号 B", priority: 1, weight: 1 });
  const saved = { public_model_id: "pub-1", public_model_name: "pub-x", served: true };
  const next = withSaved(
    view([a, b]),
    "b",
    { ...saved, upstream_model: "gpt-b", priority: 5, weight: 2, enabled: true },
    { display_name: "unused", groups: [] },
  );
  assert.deepEqual(
    next.served_by.map((s) => [s.provider_key_id, s.display_name, s.upstream_model, s.priority, s.weight]),
    [
      ["b", "账号 B", "gpt-b", 5, 2],
      ["a", "账号 A", null, 1, 1],
    ],
  );
  // An account that did not serve the model before is added with its own name and groups.
  const added = withSaved(
    view([a]),
    "c",
    { ...saved, upstream_model: null, priority: 0, weight: 1, enabled: true },
    { display_name: "账号 C", groups: ["grp-c"] },
  );
  assert.deepEqual(added.served_by.at(-1), {
    provider_key_id: "c",
    display_name: "账号 C",
    groups: ["grp-c"],
    upstream_model: null,
    priority: 0,
    weight: 1,
    enabled: true,
  });
});
