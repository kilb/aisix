import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { readTokens } from "../../../web-cp/src/lib/contrast.ts";

const source = (file: string): string => readFileSync(new URL(`../${file}`, import.meta.url), "utf8");

// 用户指定模板后，旧蓝色方案的断言改为精确约束模板颜色与组件层级。
function templatePalette(css: string): void {
  const tokens = readTokens(css, "light");
  for (const [name, value] of Object.entries({
    "--background": "#f3f2ee", "--card": "#ffffff", "--foreground": "#111110",
    "--lilac": "#e3d0ff", "--lime": "#d5f95f", "--primary": "#111110",
  })) assert.equal(tokens[name]?.toLowerCase(), value, name);
  assert.equal(tokens["--card-gap"], "16px");
  assert.equal(tokens["--r-card"], "28px");
  assert.equal(tokens["--h-control"], "40px");
  assert.ok(tokens["--shadow-float"], "浮层阴影必须显式存在");
  assert.notEqual(tokens["--shadow-float"], "none");
  assert.match(tokens["--font-sans"] ?? "", /Manrope/);
}

test("template 使用暖白画布、墨黑交互与丁香紫青柠强调", () => {
  const css = source("theme/tokens.css");
  templatePalette(css);
  const mutated = css.replace(/#f3f2ee/i, "#000000");
  assert.notEqual(mutated, css, "画布 token 必须存在才能执行反例");
  assert.throws(() => templatePalette(mutated), /--background/);
});

test("template 悬浮侧栏保留16px间距，可收起为100px外侧轨道", () => {
  const css = source("ui/Sidebar.css");
  const shell = /\.sidebar-shell\s*\{([^}]+)\}/.exec(css)?.[1] ?? "";
  const sidebar = /\.shell-sidebar\s*\{([^}]+)\}/.exec(css)?.[1] ?? "";
  const drawer = /\.shell-drawer\s*\{([^}]+)\}/.exec(css)?.[1] ?? "";
  assert.match(shell, /grid-template-columns:\s*236px minmax\(0, 1fr\)/);
  assert.match(shell, /padding:\s*16px/);
  assert.match(shell, /gap:\s*16px/);
  assert.match(sidebar, /top:\s*16px/);
  assert.match(sidebar, /height:\s*calc\(100dvh - 32px\)/);
  assert.match(sidebar, /border-radius:\s*var\(--r-card\)/);
  assert.match(css, /\[data-collapsed="true"\]\s*\{\s*grid-template-columns:\s*68px minmax\(0, 1fr\)/);
  assert.match(drawer, /height:\s*calc\(100dvh - 32px\)/);
  assert.match(drawer, /width:\s*min\(288px, calc\(100vw - 32px\)\)/);
});

test("template 页面与指标使用粗体层级，卡片语义覆盖三种强调色", () => {
  const page = source("components/page.tsx");
  assert.match(page, /metric[^"\n]*font-extrabold[^"\n]*tabular/);
  for (const tone of ["lilac", "ink", "lime"]) assert.ok(page.includes(`"${tone}"`), tone);
  assert.ok(page.includes('"table"'), "表格拥有独立内边距模式");
  assert.doesNotMatch(page, /text-\[\d+(?:\.\d+)?(?:px|rem|em)\]/);
});

// 独立读取两份文件，任意一端单独修改都会使断言失败。
test("template 侧栏与页面原语在两端保持逐字节一致", () => {
  for (const file of ["theme/tokens.css", "ui/Sidebar.css", "components/page.tsx", "ui/CONTRACT.md"])
    assert.equal(source(file), readFileSync(new URL(`../../../web-cp/src/${file}`, import.meta.url), "utf8"), file);
});
