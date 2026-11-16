import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { build } from "esbuild";
import { chromium, expect } from "@playwright/test";
import { createElement, isValidElement, type ComponentType, type ReactNode } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import type { PublicModelRouting } from "./api.ts";
import type { RoutingModeFieldProps } from "../ui/RoutingModeField.tsx";

const require = createRequire(import.meta.url);
const compiled = await build({
  stdin: { contents: 'export { RoutePolicy } from "./ui/RoutePolicy.tsx"; export { RoutingModeField } from "./ui/RoutingModeField.tsx";', resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "routing-controls.tsx" },
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  loader: { ".css": "empty" },
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, (args) => ({ path: require.resolve(args.path), external: true })); } }],
});
const loaded = { exports: {} };
new Function("require", "module", "exports", compiled.outputFiles[0]!.text)(require, loaded, loaded.exports);
const { RoutePolicy, RoutingModeField } = loaded.exports as {
  RoutePolicy: ComponentType<{ routing: PublicModelRouting; defaultStrategy?: string; compact?: boolean }>;
  RoutingModeField: (props: RoutingModeFieldProps) => ReactNode;
};
const textOf = (markup: string) => markup.replace(/<[^>]+>/g, "");
const comboboxes = (markup: string) => markup.match(/<button\b[^>]*\brole="combobox"[^>]*>/g) ?? [];

// 使用真实 ReactDOM 验证弹层里的选项和回调，不替换组件或请求后端。
const browserCompiled = await build({
  stdin: { loader: "tsx", resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "routing-mode-journey.tsx", contents: `
    import { useState } from "react";
    import { createRoot } from "react-dom/client";
    import { RoutingModeField } from "./ui/RoutingModeField";
    function Scenario({ label, value, ...props }) {
      const [draft, setDraft] = useState(value);
      const [changes, setChanges] = useState([]);
      return <section aria-label={label}><form>
        <RoutingModeField {...props} value={draft} onChange={next => {
          setChanges(current => [...current, next]);
          setDraft(current => ({...current, ...next}));
        }} />
        <output aria-label="路由草稿">{JSON.stringify(draft)}</output>
        <output aria-label="路由更改">{JSON.stringify(changes)}</output>
      </form></section>;
    }
    createRoot(document.getElementById("root")).render(<>
      <Scenario label="自定义" value={{strategy:"consistent_hash",affinity:"custom",retries:"9"}} storedHashOn={[{type:"header",name:"x-session"}]} />
      <Scenario label="未知策略" value={{strategy:"future_strategy",affinity:"default"}} strategies={["round_robin","least_cost"]} />
      <Scenario label="只读" value={{strategy:"consistent_hash",affinity:"api_key"}} disabled error="策略已被拒绝" hashError="亲和来源已被拒绝" />
    </>);` },
  bundle: true, platform: "browser", format: "iife", jsx: "automatic", write: false,
  loader: { ".css": "empty" }, define: { "process.env.NODE_ENV": '"test"' },
});

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
    const selects = comboboxes(markup);
    assert.match(selects[0]!, /data-value="preserve"/);
    assert.doesNotMatch(selects[0]!, /data-value="api_key"/);
    if (strategy === "consistent_hash") assert.match(selects[1]!, /data-value="custom"/);
    assert.match(textOf(markup), new RegExp(strategy));
  }
});

test("read-only routing and field refusals retain disabled and accessible error state", () => {
  const markup = renderToStaticMarkup(createElement(RoutingModeField, {
    value: { strategy: "consistent_hash", affinity: "api_key" },
    onChange: () => undefined,
    disabled: true, error: "策略已被拒绝", hashError: "亲和来源已被拒绝",
  }));
  const selects = comboboxes(markup);
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
  assert.match(comboboxes(markup)[0]!, /data-value="preserve"/);
});

test("真实路由菜单保留选项值、schema禁用、自定义来源与未知策略，并只更新路由字段", { timeout: 30_000 }, async () => {
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage();
    const errors: string[] = [];
    page.on("pageerror", error => errors.push(error.message));
    await page.setContent('<div id="root"></div>');
    await page.addScriptTag({ content: browserCompiled.outputFiles[0]!.text });
    const custom = page.getByRole("region", { name: "自定义", exact: true });
    const mode = custom.getByRole("combobox", { name: "同级分配方式", exact: true });
    const affinity = custom.getByRole("combobox", { name: "亲和来源", exact: true });
    await expect(mode).toHaveAttribute("data-value", "preserve");
    await expect(affinity).toHaveAttribute("data-value", "custom");
    await mode.click();
    await expect(page.getByRole("option", { name: /^保留当前配置/ })).toHaveAttribute("aria-selected", "true");
    await expect.poll(() => page.getByRole("option").evaluateAll(options => options.map(option => option.getAttribute("data-value")))).toEqual(["preserve", "session", "api_key", "round_robin", "failover"]);
    await page.keyboard.press("Escape");
    await expect(custom.getByRole("status", { name: "路由更改", exact: true })).toHaveText("[]");
    await affinity.click();
    await expect(page.getByRole("option", { name: "保留已存自定义来源链", exact: true })).toHaveAttribute("aria-selected", "true");
    await expect.poll(() => page.getByRole("option").evaluateAll(options => options.map(option => option.getAttribute("data-value")))).toEqual(["session", "default", "api_key", "custom"]);
    await page.keyboard.press("Escape");
    await mode.click();
    await page.getByRole("option", { name: "推荐：同级加权一致性哈希 · API key ID 亲和", exact: true }).click();
    await expect(mode).toHaveAttribute("data-value", "api_key");
    assert.deepEqual(JSON.parse(await custom.getByRole("status", { name: "路由更改", exact: true }).innerText()), [{ strategy: "consistent_hash", affinity: "api_key" }]);
    assert.deepEqual(JSON.parse(await custom.getByRole("status", { name: "路由草稿", exact: true }).innerText()), { strategy: "consistent_hash", affinity: "api_key", retries: "9" });

    const unknown = page.getByRole("region", { name: "未知策略", exact: true });
    const unknownMode = unknown.getByRole("combobox", { name: "同级分配方式", exact: true });
    await unknownMode.click();
    await expect(page.getByRole("option", { name: /^保留当前配置/ })).toHaveAttribute("aria-selected", "true");
    await expect.poll(() => page.getByRole("option").evaluateAll(options => options.map(option => option.getAttribute("data-value")))).toEqual(["preserve", "session", "api_key", "round_robin", "failover", "strategy:least_cost"]);
    await expect(page.getByRole("option", { name: "推荐：同级加权一致性哈希 · API key ID 亲和", exact: true })).toHaveAttribute("aria-disabled", "true");
    await expect(page.getByRole("option", { name: "会话固定 · 同一会话使用同一上游账号", exact: true })).toHaveAttribute("aria-disabled", "true");
    await expect(page.getByRole("option", { name: "同级固定顺序", exact: true })).toHaveAttribute("aria-disabled", "true");
    await expect(page.getByRole("option", { name: "同级按请求加权轮询", exact: true })).toBeEnabled();
    await page.keyboard.press("Escape");
    await expect(unknown.getByRole("status", { name: "路由更改", exact: true })).toHaveText("[]");
    await unknownMode.click();
    await page.getByRole("option", { name: /least_cost/ }).click();
    assert.deepEqual(JSON.parse(await unknown.getByRole("status", { name: "路由更改", exact: true }).innerText()), [{ strategy: "least_cost", affinity: "default" }]);
    await expect(unknownMode).toHaveAttribute("data-value", "preserve");

    const readOnly = page.getByRole("region", { name: "只读", exact: true });
    for (const [name, error] of [["同级分配方式", "策略已被拒绝"], ["亲和来源", "亲和来源已被拒绝"]]) {
      const select = readOnly.getByRole("combobox", { name, exact: true });
      await expect(select).toBeDisabled();
      await expect(select).toHaveAttribute("aria-invalid", "true");
      await expect(select).toHaveAccessibleDescription(new RegExp(error!));
    }
    await expect(readOnly.getByRole("status", { name: "路由更改", exact: true })).toHaveText("[]");
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
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
  assert.ok(isValidElement<{ onValueChange: (value: string) => void }>(select));
  select.props.onValueChange("api_key");
  assert.deepEqual(changed, { strategy: "consistent_hash", affinity: "api_key" });
});

test("会话固定菜单显示账号池边界，切回密钥亲和不会残留禁止切换", { timeout: 30_000 }, async () => {
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage();
    await page.setContent('<div id="root"></div>');
    await page.addScriptTag({ content: browserCompiled.outputFiles[0]!.text });
    const region = page.getByRole("region", { name: "自定义", exact: true });
    const mode = region.getByRole("combobox", { name: "同级分配方式", exact: true });
    await mode.click();
    await page.getByRole("option", { name: "会话固定 · 同一会话使用同一上游账号", exact: true }).click();
    await expect(mode).toHaveAttribute("data-value", "session");
    assert.deepEqual(JSON.parse(await region.getByRole("status", { name: "路由草稿", exact: true }).innerText()), { strategy: "consistent_hash", affinity: "session", hashFailover: false, retries: "9" });
    await expect(region).toContainText("多个同优先级、正权重的主用目标才能分流");
    await expect(region).toContainText("原目标不可用时请求失败，不切换账号");
    for (const header of ["x-claude-code-session-id", "session-id", "thread-id"]) await expect(region).toContainText(header);
    await expect(region).toContainText("不回退 API key 或空值");
    await mode.click();
    await page.getByRole("option", { name: "推荐：同级加权一致性哈希 · API key ID 亲和", exact: true }).click();
    const ordinary = JSON.parse(await region.getByRole("status", { name: "路由草稿", exact: true }).innerText());
    assert.deepEqual(ordinary, { strategy: "consistent_hash", affinity: "api_key", retries: "9" });
    await expect(mode).toHaveAttribute("data-value", "api_key");
  } finally { await browser.close(); }
});
