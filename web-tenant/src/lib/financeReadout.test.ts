import assert from "node:assert/strict";
import { after, test } from "node:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

// 连同渲染器一起编译实际组件，保持同一 React 实例，不替换业务组件。
const compiled = await build({
  stdin: {
    contents: `
      import { createElement } from "react";
      import { renderToStaticMarkup } from "react-dom/server";
      import { Money, BalanceReadout } from "./ui/BalanceReadout.tsx";
      export function money(micro) { return renderToStaticMarkup(createElement(Money, { micro })); }
      export function readout(props) { return renderToStaticMarkup(createElement(BalanceReadout, props)); }
    `,
    resolveDir: fileURLToPath(new URL("../", import.meta.url)),
    sourcefile: "finance-readout-component.ts",
  },
  bundle: true,
  platform: "node",
  format: "cjs",
  jsx: "automatic",
  write: false,
});
const temporary = await mkdtemp(join(tmpdir(), "aisix-finance-readout-"));
after(() => rm(temporary, { recursive: true, force: true }));
const entry = join(temporary, "component.cjs");
await writeFile(entry, compiled.outputFiles[0]!.text);
const { money, readout } = createRequire(import.meta.url)(entry) as {
  money: (micro: number | string | bigint) => string;
  readout: (props: {
    effectiveMicroCny: number | string | bigint | null;
    stops: { level: "organization" | "member" | "key"; label: string; remainingMicroCny: number | string | bigint | null }[];
    binding: "organization" | "member" | "key" | null;
  }) => string;
};

function text(markup: string): string {
  return markup.replace(/<[^>]+>/g, "");
}

test("金额组件拒绝已舍入的旧number与非法整数，不伪装为零", () => {
  for (const value of [Number.MAX_SAFE_INTEGER + 1, Number.NaN, Number.POSITIVE_INFINITY, "0.5", "invalid"]) {
    assert.equal(text(money(value)), "未知");
  }
  assert.equal(text(money(0)), "¥0.00");
  assert.equal(text(money("9007199254740993")), "¥9,007,199,254.740993");
  assert.equal(text(money(-9007199254740993n)), "-¥9,007,199,254.740993");
});

test("实际可用额度分别保留不限、零与未知，不丢失精确值", () => {
  const render = (effectiveMicroCny: number | string | bigint | null) => readout({ effectiveMicroCny, stops: [], binding: null });
  assert.match(render(null), /data-testid="readout-amount"[^]*?>不限</);
  assert.match(text(render(0)), /现在还能花¥0\.00/);
  assert.match(text(render(Number.MAX_SAFE_INTEGER + 1)), /现在还能花未知/);
  assert.match(text(render("9007199254740993")), /现在还能花¥9,007,199,254\.740993/);
});

test("约束层级标记绑定方，标题不携带无语义编号", () => {
  const markup = readout({
    effectiveMicroCny: "0",
    binding: "key",
    stops: [
      { level: "organization", label: "组织", remainingMicroCny: "9007199254740993" },
      { level: "key", label: "密钥", remainingMicroCny: 0 },
    ],
  });
  assert.match(markup, /<h2[^>]*><span>可用额度<\/span><\/h2>/);
  assert.doesNotMatch(markup, /aria-hidden="true">02<\/span>/);
  assert.match(markup, /data-level="organization" data-binding="false"/);
  assert.match(markup, /data-level="key" data-binding="true"/);
  assert.equal((text(markup).match(/以此为准/g) ?? []).length, 1);
  assert.match(text(markup), /¥9,007,199,254\.740993/);
});
