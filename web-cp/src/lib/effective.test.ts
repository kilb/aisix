import assert from "node:assert/strict";
import { test } from "node:test";
import { parseUtc, sendAt, type EffectiveResolver } from "./effective.ts";

const past = Object.assign(new Error("past"), { status: 422, details: [{ path: "/effective_from", message: "x" }] });

function resolver(immediate: boolean, stamps: (string | null)[]): EffectiveResolver {
  let i = 0;
  return { immediate, stamp: () => stamps[Math.min(i++, stamps.length - 1)] ?? null };
}

test("parseUtc reads a UTC minute, with optional seconds and a T or Z", () => {
  assert.equal(parseUtc("2030-01-02 03:04"), "2030-01-02T03:04:00.000Z");
  assert.equal(parseUtc("2030-01-02T03:04:05Z"), "2030-01-02T03:04:05.000Z");
  for (const bad of ["", "2030-01-02", "2030-13-45 03:04", "02/01/2030 03:04"]) assert.equal(parseUtc(bad), null, bad);
});

test("parseUtc refuses a day or a time the calendar does not have, rather than moving it", () => {
  for (const bad of ["2027-02-29 00:00", "2027-04-31 10:00", "2027-02-30 12:00", "2027-01-01 24:00", "2027-01-01 23:60", "2027-06-31T08:15:00Z"]) {
    assert.equal(parseUtc(bad), null, bad);
  }
  // A leap year's 29 February, and the last minute of a month, are days like any other.
  assert.equal(parseUtc("2028-02-29 00:00"), "2028-02-29T00:00:00.000Z");
  assert.equal(parseUtc("2027-04-30 23:59:59"), "2027-04-30T23:59:59.000Z");
});

test("an invalid typed time is `invalid` and nothing is sent", async () => {
  let sent = 0;
  const r = await sendAt(resolver(false, [null]), async () => void sent++);
  assert.equal(r, "invalid");
  assert.equal(sent, 0);
});

test("an immediate start refused as past is stamped again and sent once more", async () => {
  const seen: string[] = [];
  const r = await sendAt(resolver(true, ["a", "b"]), async (at) => {
    seen.push(at);
    if (at === "a") throw past;
    return "ok";
  });
  assert.equal(r, "ok");
  assert.deepEqual(seen, ["a", "b"]);
});

test("a typed start refused as past is not retried", async () => {
  const seen: string[] = [];
  await assert.rejects(
    sendAt(resolver(false, ["a", "b"]), async (at) => {
      seen.push(at);
      throw past;
    }),
    past,
  );
  assert.deepEqual(seen, ["a"]);
});

test("another refusal of an immediate start is not retried", async () => {
  const other = Object.assign(new Error("bad"), { status: 422, details: [{ path: "/card" }] });
  let sent = 0;
  await assert.rejects(
    sendAt(resolver(true, ["a", "b"]), async () => {
      sent++;
      throw other;
    }),
    other,
  );
  assert.equal(sent, 1);
});
