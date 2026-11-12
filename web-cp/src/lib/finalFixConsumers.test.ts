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

import { createPublicModelWriteGate, runPublicModelWrite } from "./publicModelWriteGate.ts";
import { chargesQuery } from "./analysisNavigation.ts";

test("删除成功但 GET 拒绝：读回结束前 gate 不释放，之后重新生成不写回已删账号", async () => {
  const gate = createPublicModelWriteGate({ current: false }, noop);
  const removed = { provider_key_id: "removed", upstream_model: "a", priority: 1, weight: 1, enabled: true };
  const kept = { ...removed, provider_key_id: "kept" };
  const original = { id: "model", name: "模型", served_by: [removed, kept], unknown_field: { untouched: true } };
  let current = original;
  let release!: () => void;
  const reload = new Promise<void>(resolve => { release = resolve; });
  const errors: unknown[] = [];
  const notes: { kind: string; text: string }[] = [];
  const callback = execute(find("../tabs/PublicRoutes.tsx", node => ts.isJsxAttribute(node) && node.name.getText() === "onTargetRemoved").getChildren().find(node => ts.isJsxExpression(node))!.getChildren().find(node => ts.isArrowFunction(node))!, {
    model: original, setNote: (note: { kind: string; text: string }) => notes.push(note), apply: (view: typeof original) => { current = view; }, reload: () => reload.then(() => false), models: { reload: async () => undefined },
  });
  const remove = handler("../tabs/models/PublicRoutingForm.tsx", "removeTarget", {
    writeGate: gate, runPublicModelWrite, publicModel: original,
    channelModels: { remove: async () => undefined }, publicModels: { get: async () => { throw new Error("GET failed"); } },
    setRemovingBusy: noop, setRefusals: noop, rowRefusal: noop, setRemoving: noop, setEdits: noop, without: noop, settle: noop, setErr: (error: unknown) => errors.push(error), onTargetRemoved: callback,
  });
  const removing = remove("removed", "账号");
  await new Promise(resolve => setImmediate(resolve));
  const appliedWhilePending = current;
  const blockedWhilePending = !gate.begin();
  if (!blockedWhilePending) gate.end();
  release();
  await removing;
  assert.equal(blockedWhilePending, true, "不能在父列表读回完成前允许第二次写入");
  assert.deepEqual(appliedWhilePending.served_by, [kept], "GET 失败也必须应用 DELETE 已知移除结果");
  assert.equal(current.unknown_field, original.unknown_field);
  assert.ok(notes.some(note => note.kind === "warn" && note.text.includes("读回失败")), "父列表失败也不能当作已完整读回");
  assert.ok(errors.length > 0, "读回失败必须可见，不能假报完整读回成功");
  const puts: string[] = [];
  await handler("../tabs/pricing/PublicModelDetail.tsx", "regenerate", {
    writeGate: gate, runPublicModelWrite, model: current, setBusy: noop, setError: noop, setNote: noop, toast: noop,
    channelModels: { put: async (id: string) => { puts.push(id); } }, onChanged: async () => undefined,
  })();
  assert.deepEqual(puts, ["kept"]);
});

for (const operation of ["create", "remove", "restore"]) test(`详情 pending 后回列表：${operation} 的消费者同步拒绝，完成后可写`, async () => {
  const gate = createPublicModelWriteGate({ current: false }, noop);
  let writes = 0;
  const action = handler("../tabs/pricing/PublicModels.tsx", operation, {
    writeGate: gate, runPublicModelWrite, canEdit: true, name: "new", setCreating: noop, setRefusal: noop, setAtName: noop, setName: noop, toast: noop, select: noop, load: async () => true, mark: noop,
    publicModels: { create: async () => { writes++; return { id: "new", name: "new" }; }, delete: async () => { writes++; }, restore: async () => { writes++; } },
  });
  gate.begin();
  await action(operation === "create" ? { preventDefault: noop } : { id: "model", name: "model" });
  const pendingWrites = writes;
  gate.end();
  await action(operation === "create" ? { preventDefault: noop } : { id: "model", name: "model" });
  assert.equal(pendingWrites, 0, "禁止只依赖 disabled；旧 handler 也不能写");
  assert.equal(writes, 1);
});

for (const field of ["tenant_id", "api_key_id"]) test(`CP ${field} 未加载/不存在也显示原筛选，key-only 可清除`, () => {
  const query = new URLSearchParams(`${field}=historic-id`);
  const filter = chargesQuery(query);
  const node = select("../tabs/billing/Charges.tsx", `filter.${field} ?? ""`, { filter, tenants: [], keys: [], set: (patch: Record<string, string | undefined>) => { for (const [key, value] of Object.entries(patch)) value ? query.set(key, value) : query.delete(key); } });
  assert.ok(optionValues(node.props.children).includes("historic-id"), "缺少匹配 option 时 React 会显示全部");
  assert.notEqual(node.props.disabled, true, "URL key-only 必须能清除");
  assert.equal(chargesQuery(query)[field], "historic-id");
  node.props.onChange({ target: { value: "" } });
  assert.equal(Object.hasOwn(chargesQuery(query), field), false);
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

// 应用写入投影后，早先发出的完整列表不能把已移除账号带回来。
test("父列表丢弃写入前发出的旧读，失败读回返回 false 并保留已应用投影", async () => {
  const latest = { current: 0 };
  const old = { id: "model", served_by: [{ provider_key_id: "removed" }] };
  const updated = { id: "model", served_by: [] };
  let rows = [old];
  const errors: unknown[] = [];
  let resolve!: (value: typeof rows) => void;
  const pending = new Promise<typeof rows>(done => { resolve = done; });
  let fails = false;
  const setRows = (next: typeof rows | ((previous: typeof rows) => typeof rows)) => { rows = typeof next === "function" ? next(rows) : next; };
  const loadVariable = find("../tabs/pricing/PublicModels.tsx", node => ts.isVariableDeclaration(node) && node.name.getText() === "load") as ts.VariableDeclaration;
  const load = execute((loadVariable.initializer as ts.CallExpression).arguments[0]!, {
    latest, setRows, setPrices: noop, setLiveGroups: noop, setError: (error: unknown) => errors.push(error), errorText: (error: Error) => error.message,
    publicModels: { list: () => fails ? Promise.reject(new Error("list failed")) : pending }, groupPrices: { list: async () => [] }, groupsApi: { list: async () => [] },
  });
  const apply = execute(find("../tabs/pricing/PublicModels.tsx", node => ts.isArrowFunction(node) && node.body.getText().includes("latest.current++")), { latest, setRows });
  const reading = load();
  apply(updated);
  resolve([old]);
  assert.equal(await reading, false);
  assert.deepEqual(rows, [updated]);
  fails = true;
  assert.equal(await load(), false);
  assert.deepEqual(rows, [updated]);
  assert.deepEqual(errors, ["list failed"]);
});

for (const operation of ["create", "remove", "restore"]) test(`${operation} 写入失败也释放同步门禁，仍报告原拒绝`, async () => {
  const gate = createPublicModelWriteGate({ current: false }, noop);
  const refusals: unknown[] = [];
  const failure = async () => { throw new Error("refused"); };
  const action = handler("../tabs/pricing/PublicModels.tsx", operation, {
    writeGate: gate, runPublicModelWrite, canEdit: true, name: "new", setCreating: noop, setRefusal: (value: unknown) => refusals.push(value), setAtName: noop, mark: noop, focusRefusal: noop, load: async () => false,
    ApiError: class ApiError extends Error {}, isUnknownOutcome: () => false, errorText: (error: Error) => error.message,
    publicModels: { create: failure, delete: failure, restore: failure },
  });
  await action(operation === "create" ? { preventDefault: noop } : { id: "model", name: "model" });
  assert.ok(refusals.includes("refused"));
  assert.equal(gate.begin(), true);
  gate.end();
});

test("CP 候选加载后只保留一个已选 option，未选密钥且无组织仍禁用", () => {
  for (const field of ["tenant_id", "api_key_id"]) {
    const filter = { [field]: "known" };
    const node = select("../tabs/billing/Charges.tsx", `filter.${field} ?? ""`, { filter, tenants: [{ id: "known", name: "组织" }], keys: [{ id: "known", label: "密钥", prefix: "sk-" }], set: noop });
    assert.equal(optionValues(node.props.children).filter(id => id === "known").length, 1);
  }
  const node = select("../tabs/billing/Charges.tsx", 'filter.api_key_id ?? ""', { filter: {}, tenants: [], keys: [], set: noop });
  assert.equal(node.props.disabled, true);
});
