import assert from "node:assert/strict";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";
import { chromium, expect } from "@playwright/test";

// 组件单元测试直接驱动成功与剪贴板拒绝，不发网络或密钥 API 请求。
const compiled = await build({
  stdin: {
    loader: "tsx", resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "copy-controls.tsx",
    contents: `import { createRoot } from "react-dom/client";
      import { KeySecret } from "./ui/KeySecret";
      import { CopyButton } from "./ui/CopyButton";
      import { ShownOnce } from "./controls";
      globalThis.mountCopyControls = secret => createRoot(document.getElementById("root")).render(<>
        <KeySecret prefix="sk-example" reveal={async () => { globalThis.reveals++; return secret; }} unavailable={null} onError={() => {}} className="key-actions" />
        <CopyButton text="model-example" label="复制模型名" onFailed={() => {}} />
        <ShownOnce title="仅本次密钥" value="minted-example" onDone={() => {}} />
      </>);`,
  },
  bundle: true, platform: "browser", format: "iife", jsx: "automatic", write: false,
  loader: { ".css": "empty" }, define: { "import.meta.env.BASE_URL": '"/"', "process.env.NODE_ENV": '"test"' },
});

test("所有复制控件只显示图标，密钥保持遮罩，仅剪贴板失败后提供手动复制", { timeout: 30_000 }, async () => {
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage();
    const requests: string[] = [];
    page.on("request", request => requests.push(request.url()));
    await page.setContent('<div id="root"></div>');
    await page.addScriptTag({ content: compiled.outputFiles[0]!.text });
    const secret = "sk-component-unit-clipboard-value";
    await page.evaluate(value => {
      const state = globalThis as typeof globalThis & { mountCopyControls: (s: string) => void; denied: boolean; copied: string; reveals: number };
      state.denied = false;
      state.copied = "";
      state.reveals = 0;
      Object.defineProperty(navigator, "clipboard", { configurable: true, value: {
        writeText: async (text: string) => { if (state.denied) throw new DOMException("clipboard denied", "NotAllowedError"); state.copied = text; },
      } });
      state.mountCopyControls(value);
    }, secret);
    for (const name of ["复制密钥", "复制模型名", "复制"]) {
      const button = page.getByRole("button", { name, exact: true });
      await expect(button).toHaveText("");
      await expect(button).toHaveAttribute("title", name);
    }
    await expect(page.getByRole("button", { name: /^(显示|隐藏)$/ })).toHaveCount(0);
    await expect(page.getByLabel("手动复制密钥", { exact: true })).toHaveCount(0);
    await expect(page.getByLabel("密钥前缀", { exact: true })).toHaveText("sk-example…");
    const keyButton = page.locator(".key-actions button");
    await keyButton.click();
    await expect(keyButton).toHaveAttribute("aria-label", "已复制");
    await expect(keyButton).toHaveText("");
    expect(await page.evaluate(() => (globalThis as typeof globalThis & { copied: string }).copied)).toBe(secret);
    expect(await page.locator("body").innerText()).not.toContain(secret);
    await expect(page.getByLabel("手动复制密钥", { exact: true })).toHaveCount(0);
    await page.evaluate(() => { (globalThis as typeof globalThis & { denied: boolean }).denied = true; });
    await keyButton.click();
    const manual = page.getByLabel("手动复制密钥", { exact: true });
    await expect(manual).toHaveValue(secret);
    expect(await keyButton.getAttribute("aria-label")).toBe("复制密钥");
    await expect(manual).toHaveAttribute("readonly", "");
    await manual.focus();
    expect(await manual.evaluate((input: HTMLInputElement) => [input.selectionStart, input.selectionEnd])).toEqual([0, secret.length]);
    await page.evaluate(() => { (globalThis as typeof globalThis & { denied: boolean }).denied = false; });
    await keyButton.click();
    await expect(keyButton).toHaveAttribute("aria-label", "已复制");
    expect(await page.evaluate(() => (globalThis as typeof globalThis & { copied: string }).copied)).toBe(secret);
    await expect(manual).toHaveCount(0);
    expect(await page.locator("body").innerText()).not.toContain(secret);
    await expect(page.getByLabel("密钥前缀", { exact: true })).toHaveText("sk-example…");
    expect(await page.evaluate(() => (globalThis as typeof globalThis & { reveals: number }).reveals)).toBe(1);
    assert.deepEqual(requests, [], "组件测试不发请求");
  } finally {
    await browser.close();
  }
});
