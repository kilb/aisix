import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

// `npm run test:unit`. The portal carries copies of the Dashboard's design
// system files (under web-cp/src) rather than importing them, so the two
// apps build alone; this pins each copy to its source, byte for byte.

/** Reads `path`, relative to `src/`, from this app and from the Dashboard. */
function both(path: string): [Buffer, Buffer] {
  return [
    readFileSync(new URL(`../${path}`, import.meta.url)),
    readFileSync(new URL(`../../../web-cp/src/${path}`, import.meta.url)),
  ];
}

test("门户与运维台的设计令牌逐字节一致", () => {
  const [mine, cp] = both("theme/tokens.css");
  assert.ok(mine.equals(cp));
});

test("门户与运维台的基础样式逐字节一致", () => {
  const [mine, cp] = both("theme/base.css");
  assert.ok(mine.equals(cp));
});

test("门户与运维台的金额工具逐字节一致", () => {
  const [mine, cp] = both("lib/money.ts");
  assert.ok(mine.equals(cp));
});

// The shared components and their contract: one set of props, states and
// class names in both apps (web-cp/src/ui/CONTRACT.md).
const SHARED_UI = [
  "CONTRACT.md",
  "Button.tsx",
  "Field.tsx",
  "Toast.tsx",
  "TwoStep.tsx",
  "Lamp.tsx",
  "EmptyState.tsx",
  "DataTable.tsx",
  "Icon.tsx",
];

for (const file of SHARED_UI) {
  test(`门户与运维台的共享组件 ${file} 逐字节一致`, () => {
    const [mine, cp] = both(`ui/${file}`);
    assert.ok(mine.equals(cp));
  });
}
