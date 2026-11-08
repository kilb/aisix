import assert from "node:assert/strict";
import { test } from "node:test";
import { deltaWord, outcomeUnknown, utcInputToIso, utcInputValue } from "./repricing.ts";

// Shaped like the client's `ApiError`, whose class needs a TypeScript transform to load here.
const err = (status: number, code: string) => Object.assign(new Error("x"), { status, code });

test("a datetime-local value is read as UTC, whatever the viewer's zone", () => {
  assert.equal(utcInputToIso("2026-11-07T08:30"), "2026-11-07T08:30:00.000Z");
  assert.equal(utcInputToIso("2026-11-07T08:30:15"), "2026-11-07T08:30:15.000Z");
  assert.equal(utcInputValue(new Date("2026-11-07T08:30:15.999Z")), "2026-11-07T08:30:15");
});

test("a value that is not a date is refused, not rolled over", () => {
  assert.equal(utcInputToIso(""), null);
  assert.equal(utcInputToIso("2026-02-30T00:00"), null);
  assert.equal(utcInputToIso("2026-11-07"), null);
  assert.equal(utcInputToIso("2026-11-07T25:00"), null);
});

test("the sign of a total says who is owed", () => {
  assert.equal(deltaWord(33_000), "多收");
  assert.equal(deltaWord(-1), "退还");
  assert.equal(deltaWord(0), "无差额");
});

test("only a 4xx from the control plane proves an apply did nothing", () => {
  assert.equal(outcomeUnknown(err(409, "stale_preview")), false);
  assert.equal(outcomeUnknown(err(403, "forbidden")), false);
  assert.equal(outcomeUnknown(err(0, "network")), true);
  assert.equal(outcomeUnknown(err(500, "internal")), true);
  assert.equal(outcomeUnknown(err(503, "unavailable")), true);
  assert.equal(outcomeUnknown(err(502, "bad_response")), true);
  assert.equal(outcomeUnknown(err(200, "bad_response")), true);
  assert.equal(outcomeUnknown(err(404, "bad_response")), true);
  assert.equal(outcomeUnknown(new TypeError("boom")), true);
});
