import { expect, test } from "@playwright/test";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const session = process.env.CARD_PALETTE_SESSION!;
const origin = process.env.CARD_PALETTE_ORIGIN!;
const output = process.env.CARD_PALETTE_OUTPUT!;
const world = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
if (world.ready !== true || world.realBackend !== true || world.mockUpstream !== false) throw new Error("A ready real backend is required");
if (!["localhost", "127.0.0.1"].includes(new URL(origin).hostname)) throw new Error("Only isolated loopback origins are allowed");
mkdirSync(output, { recursive: true });

const routes = [
  ["org", "概览"], ["usage", "用量分析"], ["billing", "账务账户"],
  ["budgets", "预算"], ["members", "成员"], ["prices", "模型与价格"], ["keys", "API 密钥"],
] as const;

for (const width of [1440, 390]) {
  for (const theme of ["light", "dark"] as const) {
    test(`user cards ${width} ${theme}: coordinated surfaces and readable real data`, async ({ page, context }) => {
      await page.setViewportSize({ width, height: 1000 });
      await context.addInitScript(value => {
        if (localStorage.getItem("aisix-portal-theme") === null) localStorage.setItem("aisix-portal-theme", value);
      }, theme);
      const who = auth.metricsOwner ?? auth.owner;
      expect((await context.request.post(`${origin}/api/login`, { data: who })).status()).toBe(200);
      const errors: string[] = [];
      const writes: string[] = [];
      const failedReads: string[] = [];
      const readPaths = new Set<string>();
      page.on("pageerror", () => errors.push("pageerror"));
      page.on("console", message => { if (message.type() === "error") errors.push("console error"); });
      page.on("request", request => {
        const path = new URL(request.url()).pathname;
        if (path.startsWith("/api/") && request.method() !== "GET") writes.push(`${request.method()} ${path}`);
      });
      page.on("response", response => {
        const path = new URL(response.url()).pathname;
        if (path.startsWith("/api/") && response.request().method() === "GET") {
          readPaths.add(path);
          if (response.status() >= 400) failedReads.push(`${response.status()} ${path}`);
        }
      });
      const problems: string[] = [];
      const evidence: unknown[] = [];
      for (const [route, heading] of routes) {
        await page.goto(`${origin}/#${route}`);
        await expect(page).toHaveTitle("Routio 组织门户");
        await expect(page.getByRole("heading", { name: heading, exact: true }).first()).toBeVisible();
        if (route === "org") await expect(page.getByRole("group", { name: "API 指标概览", exact: true })).toHaveAttribute("aria-busy", "false");
        if (route === "usage") await expect(page.getByRole("region", { name: "完整 API 指标", exact: true })).toHaveAttribute("aria-busy", "false");
        await page.waitForLoadState("networkidle");
        await page.evaluate(() => document.fonts.ready);
        await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
        await expect(page.locator("vite-error-overlay")).toHaveCount(0);
        await expect(page.locator(".shell-content .bento-card").first()).toBeVisible();
        await page.screenshot({ path: join(output, `${route}-${width}-${theme}.png`), fullPage: true });
        // 从页面实际像素颜色验证可读性，不锁定某个实现类名对应的色值。
        const audit = await page.locator(".shell-content .bento-card").evaluateAll((cards, dark) => {
          const canvas = document.createElement("canvas");
          canvas.width = canvas.height = 1;
          const ctx = canvas.getContext("2d")!;
          const rgb = (value: string) => {
            ctx.clearRect(0, 0, 1, 1);
            ctx.fillStyle = value;
            ctx.fillRect(0, 0, 1, 1);
            return Array.from(ctx.getImageData(0, 0, 1, 1).data).slice(0, 3).map(channel => channel / 255);
          };
          const luminance = (value: number[]) => value.map(v => v <= .04045 ? v / 12.92 : ((v + .055) / 1.055) ** 2.4).reduce((sum, v, i) => sum + v * [.2126, .7152, .0722][i]!, 0);
          const canvasLight = luminance(rgb(getComputedStyle(document.documentElement).getPropertyValue("--background")));
          return cards.map(card => {
            const css = getComputedStyle(card);
            const background = rgb(css.backgroundColor);
            const light = luminance(background);
            const chroma = Math.max(...background) - Math.min(...background);
            const foregrounds = [...card.querySelectorAll("h2,h3,p,.metric,dt,dd")].filter(node => node.textContent?.trim() && node.getBoundingClientRect().height > 0);
            const contrasts = foregrounds.map(node => {
              const foreground = luminance(rgb(getComputedStyle(node).color));
              return (Math.max(foreground, light) + .05) / (Math.min(foreground, light) + .05);
            });
            return {
              label: card.querySelector("h2,h3")?.textContent ?? card.getAttribute("aria-label") ?? "card",
              background: css.backgroundColor, chroma, luminance: light,
              minTextContrast: Math.min(...contrasts), textSamples: contrasts.length,
              surfaceFitsTheme: dark ? light > canvasLight + .002 && light < .15 : light > .72,
            };
          });
        }, theme === "dark");
        for (const card of audit) {
          if (!card.surfaceFitsTheme || card.chroma > .13) problems.push(`${route}: ${card.label} has an isolated high contrast or saturated surface`);
          if (card.textSamples > 0 && card.minTextContrast < 4.5) problems.push(`${route}: ${card.label} text contrast ${card.minTextContrast.toFixed(2)}`);
        }
        evidence.push({ route, cards: audit });
        expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), `${route} must stay inside the viewport`).toBe(true);
        if (route === "usage") {
          await page.getByRole("tab", { name: "费用", exact: true }).click();
          await expect(page.getByRole("tab", { name: "费用", exact: true })).toHaveAttribute("aria-selected", "true");
          await page.getByRole("tab", { name: "Token", exact: true }).click();
          await expect(page.getByRole("tab", { name: "Token", exact: true })).toHaveAttribute("aria-selected", "true");
        }
      }
      writeFileSync(join(output, `audit-${width}-${theme}.json`), JSON.stringify({ evidence, readPaths: [...readPaths], problems }, null, 2));
      expect(readPaths.has("/api/usage/metrics"), "real statistics were loaded").toBe(true);
      expect(readPaths.has("/api/billing/balance"), "real financial data was loaded").toBe(true);
      expect(failedReads).toEqual([]);
      expect(errors).toEqual([]);
      expect(writes, "browsing and local tab changes perform no business writes").toEqual([]);
      expect(problems).toEqual([]);
      // 真正点击主题开关并刷新，不能只检查初始化存储值。
      await page.getByRole("button", { name: theme === "dark" ? "切换到浅色" : "切换到深色", exact: true }).click();
      const next = theme === "dark" ? "light" : "dark";
      await expect(page.locator("html")).toHaveAttribute("data-theme", next);
      await page.reload();
      await expect(page.locator("html")).toHaveAttribute("data-theme", next);
    });
  }
}
