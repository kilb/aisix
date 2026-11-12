import assert from "node:assert/strict";
import { test } from "node:test";
import { exactInteger, exactRatio, formatCount, formatDuration, formatRatio, normalizeExactValues, summarizeUsage } from "./chartFormat.ts";

// 数值先变浮点或把未知改成零，都会破坏这些独立算出的结果。
test("exact integers retain every digit and reject unsafe or missing numbers", () => {
  assert.equal(exactInteger("900719925474099312345"), 900719925474099312345n);
  assert.equal(exactInteger(9007199254740992), null);
  assert.equal(exactInteger(undefined), null);
  assert.equal(exactInteger(""), null);
  assert.equal(exactInteger("1.2"), null);
  assert.equal(exactInteger(0), 0n);
});

test("geometry normalizes huge signed amounts without changing their display source", () => {
  assert.deepEqual(normalizeExactValues(["900719925474099312345", "450359962737049656172", "-900719925474099312345", 0, null, 9007199254740992]), [1, 0.499999, -1, 0, null, null]);
  assert.deepEqual(normalizeExactValues([0, "0", null]), [0, 0, null]);
});

test("budget proportions distinguish zero spend from unknown or zero limits", () => {
  assert.equal(exactRatio("0", "20"), 0);
  assert.equal(exactRatio("3", "20"), 0.15);
  assert.equal(exactRatio("30", "20"), 1);
  assert.equal(exactRatio("-3", "20"), 0);
  assert.equal(exactRatio("0", "0"), null);
  assert.equal(exactRatio(null, "20"), null);
  assert.equal(exactRatio(9007199254740992, "20"), null);
});

test("ordinary formats preserve exact counts and show unknown measurements", () => {
  assert.equal(formatCount("900719925474099312345"), "900,719,925,474,099,312,345");
  assert.equal(formatCount(9007199254740992), "—");
  assert.equal(formatRatio(0), "0%");
  assert.equal(formatRatio(null), "—");
  assert.equal(formatDuration(0), "0 ms");
  assert.equal(formatDuration(null), "—");
});

const model = { id: "model-a", name: "Model A" };

test("usage aggregates repeated periods and models exactly without inventing missing days", () => {
  const result = summarizeUsage([
    { period_start: "2026-11-03T00:00:00Z", model, requests: 2, charged_micro_cny: "9007199254740993", estimated_requests: 0 },
    { period_start: "2026-11-01T00:00:00Z", model, requests: 3, charged_micro_cny: "7", estimated_requests: 1 },
    { period_start: "2026-11-03T00:00:00Z", model: { id: "model-b", name: "Model B" }, requests: 1, charged_micro_cny: "10", estimated_requests: 0 },
  ]);
  assert.equal(result.requests, 6n);
  assert.equal(result.chargedMicroCny, 9007199254741010n);
  assert.equal(result.estimatedRequests, 1n);
  assert.equal(result.hasMergedPeriods, false);
  assert.deepEqual(result.periods, [
    { label: "2026-11-01T00:00:00Z", requests: 3n, chargedMicroCny: 7n },
    { label: "2026-11-03T00:00:00Z", requests: 3n, chargedMicroCny: 9007199254741003n },
  ]);
  assert.deepEqual(result.models, [
    { id: "model-a", label: "Model A", requests: 5n, chargedMicroCny: 9007199254741000n },
    { id: "model-b", label: "Model B", requests: 1n, chargedMicroCny: 10n },
  ]);
});

test("cross-period other contributes to totals but disables the entire trend", () => {
  const result = summarizeUsage([
    { period_start: "2026-11-01", model, requests: 3, charged_micro_cny: 7, estimated_requests: 0 },
    { period_start: "2026-11-02", model: { id: null, name: "other" }, requests: 100, charged_micro_cny: 300, estimated_requests: 2 },
  ]);
  assert.equal(result.requests, 103n);
  assert.equal(result.chargedMicroCny, 307n);
  assert.equal(result.hasMergedPeriods, true);
  assert.deepEqual(result.periods, []);
  assert.equal(result.models.find(row => row.id === null)?.chargedMicroCny, 300n);
});

test("unknown or unsafe values invalidate only their affected aggregate", () => {
  const result = summarizeUsage([
    { period_start: "2026-11-01", model, requests: 1, charged_micro_cny: 9007199254740992, estimated_requests: 0 },
    { period_start: "2026-11-02", model, requests: 2, charged_micro_cny: 0, estimated_requests: 0 },
  ]);
  assert.equal(result.requests, 3n);
  assert.equal(result.chargedMicroCny, null);
  assert.equal(result.models[0]?.chargedMicroCny, null);
  assert.equal(result.periods[0]?.chargedMicroCny, null);
  assert.equal(result.periods[1]?.chargedMicroCny, 0n);
  assert.equal(summarizeUsage([]).chargedMicroCny, 0n);
  const unknown = summarizeUsage([{ period_start: "2026-11-01", model, requests: null, charged_micro_cny: null, estimated_requests: null }]);
  assert.equal(unknown.requests, null);
  assert.equal(unknown.chargedMicroCny, null);
  assert.equal(unknown.estimatedRequests, null);
});
