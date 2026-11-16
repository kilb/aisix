import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { test } from "node:test";
import { build } from "esbuild";
import { chromium, expect, type Page } from "@playwright/test";

// 只控制剪贴板这一组件边界；真实 ReactDOM 与浏览器参与，不启动服务或替换网络。
// bundle 不携带密钥值，mount 的参数仅保留在闭包，避免测试脚本本身制造 DOM 泄漏。
const compiled = await build({
  stdin: {
    loader: "tsx", resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "key-secret-privacy.tsx", contents: `
      import { useState } from "react";
      import { createRoot } from "react-dom/client";
      import { KeySecret } from "./ui/KeySecret";
      import { ToastProvider } from "./ui/Toast";
      window.mountKeySecret = (secret, prefix, failure) => {
        const written = [];
        let writes = 0;
        let reveals = 0;
        let exposed = false;
        const contains = value => typeof value === "string" && value.includes(secret);
        const observer = new MutationObserver(records => {
          for (const record of records) {
            if (contains(record.oldValue)) exposed = true;
            for (const node of record.addedNodes) {
              if (contains(node.textContent) || (node instanceof Element && contains(node.outerHTML))) exposed = true;
            }
          }
          if (contains(document.documentElement.outerHTML)) exposed = true;
        });
        observer.observe(document.documentElement, { subtree: true, childList: true, attributes: true, attributeOldValue: true, characterData: true, characterDataOldValue: true });
        Object.defineProperty(navigator, "clipboard", { configurable: true, get: () => ({
          writeText: async value => {
            writes += 1;
            if (failure === "clipboard" && writes === 1) throw new DOMException("Clipboard permission denied", "NotAllowedError");
            written.push(value);
          },
        }) });
        window.keySecretClipboard = () => ({ writes, reveals, written: written.length, copied: written.length === 1 && written[0] === secret });
        window.keySecretPrivacy = () => ({
          text: contains(document.documentElement.textContent),
          visibleText: contains(document.body.innerText),
          serializedHtml: contains(document.documentElement.outerHTML),
          attributes: [...document.querySelectorAll("*")].some(element => [...element.attributes].some(attribute => contains(attribute.value))),
          editableProperties: [...document.querySelectorAll("input,textarea")].some(element => contains(element.value) || contains(element.defaultValue)),
          storage: [localStorage, sessionStorage].some(storage => Object.keys(storage).some(key => contains(key) || contains(storage.getItem(key)))) || contains(document.cookie) || contains(window.name),
          transientExposure: exposed,
        });
        function Journey() {
          const [error, setError] = useState(null);
          return <ToastProvider><KeySecret prefix={prefix} className="key-actions" unavailable={null}
            reveal={async () => { reveals += 1; if (failure === "reveal" && reveals === 1) throw new Error("Reveal permission denied"); return secret; }}
            onError={() => setError("无法读取密钥，请重试")} />
            {error && <p role="alert">{error}</p>}
          </ToastProvider>;
        }
        createRoot(document.getElementById("root")).render(<Journey />);
      };`,
  },
  bundle: true, platform: "browser", format: "iife", jsx: "automatic", write: false,
  loader: { ".css": "empty" }, define: { "process.env.NODE_ENV": '"test"' },
});

type Failure = "none" | "clipboard" | "reveal";
interface ClipboardResult { writes: number; reveals: number; written: number; copied: boolean }
interface PrivacyWindow extends Window {
  mountKeySecret: (secret: string, prefix: string, failure: Failure) => void;
  keySecretClipboard: () => ClipboardResult;
  keySecretPrivacy: () => Record<string, boolean>;
}

async function withKey(failure: Failure, run: (page: Page, assertPrivate: () => Promise<void>) => Promise<void>): Promise<void> {
  const browser = await chromium.launch();
  const directory = await mkdtemp(join(tmpdir(), "aisix-key-privacy-"));
  try {
    const path = join(directory, "index.html");
    await writeFile(path, '<!doctype html><html><body><div id="root"></div></body></html>');
    const page = await browser.newPage();
    const secret = `sk-unit-privacy-${randomUUID().replaceAll("-", "")}`;
    const prefix = secret.slice(0, 12);
    const consoleLeaks: Promise<boolean>[] = [];
    const pageErrors: boolean[] = [];
    page.on("console", message => {
      consoleLeaks.push(Promise.all(message.args().map(async arg => {
        try { return JSON.stringify(await arg.jsonValue()); } catch { return "[unserializable console argument]"; }
      })).then(values => message.text().includes(secret) || values.some(value => value?.includes(secret))));
    });
    page.on("pageerror", error => pageErrors.push(error.message.includes(secret)));
    // file origin 让 localStorage/sessionStorage 可实际读写，无需 HTTP 服务。
    await page.goto(pathToFileURL(path).href);
    await page.addScriptTag({ content: compiled.outputFiles[0]!.text });
    await page.evaluate(({ secret, prefix, failure }) => (window as unknown as PrivacyWindow).mountKeySecret(secret, prefix, failure), { secret, prefix, failure });
    await expect(page.getByLabel("密钥前缀", { exact: true })).toHaveText(`${prefix}…`);
    const assertPrivate = async (): Promise<void> => {
      const report = await page.evaluate(() => (window as unknown as PrivacyWindow).keySecretPrivacy());
      const exposedSurfaces = Object.entries(report).filter(([, exposed]) => exposed).map(([surface]) => surface);
      assert.deepEqual(exposedSurfaces, [], "完整密钥不得进入页面、属性、表单、存储或瞬时 DOM");
      assert.equal((await Promise.all(consoleLeaks)).some(Boolean), false, "控制台不得包含完整密钥");
      assert.equal(pageErrors.some(Boolean), false, "浏览器异常不得包含完整密钥");
      assert.equal(pageErrors.length, 0, "组件不得产生未处理的浏览器异常");
      await expect(page.getByLabel("密钥前缀", { exact: true })).toHaveText(`${prefix}…`);
    };
    await assertPrivate();
    await run(page, assertPrivate);
  } finally {
    await browser.close();
    await rm(directory, { recursive: true, force: true });
  }
}

async function copiedSecret(page: Page): Promise<void> {
  await expect(page.getByRole("button", { name: "已复制", exact: true })).toBeVisible();
  const result = await page.evaluate(() => (window as unknown as PrivacyWindow).keySecretClipboard());
  assert.ok(result.writes > 0 && result.reveals > 0, "复制必须实际读取密钥并触达剪贴板边界");
  assert.ok(result.written === 1 && result.copied, "成功复制必须把完整密钥交给剪贴板");
}

// 若只是展示成功提示却未复制，或把明文藏进属性、输入值或日志，本例必须失败。
test("密钥复制成功只交给剪贴板，页面持续仅显示遮罩", { timeout: 20_000 }, async () => {
  await withKey("none", async (page, assertPrivate) => {
    await page.getByRole("button", { name: "复制密钥", exact: true }).click();
    await copiedSecret(page);
    await assertPrivate();
  });
});

// 旧版手动复制输入框会让本例变红；失败必须只给提示，重试仍能复制。
test("剪贴板拒绝后不回退为明文，提示失败并允许重试", { timeout: 20_000 }, async () => {
  await withKey("clipboard", async (page, assertPrivate) => {
    const copy = page.getByRole("button", { name: "复制密钥", exact: true });
    await copy.click();
    await expect(page.locator('[role="status"], [role="alert"], [aria-live="polite"]').filter({ hasText: /剪贴板|复制失败|无法复制|未复制/ }).first()).toBeVisible();
    await expect(copy).toBeEnabled();
    await assertPrivate();
    const failed = await page.evaluate(() => (window as unknown as PrivacyWindow).keySecretClipboard());
    assert.equal(failed.writes, 1);
    assert.equal(failed.written, 0);
    await copy.click();
    await copiedSecret(page);
    await assertPrivate();
  });
});

// 若读取被拒仍写剪贴板，或失败后失去重试入口，本例必须失败。
test("密钥读取拒绝不写剪贴板也不泄漏，随后可以重新复制", { timeout: 20_000 }, async () => {
  await withKey("reveal", async (page, assertPrivate) => {
    const copy = page.getByRole("button", { name: "复制密钥", exact: true });
    await copy.click();
    await expect(page.getByRole("alert")).toHaveText("无法读取密钥，请重试");
    await expect(copy).toBeEnabled();
    const failed = await page.evaluate(() => (window as unknown as PrivacyWindow).keySecretClipboard());
    assert.equal(failed.reveals, 1);
    assert.equal(failed.writes, 0);
    assert.equal(failed.written, 0);
    await assertPrivate();
    await copy.click();
    await copiedSecret(page);
    await assertPrivate();
  });
});
