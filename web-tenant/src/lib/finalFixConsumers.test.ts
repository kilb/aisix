import assert from "node:assert/strict";
import { test } from "node:test";
import { readFileSync } from "node:fs";
import ts from "typescript";
import * as React from "react";

// 直接执行消费者的函数/JSX，不复制生产分支；异步依赖只在单元测试中受控。
function source(file: string) {
  return ts.createSourceFile(file, readFileSync(new URL(file, import.meta.url), "utf8"), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
}
function find(file: string, predicate: (node: ts.Node) => boolean): ts.Node {
  const root = source(file);
  let found: ts.Node | undefined;
  function visit(node: ts.Node) { if (!found && predicate(node)) found = node; if (!found) ts.forEachChild(node, visit); }
  visit(root);
  assert.ok(found, `consumer missing: ${file}`);
  return found;
}
function execute(node: ts.Node, bindings: Record<string, unknown>) {
  const expression = node.getText().replace(/^export /, "");
  const js = ts.transpileModule(`const consumer = (${expression});`, { compilerOptions: { target: ts.ScriptTarget.ES2022, jsx: ts.JsxEmit.React } }).outputText;
  return new Function(...Object.keys(bindings), `${js}; return consumer;`)(...Object.values(bindings));
}
function handler(file: string, name: string, bindings: Record<string, unknown>) {
  return execute(find(file, node => ts.isFunctionDeclaration(node) && node.name?.text === name), bindings);
}
function select(file: string, value: string, bindings: Record<string, unknown>) {
  const node = find(file, node => ts.isJsxElement(node) && node.openingElement.tagName.getText() === "select" && node.openingElement.attributes.properties.some(prop => ts.isJsxAttribute(prop) && prop.name.getText() === "value" && prop.initializer?.getText() === `{${value}}`));
  return execute(node, { React, ...bindings }) as React.ReactElement<{ value: string; disabled?: boolean; children: React.ReactNode; onChange: (event: { target: { value: string } }) => void }>;
}
function optionValues(node: React.ReactNode): string[] {
  return React.Children.toArray(node).flatMap(child => React.isValidElement<{ value: string; children: React.ReactNode }>(child) ? child.type === "option" ? [child.props.value] : optionValues(child.props.children) : []);
}
const noop = () => undefined;

for (const [value, bindings, field] of [
  ["value", { value: "historic-key", keys: [] }, "api_key_id"],
  ['query.get("model_id") ?? ""', { models: [], metricsFrom: "from", metricsTo: "to" }, "model_id"],
  ["modelId", { modelId: "historic-model", models: [] }, "model_id"],
] as const) test(`门户 ${value} 保留 URL 未知 ID，清除更新同一范围`, () => {
  const id = field === "api_key_id" ? "historic-key" : "historic-model";
  const query = new URLSearchParams(`${field}=${id}&member_id=member`);
  const update = (patch: Record<string, string | undefined>) => { for (const [key, value] of Object.entries(patch)) value ? query.set(key, value) : query.delete(key); };
  const node = select("../Usage.tsx", value, { ...bindings, query, update, onChange: (value: string) => update({ [field]: value }), setModelId: (value: string) => update({ model_id: value }) });
  assert.ok(optionValues(node.props.children).includes(id), "当前候选缺失时也必须存在匹配的 option");
  assert.equal(query.get(field), id);
  node.props.onChange({ target: { value: "" } });
  assert.equal(query.has(field), false);
  assert.equal(query.get("member_id"), "member");
});

test("主题颜色在过渡保护内同步提交并刷新样式，普通交互立即恢复", () => {
  const attributes = new Map([["data-theme", "light"]]);
  const flushes: { theme: string | undefined; protected: boolean }[] = [];
  const root = {
    getAttribute: (name: string) => attributes.get(name) ?? null,
    setAttribute: (name: string, value: string) => { attributes.set(name, value); },
    removeAttribute: (name: string) => { attributes.delete(name); },
    get offsetHeight() { flushes.push({ theme: attributes.get("data-theme"), protected: attributes.has("data-theme-changing") }); return 100; },
  };
  handler("./useTheme.ts", "useTheme", {
    stored: () => "dark", useState: () => ["dark", noop], useCallback: (fn: unknown) => fn,
    useLayoutEffect: (fn: () => void) => fn(), document: { documentElement: root },
  })();
  assert.ok(flushes.some(frame => frame.theme === "dark" && frame.protected), "改变主题的 style flush 必须阻止颜色插值");
  assert.equal(attributes.has("data-theme-changing"), false, "保护只跨过样式提交，不影响后续 hover/抽屉");
  assert.equal(attributes.get("data-theme"), "dark");
  const css = readFileSync(new URL("../theme/tokens.css", import.meta.url), "utf8");
  const protection = css.match(/\[data-theme-changing\][^{]*\{([^}]*)\}/s)?.[1];
  assert.ok(protection);
  const propertyList = protection.match(/transition-property:\s*([^;!]+)!important/)?.[1];
  assert.ok(propertyList);
  const properties = propertyList.split(",").map(value => value.trim());
  for (const color of ["all", "color", "background-color", "border-color", "fill", "stroke"]) assert.equal(properties.includes(color), false);
  for (const motion of ["opacity", "transform"]) assert.equal(properties.includes(motion), true);
});

test("门户候选加载后不重复已选值，不把未知 ID 当成全部", () => {
  for (const [value, bindings] of [
    ["value", { value: "known", keys: [{ id: "known", label: "密钥" }], onChange: noop }],
    ['query.get("model_id") ?? ""', { query: new URLSearchParams("model_id=known"), models: [{ id: "known", name: "模型" }], update: noop }],
    ["modelId", { modelId: "known", models: [{ id: "known", name: "模型" }], setModelId: noop }],
  ] as const) {
    const node = select("../Usage.tsx", value, bindings);
    assert.equal(optionValues(node.props.children).filter(id => id === "known").length, 1);
    assert.equal(node.props.value, "known");
  }
});
