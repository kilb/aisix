import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { readTokens } from "../../../web-cp/src/lib/contrast.ts";

const source = (file: string): string => readFileSync(new URL(`../${file}`, import.meta.url), "utf8");

// 约束令牌与布局声明；真实渲染、焦点和横滚由真实后端 E2E 验收。
test("refined 使用暖石灰画布、暖白面板、墨色交互与独立橙色强调", () => {
  const tokens = readTokens(source("theme/tokens.css"), "light");
  for (const [name, value] of Object.entries({
    "--background": "#ECEAE4", "--card": "#FBFAF7", "--well": "#E1DED6",
    "--foreground": "#121211", "--muted-foreground": "#64635D", "--bento-accent": "#FF5A1F",
    "--primary": "#121211", "--link": "#121211", "--ring": "#121211",
  })) assert.equal(tokens[name], value, name);
  assert.equal(tokens["--h-control"], "44px");
  assert.equal(tokens["--shadow-float"], "none");
});

test("refined 桌面侧栏留出四周空间，抽屉保留独立宽度和高度", () => {
  const css = source("ui/Sidebar.css");
  const shell = /\.sidebar-shell\s*\{([^}]+)\}/.exec(css)?.[1] ?? "";
  const sidebar = /\.shell-sidebar\s*\{([^}]+)\}/.exec(css)?.[1] ?? "";
  const drawer = /\.shell-drawer\s*\{([^}]+)\}/.exec(css)?.[1] ?? "";
  assert.match(shell, /grid-template-columns:\s*224px minmax\(0, 1fr\)/);
  assert.match(shell, /padding:\s*24px/);
  assert.match(shell, /gap:\s*24px/);
  assert.match(sidebar, /top:\s*24px/);
  assert.match(sidebar, /height:\s*calc\(100dvh - 48px\)/);
  assert.match(sidebar, /border-radius:\s*var\(--r-rail\)/);
  assert.match(drawer, /height:\s*calc\(100dvh - 32px\)/);
  assert.match(drawer, /width:\s*min\(288px, calc\(100vw - 32px\)\)/);
});

test("refined 数字使用400字重且旧样式不得将其改回500", () => {
  assert.match(source("components/page.tsx"), /metric font-normal tabular-nums/);
  for (const file of ["styles.css", "ui/ui.css"])
    assert.doesNotMatch(source(file), /\.metric\s*\{[^}]*font-weight:\s*(?:500|600|700)/);
  const tokens = readTokens(source("theme/tokens.css"), "light");
  for (const name of ["--type-hero", "--type-display", "--type-figure", "--type-value"])
    assert.ok(parseInt(tokens[name] ?? "0") >= 28 && parseInt(tokens[name] ?? "0") <= 36, name);
});

// 文件各自独立读取，任意一端单改都必须失败。
test("refined 侧栏与页面原语在两端保持逐字节一致", () => {
  for (const file of ["theme/tokens.css", "ui/Sidebar.css", "components/page.tsx", "ui/CONTRACT.md"])
    assert.equal(source(file), readFileSync(new URL(`../../../web-cp/src/${file}`, import.meta.url), "utf8"), file);
});
