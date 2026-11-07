import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { contrastRatio, readTokens } from "./contrast.ts";

// `npm run test:unit`. The token file is read as shipped, so a colour edit
// that breaks AA fails here before any page is looked at.

test("令牌配色在两种主题下都达到 AA 对比度", () => {
  const css = readFileSync(new URL("../theme/tokens.css", import.meta.url), "utf8");
  const L = readTokens(css, "light"), D = readTokens(css, "dark");
  // text pairs ≥ 4.5
  for (const [fg, bg] of [["--ink", "--paper"], ["--ink", "--concrete"], ["--ink", "--sign"], ["--n2", "--paper"],
                          ["--n2", "--concrete"], ["--line", "--paper"], ["--stop", "--paper"]] as [string, string][])
    assert.ok(contrastRatio(L[fg]!, L[bg]!) >= 4.5, `${fg} on ${bg}`);
  for (const [fg, bg] of [["--text", "--bg"], ["--text", "--surface"], ["--text-muted", "--surface"],
                          ["--line", "--surface"], ["--stop", "--surface"]] as [string, string][])
    assert.ok(contrastRatio(D[fg]!, D[bg]!) >= 4.5, `dark ${fg} on ${bg}`);
  // non-text pairs (lamps, focus ring, input borders) ≥ 3
  for (const [fg, bg] of [["--proceed", "--paper"], ["--line", "--paper"], ["--input-border", "--paper"]] as [string, string][])
    assert.ok(contrastRatio(L[fg]!, L[bg]!) >= 3, `${fg} on ${bg}`);
  assert.ok(contrastRatio(D["--proceed"]!, D["--surface"]!) >= 3);
});

test("对比度按 WCAG 公式计算", () => {
  assert.equal(contrastRatio("#000000", "#FFFFFF").toFixed(2), "21.00");
  assert.equal(contrastRatio("#FFFFFF", "#FFFFFF").toFixed(2), "1.00");
});
