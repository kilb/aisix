import assert from "node:assert/strict";
import { test } from "node:test";
import { violations } from "./check-design.mjs";

test("任意新增组件与旧样式均受设计审计约束", () => {
  for (const [name, source, id] of [
    ["src/tabs/Example.tsx", '<div className="text-[15px]" />', "arbitrary-font-size"],
    ["src/tabs/Example.tsx", '<div className="bg-blue-500" />', "default-palette"],
    ["src/styles.css", ".old { color: #123456; }", "css-color"],
    ["src/tabs/Example.css", ".old { font-size: 19px; }", "css-font-size"],
    ["src/ui/ui.css", ".old { border-radius: 9px; }", "css-radius"],
    ["src/Styleguide.css", ".old { box-shadow: 0 4px 8px black; }", "css-shadow"],
  ]) assert.ok(violations(name, source).some(item => item.id === id), `${name}: ${id}`);
  assert.equal(violations("src/styles.css", ".ok { color: var(--foreground); font-size: var(--text-body); border-radius: var(--r-field); box-shadow: var(--shadow-card); }").length, 0);
  assert.equal(violations("src/theme/tokens.css", ":root { --foreground: #111110; }").length, 0);
});
