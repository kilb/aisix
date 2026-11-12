import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { build } from "esbuild";
import { createElement, type ComponentType } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { draftOf, routingBody } from "./publicRouting.ts";

// 只为测试导出私有表单，渲染真实实现，不替换控件、状态或后端响应。
const require = createRequire(import.meta.url);
const source = await readFile(new URL("../tabs/Models.tsx", import.meta.url), "utf8");
const compiled = await build({
  stdin: {
    contents: `${source}\nexport { ModelForm }; export { PublicRoutingForm } from "./models/PublicRoutingForm.tsx"; export { CooldownEditor } from "../ui/CooldownEditor.tsx";`,
    resolveDir: fileURLToPath(new URL("../tabs/", import.meta.url)), sourcefile: "default-placeholder-forms.tsx", loader: "tsx",
  },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  loader: { ".css": "empty" }, define: { "import.meta.env.BASE_URL": '"/"' },
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, (args) => ({ path: require.resolve(args.path), external: true })); } }],
});
const loaded = { exports: {} };
new Function("require", "module", "exports", compiled.outputFiles[0]!.text)(require, loaded, loaded.exports);
const { ModelForm, PublicRoutingForm, CooldownEditor } = loaded.exports as Record<string, ComponentType<any>>;

function inputOf(markup: string, label: string): string {
  const item = (markup.match(/<label\b[\s\S]*?<\/label>/g) ?? []).find((item) => item.replace(/<[^>]+>/g, "").startsWith(label));
  assert.ok(item, `缺少输入标签：${label}`);
  const input = item.match(/<input\b[^>]*>/)?.[0];
  assert.ok(input, `缺少输入：${label}`);
  return input;
}

const publicProps = {
  publicModel: {
    id: "placeholder-public", name: "public-model", routing: {}, effective_routing: { strategy: "round_robin" },
    served_by: ["a", "b", "c"].map((id) => ({ provider_key_id: id, display_name: id, groups: [], upstream_model: null, priority: 0, weight: 1, enabled: true })),
  },
  catalog: { models: new Map(), accounts: new Map() },
  schema: { strategies: ["round_robin"], preset: "round_robin" },
  writeGate: { busy: false, begin: () => true, end: () => undefined },
  readOnly: false, onClose: () => undefined, onSaved: async () => undefined, onTargetRemoved: async () => undefined,
};

test("empty routing fields display contextual defaults without filling or persisting them", () => {
  const markup = renderToStaticMarkup(createElement(PublicRoutingForm!, publicProps));
  const retries = inputOf(markup, "重试次数");
  assert.match(retries, /placeholder="目标未设且有后备时 0"/);
  assert.match(retries, /value=""/);
  const fallbacks = inputOf(markup, "最多切换几个目标");
  assert.match(fallbacks, /placeholder="2"/);
  assert.match(fallbacks, /value=""/);
  assert.deepEqual(routingBody(draftOf({}, "round_robin"), {}, "round_robin", []), {});
});

test("fallback placeholder follows configured active target count including no targets", () => {
  const markup = renderToStaticMarkup(createElement(PublicRoutingForm!, {
    ...publicProps, publicModel: { ...publicProps.publicModel, served_by: [{ ...publicProps.publicModel.served_by[0], enabled: false }] },
  }));
  assert.match(inputOf(markup, "最多切换几个目标"), /placeholder="0"/);
});

test("new manual routing target shows weight 1 and priority 0 while keeping both fields empty", () => {
  const markup = renderToStaticMarkup(createElement(ModelForm!, {
    model: null, shape: "routing", catalog: publicProps.catalog, schema: publicProps.schema,
    readOnly: false, create: async () => undefined, save: async () => undefined,
    onSaved: () => undefined, onClose: () => undefined, onReload: () => undefined,
  }));
  for (const [label, expected] of [["权重", "1"], ["优先级", "0"]]) {
    const input = inputOf(markup, label!);
    assert.match(input, new RegExp(`placeholder="${expected}"`));
    assert.match(input, /value=""/);
  }
});

test("enabled cooldown exposes static defaults as placeholders and retains an unset configuration", () => {
  const value = { enabled: true };
  const markup = renderToStaticMarkup(createElement(CooldownEditor!, { value, disabled: false, onChange: () => { throw new Error("渲染不得保存默认值"); } }));
  assert.match(inputOf(markup, "默认冷却秒数"), /placeholder="30"[^>]*value=""/);
  assert.match(inputOf(markup, "Retry-After 上限秒数"), /placeholder="600"[^>]*value=""/);
  assert.deepEqual(value, { enabled: true });
});

test("explicit zero values stay visible instead of being replaced by placeholder defaults", () => {
  const markup = renderToStaticMarkup(createElement(PublicRoutingForm!, {
    ...publicProps, publicModel: { ...publicProps.publicModel, routing: { retries: 0, max_fallbacks: 0 } },
  }));
  assert.match(inputOf(markup, "重试次数"), /value="0"/);
  assert.match(inputOf(markup, "最多切换几个目标"), /value="0"/);
});
