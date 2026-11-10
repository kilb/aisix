import { randomUUID } from "node:crypto";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

/**
 * 价格 · 价格库 against the real control plane through the built `dist/`.
 *
 * The stack cannot reach the dataset's published address, so every test
 * fills the library the offline way an operator would: by uploading the
 * dataset file on 导入审核. The file is the committed snapshot of the real
 * dataset, and every price asserted here is read from that file, the input
 * the operator gave, never from the product's answers.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

/** The committed snapshot: a real dataset file, not a hand-made fixture. */
const SNAPSHOT = join(process.cwd(), "..", "tests", "e2e", "fixtures", "pricing-import", "genai-prices-v2-7de757d.json");
/** The revision its file name carries. */
const REVISION = "genai-prices-v2-7de757d";

interface SnapshotProvider {
  id: string;
  pricing_urls?: string[];
  models: { id: string; prices: Record<string, number> }[];
}

/** What the snapshot says of `provider`'s `model`: its flat input and output prices, and the provider's first page. */
function snapshot(provider: string, model: string): { input: string; output: string; page: string } {
  const all = JSON.parse(readFileSync(SNAPSHOT, "utf8")) as SnapshotProvider[];
  const p = all.find((x) => x.id === provider);
  const m = p?.models.find((x) => x.id === model);
  if (!p || !m) throw new Error(`no ${provider}/${model} in the snapshot`);
  return { input: String(m.prices.input_mtok), output: String(m.prices.output_mtok), page: p.pricing_urls?.[0] ?? "" };
}

/** A USD rate per million as `$x / 百万 tokens`, the Dashboard's one way of writing it. */
function usd(rate: string): string {
  return `$${rate} / 百万 tokens`;
}

/** `usd × cny` per million as `¥x.xx`: exact, at least two decimals, as every ¥ figure is written. */
function cny(usd: string, rate: string): string {
  const micros = BigInt(Math.round(Number(usd) * 1_000_000));
  const e4 = BigInt(Math.round(Number(rate) * 10_000));
  const scale = 10n ** 10n;
  const value = micros * e4;
  const whole = (value / scale).toString().replace(/\B(?=(\d{3})+$)/g, ",");
  const frac = (value % scale).toString().padStart(10, "0").replace(/0+$/, "").padEnd(2, "0");
  return `¥${whole}.${frac}`;
}

async function login(page: Page): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(stack.operator.email);
  await page.getByLabel("口令", { exact: true }).fill(stack.operator.password);
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
}

async function section(page: Page, name: string): Promise<void> {
  await page.getByRole("group", { name: "价格分区" }).getByRole("button", { name, exact: true }).click();
  await expect(page.locator("main").getByText("载入中…")).toHaveCount(0);
}

/** Fills the library the offline way: the dataset file, uploaded on 导入审核. */
async function uploadDataset(page: Page): Promise<void> {
  await page.locator('[data-tab="pricing"]').click();
  await section(page, "导入审核");
  await page.getByLabel("数据集文件").setInputFiles(SNAPSHOT);
  await page.getByLabel("数据集版本").fill(REVISION);
  await page.getByRole("button", { name: "上传", exact: true }).click();
  await expect(page.getByRole("table", { name: "提议" })).toBeVisible();
}

/** An upstream account of `provider` that serves `groups`. */
async function account(provider: string, groups: string[]): Promise<string> {
  const suffix = randomUUID().slice(0, 8);
  const key = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: `acct-lib-${suffix}`,
      provider,
      api_key: `sk-acct-lib-${suffix}-upstream`,
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups,
    },
  });
  return key.id as string;
}

/** A public model `name` served by `accountId`. */
async function publicModel(name: string, accountId: string): Promise<string> {
  const pub = await stack.admin.post("/admin/public-models", { name });
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${pub.id}`, { priority: 0, weight: 1 });
  return pub.id as string;
}

function previewRow(page: Page, name: string): Locator {
  return page.getByRole("table", { name: "同步预览" }).getByRole("row").filter({ has: page.getByRole("cell", { name, exact: true }) });
}

test("离线上传数据集后，价格库按名称或供应商搜索，显示标价与价格页面", async ({ page }) => {
  await login(page);
  await uploadDataset(page);
  await section(page, "价格库");

  await expect(page.getByLabel("价格库状态")).toContainText("文件上传");
  await expect(page.getByLabel("价格库状态")).toContainText(REVISION);

  const commandA = snapshot("cohere", "command-a");
  await page.getByLabel("搜索模型").fill("command-a");
  const row = page
    .getByRole("table", { name: "价格库条目" })
    .getByRole("row")
    .filter({ has: page.getByRole("cell", { name: "cohere", exact: true }) })
    .filter({ has: page.getByText("command-a", { exact: true }) });
  await expect(row).toContainText(usd(commandA.input));
  await expect(row).toContainText(usd(commandA.output));
  await expect(row.getByRole("link", { name: "价格页面" })).toHaveAttribute("href", commandA.page);

  // By provider: the provider's own entries are listed, and nothing listed
  // fails to name what was searched for.
  await page.getByLabel("搜索模型").fill("cohere");
  const rows = page.getByRole("table", { name: "价格库条目" }).getByRole("rowgroup").last().getByRole("row");
  await expect(rows.filter({ hasNotText: /cohere/i })).toHaveCount(0);
  await expect(rows.filter({ has: page.getByRole("cell", { name: "cohere", exact: true }) }).first()).toBeVisible();
});

test("预览同步后一次确认：基准价写入，分组售价按换算率出现", async ({ page }) => {
  const suffix = randomUUID().slice(0, 6);
  const group = await stack.admin.createGroup(`grp-lib-${suffix}`, `价格库分组 ${suffix}`, "2");
  // One provider prices `command-a`: matched, whoever serves it.
  const cohere = await account("cohere", [group.name as string]);
  await publicModel("command-a", cohere);
  // Two providers price `gpt-4o`, and this model's own provider is neither.
  const relay = await account(`relay-${suffix}`, [group.name as string]);
  await publicModel("gpt-4o", relay);
  // Nothing in the dataset is called this.
  const nothing = `lib-nomatch-${suffix}`;
  await publicModel(nothing, relay);

  await login(page);
  await uploadDataset(page);
  await section(page, "价格库");
  const sync = page.getByRole("region", { name: "同步基准价" });
  await expect(sync.getByRole("table", { name: "同步预览" })).toBeVisible();

  const commandA = snapshot("cohere", "command-a");
  const created = previewRow(page, "command-a");
  await expect(created).toContainText("将新增");
  await expect(created).toContainText("cohere/command-a");
  await expect(created).toContainText(`输入 $${commandA.input} · 输出 $${commandA.output}`);
  const ambiguous = previewRow(page, "gpt-4o");
  await expect(ambiguous).toContainText("多个匹配");
  await expect(ambiguous).toContainText("openai/gpt-4o");
  await expect(ambiguous).toContainText("github-copilot/gpt-4o");
  await expect(previewRow(page, nothing)).toContainText("无匹配");

  // The one confirmation names how many models it writes: every row the
  // preview marks 将新增 or 将更新, and no other.
  const writes = await page
    .getByRole("table", { name: "同步预览" })
    .locator('tr[data-state="create"], tr[data-state="update"]')
    .count();
  expect(writes).toBeGreaterThan(0);
  const confirm = sync.getByRole("button", { name: `同步 ${writes} 个模型`, exact: true });
  await expect(confirm).toBeDisabled();
  await sync.getByRole("checkbox", { name: "已对照官方价格页核对" }).check();
  await confirm.click();
  await expect(page.locator(".toast").filter({ hasText: `已同步 ${writes} 个模型` }).first()).toBeVisible();

  // Read again: written, so nothing left to do; the others are as they were.
  await expect(previewRow(page, "command-a")).toContainText("一致");
  await expect(previewRow(page, "gpt-4o")).toContainText("多个匹配");
  await expect(previewRow(page, nothing)).toContainText("无匹配");

  // 基准价: priced from the third party, by this operator.
  await section(page, "基准价");
  const baseline = page
    .getByRole("table", { name: "基准价" })
    .getByRole("row")
    .filter({ has: page.getByRole("button", { name: "command-a", exact: true }) });
  await expect(baseline).toHaveAttribute("data-state", "priced");
  await expect(baseline).toContainText(usd(commandA.input));
  await expect(baseline).toContainText(usd(commandA.output));
  await expect(baseline).toContainText("第三方数据");
  await expect(baseline).toContainText("价格库同步");
  await expect(baseline).toContainText(stack.operator.email);
  // The ambiguous one was never written.
  await expect(
    page.getByRole("table", { name: "基准价" }).getByRole("row").filter({ has: page.getByRole("button", { name: "gpt-4o", exact: true }) }),
  ).toHaveAttribute("data-state", "unpriced");

  // The group's ¥ column: the synced baseline times the group's rate of 2.
  await section(page, "分组");
  const display = group.display_name as string;
  await page
    .getByRole("table", { name: "分组" })
    .getByRole("row")
    .filter({ has: page.getByRole("button", { name: display, exact: true }) })
    .getByRole("button", { name: display, exact: true })
    .click();
  await expect(page.getByRole("region", { name: `分组 ${display}` })).toBeVisible();
  const priced = page
    .getByRole("table", { name: "分组换算率" })
    .getByRole("row")
    .filter({ has: page.getByRole("cell", { name: "command-a", exact: true }) });
  await expect(priced.getByRole("cell").nth(5)).toHaveText(cny(commandA.input, "2"));
  await expect(priced.getByRole("cell").nth(6)).toHaveText(cny(commandA.output, "2"));
});

test("在基准价详情中为单个模型从价格库同步，需先勾选核对声明", async ({ page }) => {
  // Two providers price `command-r`; this model is served by one of them,
  // which is the one its baseline is read from.
  const cohere = await account("cohere", ["shared"]);
  await publicModel("command-r", cohere);
  const commandR = snapshot("cohere", "command-r");

  await login(page);
  await uploadDataset(page);
  await section(page, "基准价");
  await page
    .getByRole("table", { name: "基准价" })
    .getByRole("row")
    .filter({ has: page.getByRole("button", { name: "command-r", exact: true }) })
    .getByRole("button", { name: "command-r", exact: true })
    .click();
  const panel = page.getByRole("dialog", { name: "command-r 的基准价" });
  const one = panel.getByRole("region", { name: "从价格库同步" });
  await expect(one).toContainText("将新增");
  await expect(one).toContainText("cohere/command-r");
  await expect(one).toContainText(`输入 $${commandR.input} · 输出 $${commandR.output}`);

  const button = one.getByRole("button", { name: "从价格库同步", exact: true });
  await expect(button).toBeDisabled();
  await one.getByRole("checkbox", { name: "已对照官方价格页核对" }).check();
  await button.click();
  await expect(page.locator(".toast").filter({ hasText: "已从价格库同步" }).first()).toBeVisible();

  const now = panel.getByRole("region", { name: "现行基准价" });
  await expect(now).toContainText(usd(commandR.input));
  await expect(now).toContainText(usd(commandR.output));
  await expect(now).toContainText("价格库同步");
  await expect(one).toContainText("一致");
});
