import assert from "node:assert/strict";
import { test } from "node:test";
import { safeHttpUrl } from "./safeUrl.ts";

test("a script URL, a data URL and a relative path are all refused", () => {
  assert.equal(safeHttpUrl("javascript:alert(1)"), null);
  assert.equal(safeHttpUrl("data:text/html,<script>alert(1)</script>"), null);
  assert.equal(safeHttpUrl("/pricing"), null);
  assert.equal(safeHttpUrl(""), null);
  assert.equal(safeHttpUrl(null), null);
});

test("an https page passes, normalised", () => {
  assert.equal(safeHttpUrl("https://developers.openai.com/api/docs/pricing"), "https://developers.openai.com/api/docs/pricing");
});

test("http passes too, and the scheme is what decides", () => {
  assert.equal(safeHttpUrl("http://example.test/prices"), "http://example.test/prices");
  assert.equal(safeHttpUrl("JavaScript:alert(1)"), null);
  assert.equal(safeHttpUrl("javascript://example.test/%0aalert(1)"), null);
});
