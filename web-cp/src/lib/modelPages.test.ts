import assert from "node:assert/strict";
import { after, test } from "node:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { createElement, type ComponentType } from "react";
import { renderToStaticMarkup } from "react-dom/server";

// 渲染真实页面的初始状态；恢复测试入口或公共模型价格分区必须使检查变红。
const require = createRequire(import.meta.url);
const compiled = await build({
  stdin: { contents: 'export { Models } from "./tabs/Models.tsx"; export { ModelTests } from "./tabs/ModelTests.tsx"; export { PublicRoutes } from "./tabs/PublicRoutes.tsx"; export { PublicRoutingForm } from "./tabs/models/PublicRoutingForm.tsx"; export { SECTIONS } from "./tabs/Pricing.tsx"; export { DirectModelProbe } from "./components/DirectModelProbe.tsx";', resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "model-pages.tsx" },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  loader: { ".css": "empty" }, define: { "import.meta.env.BASE_URL": '"/"' },
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, args => ({ path: require.resolve(args.path), external: true })); } }],
});
const temporary = await mkdtemp(join(tmpdir(), "aisix-model-pages-"));
after(() => rm(temporary, { recursive: true, force: true }));
const entry = join(temporary, "component.cjs");
await writeFile(entry, compiled.outputFiles[0]!.text);
const { Models, ModelTests, PublicRoutes, PublicRoutingForm, SECTIONS, DirectModelProbe } = require(entry) as { Models: ComponentType<any>; ModelTests: ComponentType<any>; PublicRoutes: ComponentType<any>; PublicRoutingForm: ComponentType<any>; SECTIONS: { id: string }[]; DirectModelProbe: ComponentType<any> };
const props = { canEdit: true, operator: { role: "admin" }, onGoto: () => undefined };

test("模型目录没有测试入口，仍可维护手工模型", () => {
  const markup = renderToStaticMarkup(createElement(Models, props));
  assert.doesNotMatch(markup, /直连模型测试|测试所有直连模型|测试历史/);
  assert.match(markup, /新增路由模型/);
  assert.match(markup, /新增直连模型/);
});

test("公共模型生命周期不再藏在定价子区", () => {
  assert.ok(SECTIONS.some((section) => section.id === "baselines"));
  assert.ok(!SECTIONS.some((section) => section.id === "public-models"));
});

test("历史视图保留当前测试面板挂载但隐藏，非管理员没有发起按钮", () => {
  const markup = renderToStaticMarkup(createElement(DirectModelProbe, { models: [], accounts: [], loadError: false, canProbe: false, probe: () => { throw new Error("页面不得自动发起测试"); }, section: "history" }));
  assert.match(markup, /id="model-tests-current"[^>]*hidden=""/);
  assert.match(markup, /id="model-tests-history"[^>]*role="tabpanel"/);
  assert.doesNotMatch(markup, /<button[^>]*>[^<]*(?:测试所有直连模型|重新测试所有直连模型)/);
  assert.match(markup, /仅管理员可发起测试/);
});

// 丢掉深链或把历史页误设为管理员专用时，真实页面的初始渲染会丢失选中状态。
test("独立测试页直接打开历史，viewer/operator仍有完整导航", () => {
  for (const role of ["viewer", "operator"]) {
    const markup = renderToStaticMarkup(createElement(ModelTests, { ...props, canEdit: role === "operator", operator: { role }, initialSection: "history" }));
    assert.match(markup, /id="model-tests-history-tab" aria-selected="true"/);
    assert.match(markup, /id="model-tests-current"[^>]*hidden=""/);
    assert.match(markup, /aria-label="测试历史"/);
    assert.doesNotMatch(markup, /重新测试所有直连模型|测试所有直连模型/);
  }
});

test("公共模型与路由有独立页面，保持生命周期入口权限", () => {
  const editable = renderToStaticMarkup(createElement(PublicRoutes, props));
  assert.match(editable, /公共模型与路由/);
  assert.match(editable, /aria-label="新建公共模型"/);
  const viewer = renderToStaticMarkup(createElement(PublicRoutes, { ...props, canEdit: false, operator: { role: "viewer" } }));
  assert.match(viewer, /公共模型与路由/);
  assert.doesNotMatch(viewer, /aria-label="新建公共模型"/);
});

test("提取后的公共表单保留现有路由策略，支持首个目标与只读查看", () => {
  const formProps = {
    publicModel: { id: "public", name: "public-model", served_by: [], routing: { strategy: "failover" }, effective_routing: { strategy: "failover" } },
    catalog: { models: new Map(), accounts: new Map() },
    schema: { strategies: ["failover", "consistent_hash", "round_robin"], preset: "failover" },
    writeGate: { busy: false, begin: () => true, end: () => undefined },
    readOnly: false, onClose: () => undefined, onSaved: () => undefined, onTargetRemoved: () => undefined,
  };
  const editable = renderToStaticMarkup(createElement(PublicRoutingForm, formProps));
  assert.match(editable, /同级分配方式/);
  assert.match(editable, /value="failover" selected=""/);
  assert.match(editable, /添加目标/);
  const viewer = renderToStaticMarkup(createElement(PublicRoutingForm, { ...formProps, readOnly: true }));
  assert.match(viewer, /同级分配方式/);
  assert.doesNotMatch(viewer, /添加目标<|>保存</);
});

// 单纯只锁输入不够；另一个写入口进行中时，目标操作按钮也必须不可用。
test("共享写入占用时公共表单禁用移除和添加入口", () => {
  const markup = renderToStaticMarkup(createElement(PublicRoutingForm, {
    publicModel: { id: "public", name: "public-model", served_by: [{ provider_key_id: "account", display_name: "account", provider: "openai", groups: [], upstream_model: null, priority: 100, weight: 1, enabled: true }], routing: { strategy: "failover" } },
    catalog: { models: new Map(), accounts: new Map() },
    schema: { strategies: ["failover"], preset: "failover" },
    writeGate: { busy: true, begin: () => false, end: () => undefined },
    readOnly: false, onClose: () => undefined, onSaved: () => undefined, onTargetRemoved: () => undefined,
  }));
  const buttons = markup.match(/<button\b[^>]*>[\s\S]*?<\/button>/g) ?? [];
  for (const name of ["移除该目标", "添加目标"]) {
    const button = buttons.find((value) => value.replace(/<[^>]+>/g, "").trim() === name);
    assert.ok(button, name);
    assert.match(button, /<button\b[^>]*\bdisabled=""/, name);
  }
});
