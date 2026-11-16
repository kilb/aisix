#!/usr/bin/env node
/**
 * 设计规范检查：扫描 src/ 下的 .ts/.tsx，找出绕过设计 token 的写法。
 * 用法：npm run lint:design
 * 发现问题时列出「文件:行号」并以非 0 退出码结束，方便 Codex / CI 自查。
 */
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";

const ROOT = new URL("..", import.meta.url).pathname;
const SRC = join(ROOT, "src");

const PALETTE =
  "slate|gray|zinc|stone|red|orange|amber|yellow|green|emerald|teal|cyan|sky|blue|indigo|violet|purple|fuchsia|pink|rose|lime|neutral";

const rules = [
  {
    id: "arbitrary-color",
    re: /\b(?:bg|text|border|ring|outline|fill|stroke|from|via|to|decoration|shadow|divide|placeholder|caret|accent)-\[(?:#|rgb|hsl|oklch|color-mix)/,
    msg: "不要在类名里写任意颜色值，改用 theme/tokens.css 里的语义颜色 token（如 bg-ink、text-fg-2）",
  },
  {
    id: "default-palette",
    re: new RegExp(
      `\\b(?:bg|text|border|ring|outline|fill|stroke|from|via|to|decoration|divide)-(?:${PALETTE})-\\d{2,3}\\b`,
    ),
    msg: "Tailwind 默认调色板已禁用（不会生效），改用规范里的颜色 token",
  },
  {
    id: "arbitrary-font-size",
    re: /\btext-\[\d+(?:\.\d+)?(?:px|rem|em)\]/,
    msg: "不要写任意字号，改用 text-display / heading / title / body / small / caption / micro / metric*",
  },
  {
    id: "arbitrary-radius",
    re: /\brounded(?:-[trbl]{1,2})?-\[\d/,
    msg: "不要写任意圆角，改用 rounded-card / rounded-3xl / rounded-tile / rounded-field / rounded-full",
  },
  {
    id: "arbitrary-shadow",
    re: /\bshadow-\[/,
    msg: "不要写任意阴影，改用 shadow-card / shadow-float / shadow-pop",
  },
  {
    id: "inline-color-style",
    re: /style=\{\{[^}]*\b(?:color|background|backgroundColor|borderColor)\s*:/,
    msg: "不要用内联 style 设置颜色，改用 token 类名（内联 style 只用于动态的宽度/位置）",
  },
  {
    id: "spa-router",
    re: /from\s+["'](?:react-router|react-router-dom|next\/[a-z]+|@tanstack\/react-router)["']/,
    msg: "保留现有 hash 导航，不要引入新的前端路由库",
  },
];

/** 所有可交付源码都扫描；只有令牌定义文件可声明原始设计值。 */
const cssRules = [
  { id: "css-color", re: /(?:(?:#|%23)[0-9a-f]{3,8}\b|(?:rgba?|hsla?|oklch)\()/i, msg: "CSS 颜色集中在 theme/tokens.css" },
  { id: "css-font-size", re: /font-size\s*:\s*\d+(?:\.\d+)?(?:px|rem)\b/, msg: "CSS 字号使用 --text-* 语义令牌" },
  { id: "css-radius", re: /border-radius\s*:[^;{}]*\b[1-9]\d*(?:px|%)/, msg: "CSS 圆角使用 --r-* 语义令牌" },
  { id: "css-shadow", re: /box-shadow\s*:\s*(?!\s|none\b|var\()[^;{}]+/, msg: "CSS 阴影使用共享语义令牌" },
];

export function violations(name, text) {
  const active = name.endsWith(".css") ? name === "src/theme/tokens.css" ? [] : cssRules : rules;
  return text.split("\n").flatMap((line, index) => {
    if (/^\s*(?:\/\/|\*|\/\*)/.test(line)) return [];
    return active.filter(rule => rule.re.test(line)).map(rule => ({ id: rule.id, line: index + 1, message: rule.msg, source: line.trim() }));
  });
}

function walk(dir) {
  return readdirSync(dir).flatMap(name => {
    const path = join(dir, name);
    return statSync(path).isDirectory() ? walk(path) : /\.(?:tsx?|jsx?|css)$/.test(name) ? [path] : [];
  });
}

if (process.argv[1] === new URL(import.meta.url).pathname) {
  const problems = walk(SRC).flatMap(file => {
    const name = relative(ROOT, file);
    return violations(name, readFileSync(file, "utf8")).map(problem => ({ name, ...problem }));
  });
  for (const item of problems) console.log(`${item.name}:${item.line} [${item.id}] ${item.message}\n  ${item.source.slice(0, 180)}`);
  if (problems.length) {
    console.log(`\n✗ ${problems.length} 处设计令牌违规`);
    process.exitCode = 1;
  } else console.log("✓ 全源码设计令牌检查通过");
}
