import assert from "node:assert/strict";
import { test } from "node:test";
import { build } from "esbuild";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

// SSR 只检查首屏可访问控件，不运行 effect 或替换后端。
const compiled = await build({
  stdin: {
    contents: `
      import { createElement } from "react";
      import { renderToStaticMarkup } from "react-dom/server";
      import { Keys, when } from "./Keys.tsx";
      import { Members } from "./Members.tsx";
      import { Usage } from "./Usage.tsx";
      export { when };
      const member = { id: "member-1", email: "owner@example.test", display_name: "Owner", role: "owner" };
      const props = { member, status: "active", onSelfChanged() {}, onLeft() {} };
      export function renderPage(page) {
        return renderToStaticMarkup(createElement({ Keys, Members, Usage }[page], props));
      }
    `,
    resolveDir: fileURLToPath(new URL("../", import.meta.url)),
    sourcefile: "tenant-records-ssr.ts",
  },
  bundle: true,
  platform: "node",
  format: "cjs",
  jsx: "automatic",
  define: { "import.meta.env.BASE_URL": '"/"' },
  loader: { ".css": "empty" },
  write: false,
});
const module = { exports: {} };
new Function("require", "module", "exports", compiled.outputFiles[0]!.text)(createRequire(import.meta.url), module, module.exports);
const { renderPage, when } = module.exports as { renderPage: (page: string) => string; when: (iso: string) => string };

test("密钥时间跨日固定按北京时间显示", () => {
  assert.equal(when("2026-11-11T16:05:06Z"), "2026-11-12 00:05:06 北京时间");
});

test("密钥和成员首屏提供公开字段搜索，邀请独立筛选", () => {
  assert.match(renderPage("Keys"), /搜索密钥/);
  const members = renderPage("Members");
  assert.match(members, /搜索成员/);
  assert.match(members, /搜索邀请/);
  assert.match(members, /北京时间月初/);
});

test("用量汇总和调用记录都有独立搜索并解释真实估算来源", () => {
  const usage = renderPage("Usage");
  assert.match(usage, /搜索汇总模型/);
  assert.match(usage, /搜索已加载调用/);
  assert.match(usage, /上游未完整返回用量，部分 token 由网关估算，费用按该用量计费/);
  assert.doesNotMatch(usage, /日期（UTC）|时间（UTC）/);
});
