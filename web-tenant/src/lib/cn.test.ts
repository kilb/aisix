import assert from "node:assert/strict";
import { test } from "node:test";
import { cn } from "./cn.ts";

test("语义字号不会删除主操作与危险操作的文字前景", () => {
  for (const color of ["text-primary-foreground", "text-destructive-foreground", "text-ink"]) {
    for (const size of ["hero", "display", "figure", "value", "title", "lead", "stat", "brand", "body", "caption", "meta", "micro"]) {
      const classes = new Set(cn(color, `text-${size}`).split(/\s+/));
      assert.ok(classes.has(color), `${color} 被 text-${size} 删除`);
      assert.ok(classes.has(`text-${size}`));
    }
  }
});

test("同组字号仍以最后一个为准，颜色和响应式字号独立保留", () => {
  const classes = new Set(cn("text-body text-foreground tablet:text-stat", "text-caption").split(/\s+/));
  assert.deepEqual([...classes].sort(), ["tablet:text-stat", "text-caption", "text-foreground"].sort());
});

test("真实小号主按钮组合同时保留深底、浅色文字与语义字号", () => {
  const classes = new Set(cn("btn bg-primary text-primary-foreground text-body", "h-11 px-4 text-caption").split(/\s+/));
  for (const required of ["btn", "bg-primary", "text-primary-foreground", "h-11", "px-4", "text-caption"]) assert.ok(classes.has(required), required);
  assert.ok(!classes.has("text-body"));
});
