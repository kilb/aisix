import assert from "node:assert/strict";
import { after, test } from "node:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { realtimeSipDraft } from "./realtimeSipBinding.ts";

const require = createRequire(import.meta.url);
const compiled = await build({
  entryPoints: [fileURLToPath(new URL("../tabs/providers/RealtimeSipBindingForm.tsx", import.meta.url))],
  bundle: true, platform: "node", format: "cjs", jsx: "automatic", write: false,
  loader: { ".css": "empty" },
  plugins: [{ name: "single-react-runtime", setup(builder) { builder.onResolve({ filter: /^react(?:\/.*)?$/ }, args => ({ path: require.resolve(args.path), external: true })); } }],
});
const temporary = await mkdtemp(join(tmpdir(), "aisix-sip-form-"));
after(() => rm(temporary, { recursive: true, force: true }));
const entry = join(temporary, "component.cjs");
await writeFile(entry, compiled.outputFiles[0]!.text);
const { RealtimeSipBindingForm } = require(entry) as typeof import("../tabs/providers/RealtimeSipBindingForm.tsx");
const binding = {
  route_id: "tenant-one",
  webhook_secret: "whsec_this-must-never-reach-markup",
  api_key_id: "11111111-1111-1111-1111-111111111111",
  model_id: "22222222-2222-2222-2222-222222222222",
};
const props = { draft: realtimeSipDraft(binding), storedSecret: true, storedProject: true, projectError: null, onProjectChange: () => undefined, disabled: false, errorAt: () => null, sectionError: null, onChange: () => undefined };

// 直接渲染实际组件；把已保存状态误接成密码框时必须变红。
test("stored SIP credentials render only their set state and a replacement action", () => {
  const markup = renderToStaticMarkup(createElement(RealtimeSipBindingForm, props));
  assert.match(markup, /已设置/);
  assert.match(markup, />替换</);
  assert.doesNotMatch(markup, /type="password"|\[REDACTED\]|whsec_this/);
  assert.match(markup, /value="11111111-1111-1111-1111-111111111111"/);
  assert.match(markup, /value="22222222-2222-2222-2222-222222222222"/);
  assert.match(markup, /\/internal\/native\/realtime\/sip\/tenant-one/);
  assert.match(markup, /realtime\.call\.incoming/);
  assert.match(markup, /该上游项目的全部 SIP 来电归此绑定租户/);
  assert.match(markup, /SIP 上游项目 ID/);
});

test("a new binding asks for a password and a viewer cannot replace stored credentials", () => {
  const fresh = renderToStaticMarkup(createElement(RealtimeSipBindingForm, { ...props, draft: { ...realtimeSipDraft(undefined), enabled: true }, storedSecret: false, storedProject: false }));
  assert.match(fresh, /type="password"[^>]*required=""/);
  const viewer = renderToStaticMarkup(createElement(RealtimeSipBindingForm, { ...props, disabled: true }));
  assert.match(viewer, /已设置/);
  assert.doesNotMatch(viewer, /<button|type="password"/);
  for (const input of viewer.match(/<input\b[^>]*>/g) ?? []) assert.match(input, /disabled=""/);
});

test("disabled bindings keep their fields hidden and cannot block form validation", () => {
  const markup = renderToStaticMarkup(createElement(RealtimeSipBindingForm, { ...props, draft: realtimeSipDraft(undefined), storedSecret: false, storedProject: false }));
  assert.match(markup, /<div hidden="">/);
  const fields = (markup.match(/<input\b[^>]*>/g) ?? []).filter(input => !input.includes('type="checkbox"'));
  assert.equal(fields.length, 5);
  for (const input of fields) assert.match(input, /disabled=""/);
});

test("the rejected SIP field receives its accessible error and unsafe routes have no URL hint", () => {
  const markup = renderToStaticMarkup(createElement(RealtimeSipBindingForm, {
    ...props,
    draft: { ...props.draft, routeId: "../../another-tenant" },
    errorAt: field => field === "api_key_id" ? "此密钥不可用于 SIP 来电" : null,
  }));
  assert.match(markup, /aria-invalid="true"[^>]*aria-describedby=/);
  assert.match(markup, /此密钥不可用于 SIP 来电/);
  assert.doesNotMatch(markup, /\/internal\/native\/realtime\/sip\//);
});
