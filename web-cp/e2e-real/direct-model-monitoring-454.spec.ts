import { expect, test, type Page, type Response } from "@playwright/test";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

// 只读取隔离世界保留的真实调用，不生成推理流量或替换网络响应。
const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
const from = process.env.MONITORING_QA_FROM;
const to = process.env.MONITORING_QA_TO;
if (state.ready !== true || state.mockUpstream !== false) throw new Error("A ready real local stack is required");
if (!["127.0.0.1", "localhost"].includes(new URL(state.adminUrl).hostname)) throw new Error("Only isolated loopback URLs are allowed");
if (!from || !to || !Number.isFinite(Date.parse(from)) || !Number.isFinite(Date.parse(to)) || Date.parse(from) >= Date.parse(to)) throw new Error("MONITORING_QA_FROM/TO must identify the retained real-call window");

interface ModelAnswer {
  model: { id: string; name: string };
  provider_key: { id: string | null; name: string };
  attempts: number;
  succeeded_attempts: number;
  failed_attempts: number;
  unknown_outcome_attempts: number;
  failure_rate: number | null;
}
interface DirectAnswer { models: ModelAnswer[] }

function monitoring(response: Response): boolean {
  return response.request().method() === "GET" && new URL(response.url()).pathname === "/admin/usage/direct-models";
}

async function login(page: Page): Promise<void> {
  await page.goto(state.adminUrl);
  await page.getByLabel("邮箱", { exact: true }).fill(auth.operator.email);
  await page.getByLabel("口令", { exact: true }).fill(auth.operator.password);
  const waiting = page.waitForResponse(response => response.request().method() === "POST" && new URL(response.url()).pathname === "/admin/login");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await waiting).status()).toBe(200);
  await page.waitForLoadState("networkidle");
}

const ratioText = (value: number | null): string => value === null ? "—" : new Intl.NumberFormat("en-US", { style: "percent", maximumFractionDigits: 1 }).format(value);

test("#98 defaults to one hour and ranks actual direct-model failure rates with red warnings", async ({ page }) => {
  const writes: string[] = [];
  const errors: string[] = [];
  page.on("pageerror", error => errors.push(error.message));
  page.on("request", request => {
    const url = new URL(request.url());
    if (!["GET", "HEAD"].includes(request.method()) && !/^\/admin\/(?:login|logout)$/.test(url.pathname)) writes.push(`${request.method()} ${url.pathname}`);
  });
  await login(page);
  const first = page.waitForResponse(monitoring);
  await page.goto(`${state.adminUrl}/#upstream-monitoring`);
  const initial = await first;
  expect(initial.status()).toBe(200);
  const initialQuery = new URL(initial.url()).searchParams;
  expect(initialQuery.has("from")).toBe(true);
  expect(initialQuery.has("to")).toBe(true);
  expect(Date.parse(initialQuery.get("to")!) - Date.parse(initialQuery.get("from")!)).toBe(60 * 60 * 1000);
  await expect(page.getByRole("combobox", { name: "监测模型排序", exact: true })).toHaveText("失败率从高到低");

  const scope = new URLSearchParams({ from: from!, to: to!, bucket: "hour" });
  const waiting = page.waitForResponse(response => monitoring(response) && new URL(response.url()).searchParams.get("from") === from && new URL(response.url()).searchParams.get("to") === to);
  await page.goto(`${state.adminUrl}/#upstream-monitoring?${scope}`);
  const response = await waiting;
  expect(response.status()).toBe(200);
  const data = await response.json() as DirectAnswer;
  const known = data.models.filter(model => model.succeeded_attempts + model.failed_attempts > 0);
  expect(known.length, "真实调用窗口至少需要两个模型的已知结果，才能验证排序").toBeGreaterThanOrEqual(2);
  expect(new Set(known.map(model => model.failure_rate)).size, "真实样本必须含不同失败率，反向排序才会失败").toBeGreaterThanOrEqual(2);
  expect(data.models.some(model => model.failed_attempts > 0), "红色告警需要真实失败样本").toBe(true);
  expect(data.models.some(model => model.succeeded_attempts + model.failed_attempts === 0), "未知展示需要无已知结果的直连模型").toBe(true);
  for (const model of data.models) {
    for (const value of [model.attempts, model.succeeded_attempts, model.failed_attempts, model.unknown_outcome_attempts]) expect(Number.isSafeInteger(value) && value >= 0).toBe(true);
    expect(model.attempts).toBe(model.succeeded_attempts + model.failed_attempts + model.unknown_outcome_attempts);
    const denominator = model.succeeded_attempts + model.failed_attempts;
    if (denominator === 0) expect(model.failure_rate).toBeNull();
    else expect(model.failure_rate).toBeCloseTo(model.failed_attempts / denominator, 10);
  }
  const sorted = [...data.models].sort((left, right) => {
    if (left.failure_rate === null && right.failure_rate !== null) return 1;
    if (left.failure_rate !== null && right.failure_rate === null) return -1;
    return (right.failure_rate ?? 0) - (left.failure_rate ?? 0) || left.model.name.localeCompare(right.model.name);
  });
  const table = page.getByRole("table", { name: "直连模型监测", exact: true });
  const rows = table.getByRole("row").filter({ has: page.getByRole("heading", { level: 3 }) });
  await expect(rows).toHaveCount(Math.min(10, sorted.length));
  for (const [index, model] of sorted.slice(0, 10).entries()) {
    const row = rows.nth(index);
    await expect(row).toContainText(model.model.id);
    await expect(row.getByRole("cell").nth(2)).toContainText(ratioText(model.failure_rate));
  }
  const failed = sorted.find(model => model.failed_attempts > 0)!;
  const failedRow = rows.filter({ hasText: failed.model.id });
  const warning = failedRow.getByRole("cell").nth(2).getByText(ratioText(failed.failure_rate), { exact: true });
  const colors = await warning.evaluate(element => {
    const sample = document.createElement("span");
    sample.style.color = "var(--color-danger)";
    element.append(sample);
    const expected = getComputedStyle(sample).color;
    sample.remove();
    const actual = getComputedStyle(element).color;
    const canvas = document.createElement("canvas");
    canvas.width = canvas.height = 1;
    const context = canvas.getContext("2d");
    if (!context) throw new Error("需要浏览器色彩采样验证红色告警");
    context.fillStyle = actual;
    context.fillRect(0, 0, 1, 1);
    return { actual, expected, rgb: [...context.getImageData(0, 0, 1, 1).data].slice(0, 3) };
  });
  expect(colors.actual).toBe(colors.expected);
  expect(colors.rgb[0], "失败率必须以实际红色呈现").toBeGreaterThan(colors.rgb[1]!);
  expect(colors.rgb[0]).toBeGreaterThan(colors.rgb[2]!);

  const idle = data.models.find(model => model.succeeded_attempts + model.failed_attempts === 0)!;
  await page.getByRole("searchbox", { name: "搜索监测模型", exact: true }).fill(idle.model.id);
  await expect(rows).toHaveCount(1);
  await expect(rows.first().getByRole("cell").nth(2).getByText("—", { exact: true })).toBeVisible();
  expect(writes).toEqual([]);
  expect(errors).toEqual([]);
});
