import { test, expect } from "@playwright/test";
import { start, PASSWORD, type Fixture } from "./fixture";

let fx: Fixture;

test.beforeAll(async () => {
  fx = await start();
});
test.afterAll(() => fx?.stop());

/**
 * 「另一个客户端」：独立浏览器上下文，接口调用走页面内的 `fetch`。
 *
 * 为什么必须在页面里发：会话 cookie 带 `Secure`，而 E2E 跑在 http:// 上。
 * 页面享有 localhost 的 Secure 豁免，Playwright 的 `APIRequestContext`
 * 没有 —— 实测同一个上下文里 `ctx.request` 拿到 `authed:false`，页面内
 * `fetch` 拿到 `authed:true`。用前者测出来的只会是 401。
 *
 * 这条约束顺带说明控制台只能部署在 HTTPS 后面。
 */
async function otherClient(browser: import("@playwright/test").Browser) {
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  await page.goto(fx.previewUrl);
  await page.getByLabel("口令").fill(PASSWORD);
  await page.getByRole("button", { name: "进入" }).click();
  await expect(page.getByRole("tab", { name: "概览" })).toBeVisible();

  return {
    async get(path: string): Promise<{ status: number; body: unknown }> {
      return page.evaluate(async (p) => {
        const r = await fetch(p, { credentials: "same-origin" });
        return { status: r.status, body: await r.json().catch(() => null) };
      }, path);
    },
    async put(path: string, data: unknown): Promise<{ status: number }> {
      return page.evaluate(
        async ([p, d]) => {
          const r = await fetch(p as string, {
            method: "PUT",
            credentials: "same-origin",
            headers: { "content-type": "application/json" },
            body: JSON.stringify(d),
          });
          return { status: r.status };
        },
        [path, data] as const,
      );
    },
  };
}

async function signIn(page: import("@playwright/test").Page) {
  await page.goto(fx.previewUrl);
  await page.getByLabel("口令").fill(PASSWORD);
  await page.getByRole("button", { name: "进入" }).click();
  await expect(page.getByRole("tab", { name: "概览" })).toBeVisible();
}

test("未登录时不渲染任何数据", async ({ page }) => {
  await page.goto(fx.previewUrl);
  await expect(page.getByRole("button", { name: "进入" })).toBeVisible();
  // 控制台能看到明文上游密钥，所以登录前连界面骨架都不该出现。
  await expect(page.getByRole("tab", { name: "供应商" })).toHaveCount(0);
});

/**
 * 主题按钮曾经存在于旧界面，移植时被漏掉了 —— 一个功能就这样静默消失。
 * 这条测试是它的回归保护。
 *
 * 默认暗色是产品判断（这个界面是仪表间，仪表间是暗的），但显式选择必须
 * 一直生效，包括刷新之后 —— 否则它不是「选择」而是「本次会话的临时效果」。
 */
test("主题可切换，且选择在刷新后仍然生效", async ({ page }) => {
  await page.goto(fx.previewUrl);
  const root = page.locator("html");
  await expect(root).toHaveAttribute("data-theme", "dark");

  await page.getByRole("button", { name: "切换到浅色" }).click();
  await expect(root).toHaveAttribute("data-theme", "light");

  await page.reload();
  await expect(root).toHaveAttribute(
    "data-theme",
    "light",
  );
  // 按钮的文案要跟着状态走，否则它在说自己是当前主题而不是可切换到的。
  await expect(page.getByRole("button", { name: "切换到深色" })).toBeVisible();
});

test("口令错误时报错且不进入", async ({ page }) => {
  await page.goto(fx.previewUrl);
  await page.getByLabel("口令").fill("wrong");
  await page.getByRole("button", { name: "进入" }).click();
  await expect(page.getByText("口令不正确")).toBeVisible();
  await expect(page.getByRole("tab", { name: "概览" })).toHaveCount(0);
});

/**
 * 网关状态胶囊必须真的是个胶囊。
 *
 * 改成侧栏骨架时，整段 `.pill` 规则被我连着区段一起删掉了 —— 状态退成了
 * 一行纯文字，边框和状态点全没。功能上没坏，所以别的测试全绿，只有肉眼
 * 看截图才发现。这条测试是它的回归保护：断言计算样式，而不是断言那段文字
 * 存在。
 */
/**
 * 状态从发光胶囊改成账簿上盖的方戳（台账方向）。形状变了，要守的东西没变：
 * 它必须仍是一个**有边框、带状态点、颜色跟状态走**的标记，而不是退成一行
 * 纯文字 —— 之前有一次整段样式被连区段删掉，健康状态就是这样悄悄没的。
 */
test("网关状态是一个带边框和状态点的方戳", async ({ page }) => {
  await signIn(page);
  const pill = page.locator(".book-foli .pill").first();
  await expect(pill).toBeVisible();

  const shape = await pill.evaluate((el) => {
    const cs = getComputedStyle(el);
    const dot = el.querySelector(".dot");
    const ds = dot ? getComputedStyle(dot) : null;
    return {
      radius: parseFloat(cs.borderRadius),
      borderWidth: parseFloat(cs.borderTopWidth),
      borderTransparent: cs.borderTopColor === "rgba(0, 0, 0, 0)",
      inkMatchesBorder: cs.color === cs.borderTopColor,
      dotSize: ds ? parseFloat(ds.width) : 0,
    };
  });

  // 账页上没有圆角。戳是方的 —— 这一条同时保证它没被退回成圆胶囊。
  expect(shape.radius).toBe(0);
  expect(shape.borderWidth).toBeGreaterThan(0);
  expect(shape.borderTransparent).toBe(false);
  // 墨色和框色同源：状态色确实上到了这个戳上，而不是只剩个灰框。
  expect(shape.inkMatchesBorder).toBe(true);
  expect(shape.dotSize).toBeGreaterThan(2);
});

/**
 * 侧栏按要求换成了页眉下的一排页签。要守的是**导航仍然是一整排、横向排布、
 * 九个都在**，而不是塌成竖排或被折叠进某个菜单里。
 */
test("导航是页眉下横排的一行页签", async ({ page }) => {
  await signIn(page);
  await expect(page.locator(".rail")).toHaveCount(0);

  const nav = page.locator("nav.tabs");
  await expect(nav).toBeVisible();

  // 横排：第二个页签在第一个的右边，且两者在同一行。
  const a = await page.getByRole("tab", { name: "概览" }).boundingBox();
  const b = await page.getByRole("tab", { name: "用量" }).boundingBox();
  expect(a && b).toBeTruthy();
  if (a && b) {
    expect(b.x).toBeGreaterThan(a.x + a.width - 2);
    expect(Math.abs(b.y - a.y)).toBeLessThan(3);
  }

  // 页签压在页眉的重线下面，而不是飘在页面中段。
  const head = await page.locator(".book-head").boundingBox();
  expect(head && a).toBeTruthy();
  if (head && a) expect(a.y).toBeGreaterThan(head.y + head.height - 2);

  // 正文因此拿到整页宽度：不再有左边那条竖栏占位。
  const frame = await page.locator(".frame").boundingBox();
  const panel = await page.locator(".panel").first().boundingBox();
  expect(frame && panel).toBeTruthy();
  if (frame && panel) expect(panel.x - frame.x).toBeLessThan(60);
});

test("登录后九个页签都能打开", async ({ page }) => {
  await signIn(page);
  for (const name of [
    "概览",
    "用量",
    "供应商",
    "模型与定价",
    "调用方密钥",
    "限流与预算",
    "全部资源",
    "调用日志",
    "配置原文",
  ]) {
    await page.getByRole("tab", { name }).click();
    await expect(page.getByRole("tab", { name })).toHaveAttribute("aria-selected", "true");
  }
});

test("新增供应商会真的写进配置文件", async ({ page }) => {
  await signIn(page);
  await page.getByRole("tab", { name: "供应商" }).click();
  await page.getByLabel("名称").fill("e2e-provider");
  await page.getByLabel("API 密钥").fill("sk-e2e-secret");
  await page.getByRole("button", { name: /保存并重载网关/ }).click();

  await expect(page.getByText(/已保存/)).toBeVisible();
  expect(fx.read()).toContain("e2e-provider");
});

test("列表里的密钥被遮蔽，只显示两端", async ({ page }) => {
  await signIn(page);
  await page.getByRole("tab", { name: "供应商" }).click();
  const row = page.getByRole("row", { name: /e2e-provider/ });
  await expect(row).toBeVisible();
  // 完整密钥绝不能出现在表格里。
  await expect(row).not.toContainText("sk-e2e-secret");
  await expect(row).toContainText("…");
});

test("只填一半的定价被拦下，且不写入配置", async ({ page }) => {
  await signIn(page);
  await page.getByRole("tab", { name: "模型与定价" }).click();
  await page.getByLabel("对外模型名").fill("half-priced");
  await page.getByLabel("上游模型名").fill("gpt-4o-mini");
  // 只给缓存读价，不给输入/输出价。
  await page.getByLabel("缓存读 USD / 1k").fill("0.00025");
  await page.getByRole("button", { name: /保存并重载网关/ }).click();

  await expect(page.getByText(/填了定价就必须同时给出输入价和输出价/)).toBeVisible();
  // 关键：不能悄悄创建一个「未定价」的模型 —— 那种模型静默豁免于所有花费上限。
  expect(fx.read()).not.toContain("half-priced");
});

test("填全定价后能保存，缓存两项按供应商倍率自动换算", async ({ page }) => {
  await signIn(page);
  await page.getByRole("tab", { name: "模型与定价" }).click();
  await page.getByLabel("对外模型名").fill("fully-priced");
  await page.getByLabel("上游模型名").fill("gpt-4o-mini");
  await page.getByLabel("输入 USD / 1k").fill("0.001");

  // openai 的公开倍率是读 0.5×，所以缓存读价应被自动填成 0.0005。
  await expect(page.getByLabel("缓存读 USD / 1k")).toHaveValue("0.0005");

  await page.getByLabel("输出 USD / 1k").fill("0.002");
  await page.getByRole("button", { name: /保存并重载网关/ }).click();
  await expect(page.getByText(/已保存/)).toBeVisible();

  const yaml = fx.read();
  expect(yaml).toContain("fully-priced");
  expect(yaml).toContain("cached_input_per_1k");
});

test("手改过缓存价后，新一轮填价不会覆盖手工输入", async ({ page }) => {
  await signIn(page);
  await page.getByRole("tab", { name: "模型与定价" }).click();
  await page.getByLabel("缓存读 USD / 1k").fill("0.42");
  await page.getByLabel("输入 USD / 1k").fill("0.001");
  // 手工值必须留住 —— 自动换算只在这两格还归它所有时才动。
  await expect(page.getByLabel("缓存读 USD / 1k")).toHaveValue("0.42");
});

test("铸密钥：明文显示一次，配置里只有散列", async ({ page }) => {
  await signIn(page);
  await page.getByRole("tab", { name: "调用方密钥" }).click();
  await page.getByLabel("名称").fill("e2e-caller");
  await page.getByRole("button", { name: /生成并重载网关/ }).click();

  const shown = page.getByText(/密钥明文——只显示这一次/);
  await expect(shown).toBeVisible();
  const plaintext = (await page.locator("pre").first().innerText()).trim();
  expect(plaintext.length).toBeGreaterThan(16);

  // 明文的出现只说明铸造成功，不说明保存已经落盘 —— 界面刻意先显示明文
  // 再去保存（顺序反了的话，保存失败就永久丢掉一把已生效的密钥）。所以要
  // 等保存自己的结果出来。
  await expect(page.getByText(/已保存/)).toBeVisible();

  const yaml = fx.read();
  expect(yaml).toContain("e2e-caller");
  // 明文绝不能落盘。
  expect(yaml).not.toContain(plaintext);
  expect(yaml).toContain("key_hash");
});

test("过小的花费上限被拦下，而不是四舍五入成 0", async ({ page }) => {
  await signIn(page);
  await page.getByRole("tab", { name: "调用方密钥" }).click();
  await page.getByLabel("名称").fill("tiny-budget");
  await page.getByLabel("花费上限 USD（留空不限）").fill("0.0000001");
  await page.getByRole("button", { name: /生成并重载网关/ }).click();

  await expect(page.getByText(/小于最小可表示单位/)).toBeVisible();
  expect(fx.read()).not.toContain("tiny-budget");
});

test("配置原文页能保存，坏 YAML 被校验挡住且不落盘", async ({ page }) => {
  await signIn(page);
  await page.getByRole("tab", { name: "配置原文" }).click();
  const box = page.locator("textarea");
  const before = fx.read();

  // 引用一个不存在的供应商密钥 —— 网关自己的校验器必须拒绝。
  await box.fill(`${before}\n  - display_name: bad-ref\n    provider: openai\n    provider_key: no-such-pk\n    model_name: x\n`);
  await page.getByRole("button", { name: "校验并保存" }).click();

  await expect(page.getByText(/校验未通过|保存失败|解析失败/)).toBeVisible();
  expect(fx.read()).not.toContain("bad-ref");
});

test("并发保存：过期版本被拒，先保存的改动完好", async ({ page, browser }) => {
  await signIn(page);
  await page.getByRole("tab", { name: "供应商" }).click();

  // 另一个「标签页」直接用当前磁盘版本写一次。
  const api = await otherClient(browser);
  const cur = (await api.get("/api/file")).body as { doc: Record<string, unknown>; version: string };
  const other = structuredClone(cur.doc);
  other.provider_keys.push({
    display_name: "racer",
    provider: "openai",
    api_key: "sk-racer",
  });
  const win = await api.put("/api/file", {
    doc: other,
    base_version: cur.version,
    client_contract: 1,
  });
  expect(win.status).toBe(200);

  // 本页手里的版本现在过期了。它的保存必须被拒，且要看得到原因。
  await page.getByLabel("名称").fill("loser");
  await page.getByLabel("API 密钥").fill("sk-loser");
  await page.getByRole("button", { name: /保存并重载网关/ }).click();

  await expect(page.getByText(/配置在你编辑期间已被改动/)).toBeVisible();
  const yaml = fx.read();
  expect(yaml).toContain("racer");
  expect(yaml).not.toContain("loser");
});

test("删除密钥会连带删掉引用它的策略", async ({ page, browser }) => {
  await signIn(page);

  // 先造一把带花费上限的密钥（策略的 scope_ref 指向它）。
  const api = await otherClient(browser);
  const cur = (await api.get("/api/file")).body as { doc: Record<string, unknown>; version: string };
  const doc = structuredClone(cur.doc);
  doc.api_keys.push({
    display_name: "doomed",
    key_hash: "1".repeat(64),
    allowed_models: ["*"],
  });
  doc.rate_limit_policies = [
    {
      name: "doomed-spend",
      scope: "api_key",
      scope_ref: "doomed",
      window: "day",
      max_spend_micro_usd: 1_000_000,
    },
  ];
  const seeded = await api.put("/api/file", {
    doc,
    base_version: cur.version,
    client_contract: 1,
  });
  expect(seeded.status).toBe(200);

  await page.reload();
  await page.getByRole("tab", { name: "调用方密钥" }).click();
  page.on("dialog", (d) => void d.accept());
  await page
    .getByRole("row", { name: /doomed/ })
    .first()
    .getByRole("button", { name: "删除" })
    .click();

  await expect(page.getByRole("row", { name: /doomed/ })).toHaveCount(0);
  const yaml = fx.read();
  // 孤儿策略会让整份配置校验失败，网关从此再也重载不了。
  expect(yaml).not.toContain("doomed-spend");
  expect(yaml).not.toContain("doomed");
});

test("后端确实在 /api/session 上报出契约版本", async ({ request }) => {
  // 偏移检测的整个前提是这个字段真的存在且是个数。它缺失时前端会把它读成
  // 0，于是每次启动都报偏移 —— 一个必然误报的告警比没有告警更糟。
  //
  // 这里只断言契约的存在与形状，不 stub 一个假的版本号来看界面反应：
  // 界面对不一致的处理由它自己的单元断言覆盖（见下），而伪造后端响应会让
  // 这条用例测的是 mock 而不是产品。
  const r = await request.get(`${fx.consoleUrl}/api/session`);
  expect(r.status()).toBe(200);
  const j = await r.json();
  expect(typeof j.api_contract).toBe("number");
  expect(j.api_contract).toBeGreaterThan(0);
});

test("界面声明契约，于是服务端能强制它带并发版本", async ({ browser }) => {
  const api = await otherClient(browser);
  // 声明了契约却不带 base_version 的调用方只可能是一个不知道这个字段的
  // 旧界面 —— 放它过去就是把丢失更新重新放回来。
  const cur = (await api.get("/api/file")).body as { doc: Record<string, unknown>; version: string };
  const refused = await api.put("/api/file", { doc: cur.doc, client_contract: 1 });
  expect(refused.status).toBe(409);

  // 不声明契约的调用方（脚本、curl）继续走逃生口，否则所有既有的非浏览器
  // 调用方会一夜之间全部失败。
  const allowed = await api.put("/api/file", { doc: cur.doc });
  expect(allowed.status).toBe(200);
});

/**
 * 以下三条盯的是表盘和「已配置」这一行的可观察约定。
 *
 * 夹具把管理 API 和 Prometheus 都指向 127.0.0.1:1（不可达），所以这里
 * 恰好覆盖的是最容易悄悄退化的那个状态：读不到。这一类退化不报错，只是
 * 把「读不到」显示成「是零」—— 而这两件事要采取的动作完全相反。
 */

test("管理 API 读不到时，「已配置」说读不到，而不是显示 0", async ({ page }) => {
  await signIn(page);
  const row = page.locator(".entry", { hasText: "已配置" });
  await expect(row).toBeVisible();

  await expect(row).toContainText(/读不到/);
  // 关键：不能把读不到渲染成一个数。
  await expect(row.locator(".val")).not.toContainText(/\d/);
});

test("没配上限时不结账，也不假装余额为零", async ({ page }) => {
  await signIn(page);
  const bal = page.locator(".balance");
  await expect(bal).toBeVisible();

  await expect(bal).toContainText(/未设花费上限/);
  // 没有上限就结不出余额：印一个 $0.00 出来读起来是「一分不剩」。
  // 盯的是那道结账双线和余额行 —— 不能盯「余额」这两个字，空态的引导语
  // 本身就该提到它（「要看余额，先配一条上限」）。
  await expect(bal.locator(".balance-sum")).toHaveCount(0);
  await expect(bal.locator(".balance-rule")).toHaveCount(0);
});

test("给密钥配上花费上限后，概览就把这笔账结出来", async ({ page }) => {
  await signIn(page);
  await page.getByRole("tab", { name: "调用方密钥" }).click();
  await page.getByLabel("名称").fill("gauge-scope");
  await page.getByLabel("花费上限 USD（留空不限）").fill("2");
  await page.getByRole("button", { name: /生成并重载网关/ }).click();
  await expect(page.getByText(/已保存|明文/)).toBeVisible();

  await page.getByRole("tab", { name: "概览" }).click();
  const bal = page.locator(".balance");
  // 上限来自刚写进去的策略，跟指标后端无关，所以这一笔必须出现。
  await expect(bal).toContainText("$2.00");
  await expect(bal).not.toContainText(/未设花费上限/);

  // 但这套夹具里 Prometheus 不可达，已花费读不到 —— 于是这笔账**结不出来**，
  // 而这正是要守的：不知道花了多少就不能算余额，宁可说读不到。
  await expect(bal).toContainText(/读不到窗口内花费/);
  await expect(bal.locator(".balance-sum")).toHaveCount(0);
});
