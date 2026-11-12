import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { build } from "esbuild";
import { createElement, isValidElement, type ComponentType, type ReactNode } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { PublicModelRouting } from "./api.ts";
import type { RoutingModeFieldProps } from "../ui/RoutingModeField.tsx";

const require = createRequire(import.meta.url);
const compiled = await build({
  stdin: { contents: 'export { RoutePolicy } from "./ui/RoutePolicy.tsx"; export { RoutingModeField } from "./ui/RoutingModeField.tsx";', resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "routing-controls.tsx" },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, (args) => ({ path: require.resolve(args.path), external: true })); } }],
});
const loaded = { exports: {} };
new Function("require", "module", "exports", compiled.outputFiles[0]!.text)(require, loaded, loaded.exports);
const { RoutePolicy, RoutingModeField } = loaded.exports as {
  RoutePolicy: ComponentType<{ routing: PublicModelRouting; defaultStrategy?: string; compact?: boolean }>;
  RoutingModeField: (props: RoutingModeFieldProps) => ReactNode;
};
const textOf = (markup: string) => markup.replace(/<[^>]+>/g, "");

test("saved policy explains weighted key space and the independent fallback cap", () => {
  const text = textOf(renderToStaticMarkup(createElement(RoutePolicy, {
    routing: { strategy: "consistent_hash", hash_on: [{ type: "api_key" }], max_fallbacks: 1 },
  })));
  // 将 key 空间误报为请求份额，或不显示上限，都会丢失操作员所需的边界说明。
  assert.match(text, /同级加权一致性哈希/);
  assert.match(text, /API key ID 亲和/);
  assert.match(text, /权重分配亲和键空间/);
  assert.match(text, /max_fallbacks=1/);
  assert.match(text, /可能到不了低优先级备组/);
  assert.match(text, /429 默认不/);
  assert.match(text, /会话续接优先原归属/);
});

test("saved failover remains fixed order rather than silently becoming the recommended mode", () => {
  const text = textOf(renderToStaticMarkup(createElement(RoutePolicy, { routing: { strategy: "failover" } })));
  assert.match(text, /同级固定顺序/);
  assert.match(text, /failover/);
  assert.doesNotMatch(text, /API key ID 亲和/);
});

test("custom and unknown modes render a selected preservation option instead of selecting the recommendation", () => {
  for (const strategy of ["consistent_hash", "future_strategy"]) {
    const markup = renderToStaticMarkup(createElement(RoutingModeField, {
      value: { strategy, affinity: "custom" },
      onChange: () => { throw new Error("渲染不能修改路由"); },
      storedHashOn: [{ type: "header", name: "x-session" }],
    }));
    assert.match(markup, /<option value="preserve" selected="">/);
    assert.doesNotMatch(markup, /<option value="api_key" selected="">/);
    if (strategy === "consistent_hash") assert.match(markup, /<option value="custom" selected="">/);
    assert.match(textOf(markup), new RegExp(strategy));
  }
});

test("read-only routing and field refusals retain native disabled and accessible error state", () => {
  const markup = renderToStaticMarkup(createElement(RoutingModeField, {
    value: { strategy: "consistent_hash", affinity: "api_key" },
    onChange: () => undefined,
    disabled: true, error: "策略已被拒绝", hashError: "亲和来源已被拒绝",
  }));
  const selects = markup.match(/<select[^>]*>/g) ?? [];
  assert.equal(selects.length, 2);
  for (const select of selects) {
    assert.match(select, /disabled=""/);
    assert.match(select, /aria-invalid="true"/);
    assert.match(select, /aria-describedby="[^"]+"/);
  }
});

test("schema strategies constrain explicit choices while preserving the saved value", () => {
  const markup = renderToStaticMarkup(createElement(RoutingModeField, {
    value: { strategy: "future_strategy", affinity: "default" },
    onChange: () => undefined,
    strategies: ["round_robin", "least_cost"],
  }));
  assert.match(markup, /<option value="preserve" selected="">/);
  assert.match(markup, /<option value="api_key" disabled="">/);
  assert.match(markup, /<option value="strategy:least_cost">/);
});

test("an explicit mode selection updates only strategy and affinity, leaving unrelated draft fields live", () => {
  let changed: unknown;
  const tree = RoutingModeField({
    value: { strategy: "consistent_hash", affinity: "custom", retries: "9" } as RoutingModeFieldProps["value"],
    onChange: (next) => { changed = next; },
  });
  assert.ok(isValidElement<{ children: ReactNode[] }>(tree));
  const field = tree.props.children[0];
  assert.ok(isValidElement<{ children: (aria: object) => ReactNode }>(field));
  const select = field.props.children({});
  assert.ok(isValidElement<{ onChange: (event: { target: { value: string } }) => void }>(select));
  select.props.onChange({ target: { value: "api_key" } });
  assert.deepEqual(changed, { strategy: "consistent_hash", affinity: "api_key" });
});
