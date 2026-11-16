import assert from "node:assert/strict";
import { test } from "node:test";
import { randomUUID } from "node:crypto";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { chromium, expect, type Locator, type Page } from "@playwright/test";

// 组件单元测试仅替换剪贴板能力，不发网络或密钥 API 请求。
const compiled = await build({
  stdin: {
    loader: "tsx", resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "copy-controls.tsx",
    contents: `import { createRoot } from "react-dom/client";
      import { KeySecret } from "./ui/KeySecret";
      import { CopyButton } from "./ui/CopyButton";
      import { ShownOnce } from "./controls";
      globalThis.mountCopyControls = (secret, minted, kind) => createRoot(document.getElementById("root")).render(<>
        {kind === "listed" && <KeySecret prefix="sk-example" reveal={async () => { globalThis.reveals++; return secret; }} unavailable={null} onError={() => {}} className="key-actions" />}
        <CopyButton text="model-example" label="复制模型名" onFailed={() => {}} />
        {kind === "created" && <ShownOnce title="仅本次密钥" value={minted} masked onDone={() => {}} />}
      </>);`,
  },
  bundle: true, platform: "browser", format: "iife", jsx: "automatic", write: false,
  loader: { ".css": "empty" }, define: { "import.meta.env.BASE_URL": '"/"', "process.env.NODE_ENV": '"test"' },
});

async function mount(page: Page, secret: string, minted: string, kind: "created" | "listed"): Promise<void> {
  await page.setContent('<div id="root"></div>');
  await page.addScriptTag({ content: compiled.outputFiles[0]!.text });
  await page.evaluate(({ secret, minted, kind }) => {
    const state = globalThis as typeof globalThis & { mountCopyControls: (secret: string, minted: string, kind: string) => void; denied: boolean; copied: string; reveals: number };
    state.denied = false;
    state.copied = "";
    state.reveals = 0;
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: {
      writeText: async (text: string) => { if (state.denied) throw new DOMException("clipboard denied", "NotAllowedError"); state.copied = text; },
    } });
    state.mountCopyControls(secret, minted, kind);
  }, { secret, minted, kind });
}

// 仅返回布尔值；失败日志永远不会回显组件持有的密钥。
async function noRawDom(page: Page, secrets: readonly string[]): Promise<void> {
  const exposed = await page.evaluate(values => [...document.querySelectorAll("*")].some(element => {
    const text = [...element.childNodes].filter(node => node.nodeType === Node.TEXT_NODE).map(node => node.textContent ?? "").join("");
    const fields = [text, ...[...element.attributes].map(attribute => attribute.value)];
    if (element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement) fields.push(element.value);
    return fields.some(field => values.some(value => field.includes(value)));
  }), [...secrets]);
  assert.equal(exposed, false, "密钥不得进入 DOM 文字、属性或输入值");
}
async function clipboardEquals(page: Page, expected: string): Promise<void> {
  const exact = await page.evaluate(value => (globalThis as typeof globalThis & { copied: string }).copied === value, expected);
  assert.equal(exact, true, "复制内容必须与内存中的密钥逐字节一致");
}
async function clipboardPermission(page: Page, denied: boolean): Promise<void> {
  await page.evaluate(value => {
    const state = globalThis as typeof globalThis & { denied: boolean; copied: string };
    state.denied = value;
    state.copied = "";
  }, denied);
}
async function iconOnly(button: Locator): Promise<void> {
  await expect(button).toHaveText("");
  await expect(button).toHaveAttribute("aria-label", /复制/);
  await expect(button).toHaveAttribute("title", /复制/);
}

for (const kind of ["created", "listed"] as const) {
  test(`${kind === "created" ? "新建" : "列表"}密钥在复制成功、拒绝与重试时始终遮罩且无明文 DOM`, { timeout: 30_000 }, async () => {
    const browser = await chromium.launch();
    try {
      const page = await browser.newPage();
      const requests: string[] = [];
      page.on("request", request => requests.push(request.url()));
      const secret = `sk-component-${randomUUID()}`;
      const minted = `sk-created-${randomUUID()}`;
      await mount(page, secret, minted, kind);
      if (kind === "created") await expect(page.getByRole("region", { name: "仅本次密钥", exact: true })).toBeVisible();
      else await expect(page.getByLabel("密钥前缀", { exact: true })).toBeVisible();
      const button = kind === "created"
        ? page.getByRole("region", { name: "仅本次密钥", exact: true }).locator("button").first()
        : page.locator(".key-actions button");
      await iconOnly(button);
      await iconOnly(page.getByRole("button", { name: "复制模型名", exact: true }));
      await expect(page.getByRole("button", { name: /^(显示|隐藏)(密钥)?$/ })).toHaveCount(0);
      await expect(page.getByLabel("手动复制密钥", { exact: true })).toHaveCount(0);
      await noRawDom(page, [minted, secret]);
      const value = kind === "created" ? minted : secret;
      await button.click();
      await expect(button).toHaveAttribute("aria-label", "已复制");
      await iconOnly(button);
      await clipboardEquals(page, value);
      await noRawDom(page, [minted, secret]);
      await clipboardPermission(page, true);
      await button.click();
      await expect(page.getByText("无法写入剪贴板", { exact: false })).toBeVisible();
      await expect(button).toHaveAttribute("aria-label", kind === "created" ? "复制" : "复制密钥");
      await noRawDom(page, [minted, secret]);
      await expect(page.getByLabel("手动复制密钥", { exact: true })).toHaveCount(0);
      await clipboardPermission(page, false);
      await button.click();
      await expect(button).toHaveAttribute("aria-label", "已复制");
      await clipboardEquals(page, value);
      await noRawDom(page, [minted, secret]);
      if (kind === "listed") {
        await expect(page.getByLabel("密钥前缀", { exact: true })).toHaveText("sk-example…");
        assert.equal(await page.evaluate(() => (globalThis as typeof globalThis & { reveals: number }).reveals), 1, "重试复用已获取的同一密钥");
      }
      assert.deepEqual(requests, [], "组件单元测试不发请求");
    } finally { await browser.close(); }
  });
}
