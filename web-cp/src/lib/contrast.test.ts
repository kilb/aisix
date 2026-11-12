import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { contrastRatio, readTokens } from "./contrast.ts";

// `npm run test:unit`. The token file is read as shipped, so a colour edit
// that breaks AA fails here before any page is looked at.

// Every text pair the pages draw, in both themes, at AA (4.5:1).
test("令牌配色在两种主题下都达到 AA 对比度", () => {
  const css = readFileSync(new URL("../theme/tokens.css", import.meta.url), "utf8");
  for (const theme of ["light", "dark"] as const) {
    const T = readTokens(css, theme);
    const pairs: [string, string][] = [
      ["--foreground", "--background"],
      ["--foreground", "--card"],
      ["--foreground", "--muted"],
      ["--foreground", "--secondary"],
      ["--card-foreground", "--card"],
      ["--popover-foreground", "--popover"],
      ["--primary-foreground", "--primary"],
      ["--destructive-foreground", "--destructive"],
      ["--muted-foreground", "--background"],
      ["--muted-foreground", "--card"],
      ["--muted-foreground", "--muted"],
      ["--text-2", "--card"],
      ["--tooltip-text", "--tooltip-bg"],
    ];
    for (const [fg, bg] of pairs)
      assert.ok(contrastRatio(T[fg]!, T[bg]!) >= 4.5, `${theme} ${fg} on ${bg}: ${contrastRatio(T[fg]!, T[bg]!).toFixed(2)}`);
  }
});

// State colours are text too: on a panel, on the page, on a hovered row and
// on their own soft fill (a badge, a note).
test("状态色文字在面板、页面、悬停行和自己的浅底上都达到 AA", () => {
  const css = readFileSync(new URL("../theme/tokens.css", import.meta.url), "utf8");
  for (const theme of ["light", "dark"] as const) {
    const T = readTokens(css, theme);
    for (const fg of ["--info", "--success", "--warning", "--danger", "--link", "--error"])
      for (const bg of ["--card", "--background", "--row-hover"])
        assert.ok(contrastRatio(T[fg]!, T[bg]!) >= 4.5, `${theme} ${fg} on ${bg}: ${contrastRatio(T[fg]!, T[bg]!).toFixed(2)}`);
    for (const k of ["info", "success", "warning", "danger"])
      assert.ok(
        contrastRatio(T[`--${k}`]!, T[`--${k}-soft`]!) >= 4.5,
        `${theme} --${k} on --${k}-soft: ${contrastRatio(T[`--${k}`]!, T[`--${k}-soft`]!).toFixed(2)}`,
      );
  }
});

// Non-text: a field's edge, the focus ring and the group bars need 3:1.
test("输入框边、焦点环与分组色条在两种主题的面板上都达到 3:1", () => {
  const css = readFileSync(new URL("../theme/tokens.css", import.meta.url), "utf8");
  for (const theme of ["light", "dark"] as const) {
    const T = readTokens(css, theme);
    for (const fg of ["--input", "--ring", "--route-1", "--route-2", "--route-3", "--route-4", "--route-5", "--route-6"])
      for (const bg of ["--card", "--background"])
        assert.ok(contrastRatio(T[fg]!, T[bg]!) >= 3, `${theme} ${fg} on ${bg}: ${contrastRatio(T[fg]!, T[bg]!).toFixed(2)}`);
  }
});

test("对比度按 WCAG 公式计算", () => {
  assert.equal(contrastRatio("#000000", "#FFFFFF").toFixed(2), "21.00");
  assert.equal(contrastRatio("#FFFFFF", "#FFFFFF").toFixed(2), "1.00");
});

// 资源编辑行实际用 bg-well + text-muted；缺少局部语义时应抓住4.48:1回退。
test("凹槽局部次级文字在浅深主题及嵌套面板上保持 AA", () => {
  const css = readFileSync(new URL("../theme/tokens.css", import.meta.url), "utf8");
  const ui = readFileSync(new URL("../ui/ui.css", import.meta.url), "utf8");
  const scoped = /\.bg-well\s*\{([^}]+)\}/.exec(ui)?.[1] ?? "";
  for (const theme of ["light", "dark"] as const) {
    const global = readTokens(css, theme);
    const local = readTokens(`${css}\n:root {${scoped}}`, theme);
    for (const fg of ["--muted-foreground", "--text-muted"])
      for (const bg of ["--well", "--card", "--background", "--info-soft", "--success-soft", "--warning-soft", "--danger-soft"])
        assert.ok(contrastRatio(local[fg]!, local[bg]!) >= 4.5, `${theme} 凹槽 ${fg} on ${bg}: ${contrastRatio(local[fg]!, local[bg]!).toFixed(4)}`);
    assert.equal(local["--bento-muted"], global["--bento-muted"]);
    for (const semantic of ["--success", "--warning", "--danger"])
      assert.equal(local[semantic], global[semantic]);
  }
});
