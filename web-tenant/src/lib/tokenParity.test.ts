import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

// `npm run test:unit`. The portal carries a copy of the Dashboard's design
// tokens (web-cp/src/theme/tokens.css) rather than importing it, so the two
// apps build alone; this pins the copy to its source, byte for byte.

test("门户与运维台的设计令牌逐字节一致", () => {
  const mine = readFileSync(new URL("../theme/tokens.css", import.meta.url));
  const cp = readFileSync(new URL("../../../web-cp/src/theme/tokens.css", import.meta.url));
  assert.ok(mine.equals(cp));
});
