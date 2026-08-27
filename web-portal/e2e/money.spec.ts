import { test, expect } from "@playwright/test";
import { startMoney, ADMIN_TOKEN, CALLER_KEY, type MoneyFixture } from "./money.fixture";

/**
 * 钱路：真网关 + 真流量 + 真指标 + 真账本 + 真停用。
 *
 * 计划初稿在这里留了个洞 —— 要求覆盖「消费耗尽 → 密钥被停用」，却没说消费从
 * 哪来。这一层给出答案：网关跑在文件模式（不需要 etcd），请求打进去、按配置的
 * 定价折算出花费、打到指标；门户对账扣余额；余额归零把密钥写成 disabled；
 * 网关据此拒绝下一次请求。
 *
 * 只有上游 LLM 与 PromQL 引擎是垫片，其余全是生产件。
 */

let fx: MoneyFixture;
/** 第一条用例铸出来的明文，第二条要用它证明网关认得这把密钥。 */
let mintedKey = "";

test.describe.configure({ timeout: 180_000 });

test.beforeAll(async () => {
  fx = await startMoney();
});
test.afterAll(() => fx?.stop());

async function grant(micro: number): Promise<void> {
  const list = await fetch(`${fx.portalUrl}/admin/users`, {
    headers: { authorization: `Bearer ${ADMIN_TOKEN}` },
  }).then((r) => r.json());
  const u = list.users.find((x: { user_id: string }) => x.user_id === fx.userId);
  expect(u, "管理端列表里找不到 e2e 用户").toBeTruthy();
  const r = await fetch(`${fx.portalUrl}/admin/users/${fx.userId}/grant`, {
    method: "POST",
    headers: { authorization: `Bearer ${ADMIN_TOKEN}`, "content-type": "application/json" },
    body: JSON.stringify({ micro_usd: micro, note: "e2e" }),
  });
  expect(r.status).toBe(200);
}

/** 等对账环跑过若干轮。轮询周期 15 秒，所以这里给足时间。 */
async function waitFor(cond: () => Promise<boolean>, label: string, ms = 90_000) {
  const until = Date.now() + ms;
  while (Date.now() < until) {
    if (await cond()) return;
    await new Promise((r) => setTimeout(r, 1000));
  }
  throw new Error(`超时等待：${label}`);
}

/** 当前余额。 */
async function balance(cookie: string): Promise<number> {
  const b = await fetch(`${fx.portalUrl}/api/balance`, { headers: { cookie } }).then((r) =>
    r.json(),
  );
  return b.balance_micro_usd as number;
}

// 这一组用例**按声明顺序**跑在同一份夹具上，所以每条都自带前提、不依赖前一条
// 的残留。第一版让最后一条靠「打请求把余额耗到零」来准备起点 —— 那是错的：
// 每次调用花 $1，而对账每 15 秒才跑一轮，循环拿着过期的正余额疯狂发请求，把
// 余额打到很深的负数，之后发放的额度根本填不平。

test("自助建的密钥在零余额时生下来就是停用的_网关据此拒绝", async () => {
  const cookie = await fx.sessionCookie();
  // 刚注册，余额为零 —— 这条必须跑在任何发放之前。
  expect(await balance(cookie)).toBe(0);

  const minted = await fetch(`${fx.portalUrl}/api/keys`, {
    method: "POST",
    headers: { cookie, "content-type": "application/json" },
    body: JSON.stringify({ label: "money-e2e" }),
  }).then((r) => r.json());
  expect(minted.plaintext).toMatch(/^sk-aisix-[0-9a-f]{64}$/);
  expect(minted.disabled).toBe(true);

  // 网关拒绝。此刻 401 还分不清「不认识」与「已停用」——「发放额度后同一把
  // 密钥转为 200」那条用例才是证明它确实被认出来的地方。
  expect(await fx.chatWith(minted.plaintext)).toBe(401);
  mintedKey = minted.plaintext;
});

test("消费耗尽后密钥被停用_网关随即拒绝_补额后恢复", async () => {
  // 从零起。给 $1.5：一次调用花 $1，第二次就把余额压成负的。
  await grant(1_500_000);
  await waitFor(async () => (await fx.chat()) === 200, "发放后预置密钥可用");
  expect(await fx.chat()).toBe(200);

  await waitFor(
    async () => /disabled:\s*true/.test(fx.readResources()),
    "余额耗尽后密钥被停用",
  );
  // 网关读到 disabled 之后必须拒绝 —— 这是整条链路真正闭合的那一步。
  await waitFor(async () => (await fx.chat()) === 401, "网关拒绝已停用的密钥");

  await grant(20_000_000);
  await waitFor(async () => (await fx.chat()) === 200, "补额后网关重新放行");
});

test("同一把自助密钥在有余额后被网关放行_消费记在这个用户名下", async () => {
  expect(mintedKey, "第一条用例没铸出密钥").toBeTruthy();
  // 上一条用例结束时余额是正的，所以这把密钥应当已被对账环启用。
  //
  // 它能通同时证明三件事：散列算法与网关一致、`user_id` 写对了、SIGHUP 送到了。
  await waitFor(
    async () => (await fx.chatWith(mintedKey)) === 200,
    "自助密钥被网关放行",
  );

  const metrics = await fetch(fx.metricsUrl).then((r) => r.text());
  expect(metrics).toContain(`user_id="${fx.userId}"`);
});

test("真实消费被网关按定价折算并带上_user_id", async () => {
  expect(await fx.chat()).toBe(200);
  const metrics = await fetch(fx.metricsUrl).then((r) => r.text());
  const line = metrics
    .split("\n")
    .find((l) => l.startsWith("aisix_llm_spend_micro_usd_total{") && l.includes(fx.userId));
  expect(line, "花费指标里没有这个用户的序列").toBeTruthy();
  // 1000 输入 + 1000 输出，两边都是 $0.5/1k → 每次调用恰好 $1.00。
  expect(Number(line!.slice(line!.lastIndexOf("}") + 1).trim())).toBeGreaterThanOrEqual(1_000_000);
});

test("门户把累计发放额推给网关_网关按它精确收口", async () => {
  // 不手写策略：下推本来就是生产路径的一部分，手写一条测的是别的东西。
  // 这一条从头到尾走真实链路 —— 发放 → 门户写策略 → SIGHUP → 网关据此拒绝。
  //
  // 自带前提，不依赖前面用例的残留：单独跑时那些发放不会发生，累计额是 0，
  // 门户就不写策略 —— 症状是「等不到配置」，跟真 bug 一模一样。
  await grant(3_000_000);
  const cookie = await fx.sessionCookie();
  const granted: number = await fetch(`${fx.portalUrl}/api/balance`, { headers: { cookie } })
    .then((r) => r.json())
    .then((b: { entries: { delta_micro_usd: number }[] }) =>
      b.entries
        .filter((e) => e.delta_micro_usd > 0)
        .reduce((n, e) => n + e.delta_micro_usd, 0),
    );
  expect(granted).toBeGreaterThan(0);

  await waitFor(async () => {
    const doc = fx.readResources();
    return (
      doc.includes(`portal-allowance-${fx.userId}`) &&
      doc.includes(`granted_micro_usd: ${granted}`)
    );
  }, `门户把累计发放额 ${granted} 写进配置`);

  // 打到网关按这条策略拒绝。每次 $1，累计消费越过发放总额即拒。
  //
  // 按需并发，不是每秒一次地慢慢烧：跑整套时前面的用例已经发放了几十美元，
  // 串行烧的时长会随那个数增长，撞上超时 —— 而那种失败长得跟「闸没生效」
  // 一模一样。需要多少次是算得出来的，就并发打多少次。
  const needed = Math.ceil(granted / 1_000_000) + 2;
  for (let fired = 0; fired < needed; fired += 20) {
    const batch = Math.min(20, needed - fired);
    await Promise.all(Array.from({ length: batch }, () => fx.chatWithStatus()));
  }
  await waitFor(async () => (await fx.chatWithStatus()) === 429, "累计发放额用尽后被拒");

  const r = await fetch(`${fx.proxyUrl}/v1/chat/completions`, {
    method: "POST",
    headers: { authorization: `Bearer ${CALLER_KEY}`, "content-type": "application/json" },
    body: JSON.stringify({ model: "gpt-4o-mini", messages: [{ role: "user", content: "hi" }] }),
  });
  expect(r.status).toBe(429);
  const body = await r.text();
  // 必须是「钱用完了」而不是「太快了」：两者同为 429，处置完全不同。
  expect(body).toContain("allowance exhausted");
  expect(body).toContain("top up");
  expect(body).toContain("billing_error");
  // 不给 retry-after —— 等是等不回来的。
  expect(r.headers.get("retry-after")).toBeNull();

  // 补额之后放行，且不需要重置任何东西：推下去的是更大的一个数。
  await grant(50_000_000);
  await waitFor(async () => (await fx.chatWithStatus()) === 200, "补额后网关重新放行");
});

/** 把某个用户的总额度**设定**成某个数（绝对值）。 */
async function setQuota(micro: number): Promise<void> {
  const r = await fetch(`${fx.portalUrl}/admin/users/${fx.userId}/quota`, {
    method: "POST",
    headers: { authorization: `Bearer ${ADMIN_TOKEN}`, "content-type": "application/json" },
    body: JSON.stringify({ micro_usd: micro, note: "e2e 设定" }),
  });
  expect(r.status).toBe(200);
}

/** 自助铸一把密钥。返回明文与短名（额度接口与策略名都按短名找得到）。 */
async function mint(cookie: string, label: string): Promise<{ plaintext: string; name: string }> {
  const m = await fetch(`${fx.portalUrl}/api/keys`, {
    method: "POST",
    headers: { cookie, "content-type": "application/json" },
    body: JSON.stringify({ label }),
  }).then((r) => r.json());
  expect(m.plaintext, "没铸出明文").toBeTruthy();
  return { plaintext: m.plaintext, name: m.name };
}

async function setKeyQuota(
  cookie: string,
  name: string,
  micro: number,
): Promise<{ status: number; body: string }> {
  const r = await fetch(`${fx.portalUrl}/api/keys/${encodeURIComponent(name)}/quota`, {
    method: "PUT",
    headers: { cookie, "content-type": "application/json" },
    body: JSON.stringify({ micro_usd: micro }),
  });
  return { status: r.status, body: await r.text() };
}

test("管理员设定的额度是绝对值_调低之后调用被拒_调高即恢复", async () => {
  // 先抬高再调低。「设定」若被实现成「追加」，网关收到的会是两次之和
  // （507000000），于是本该被收口的用户继续畅通 —— 静默白送，没有任何报错。
  await setQuota(500_000_000);
  await waitFor(
    async () => /granted_micro_usd: 500000000\b/.test(fx.readResources()),
    "门户把 500000000 推给网关",
  );
  await setQuota(7_000_000);
  await waitFor(async () => {
    const doc = fx.readResources();
    return (
      doc.includes(`portal-allowance-${fx.userId}`) && /granted_micro_usd: 7000000\b/.test(doc)
    );
  }, "门户把设定后的总额度 7000000 推给网关（不是两次之和）");

  // 自带前提：不管前面用例花掉多少，这里再花掉 $10，保证花费越过 $7。
  for (let i = 0; i < 10; i++) await fx.chatWithStatus();

  // 拒绝可能来自两个闸，两者都是「钱不够了」的正确处置，取决于哪个先到：
  //   * 网关侧的额度闸 → 429，带「top up to continue」；
  //   * 门户侧的余额闸（额度被调到已花费之下，账面为负）→ 把密钥写成停用，401。
  // 断言「被拒」而不是断言某一个码 —— 钉死其中一个等于断言两个闸的赛跑结果。
  await waitFor(async () => {
    const s = await fx.chatWithStatus();
    return s === 401 || s === 429;
  }, "调低总额度后调用被拒");

  // 调高就恢复，不需要重置任何计数器 —— 推下去的只是一个更大的数。
  await setQuota(1_000_000_000);
  await waitFor(async () => (await fx.chatWithStatus()) === 200, "调高总额度后恢复放行");
});

test("每把密钥的额度各自收口_且各把之和不得超过总额度", async () => {
  const cookie = await fx.sessionCookie();
  // 把总额度抬到远高于已消费，让**用户**那一层的闸不成为约束 —— 这一条要证的
  // 是密钥这一层，两层混在一起就分不清是谁拦下的。
  await setQuota(1_000_000_000);
  const a = await mint(cookie, "甲");
  const b = await mint(cookie, "乙");
  await waitFor(async () => (await fx.chatWith(a.plaintext)) === 200, "新铸的密钥被启用");
  expect(await fx.chatWith(b.plaintext)).toBe(200);

  // 给甲设 $2 —— 每次调用 $1，它很快就该自己撞墙；乙不设限。
  expect((await setKeyQuota(cookie, a.name, 2_000_000)).status).toBe(200);
  await waitFor(async () => {
    const doc = fx.readResources();
    return doc.includes(`portal-key-${a.name}`) && doc.includes("scope: api_key");
  }, "门户把这把密钥的额度推成 api_key 域的策略");

  await waitFor(async () => (await fx.chatWith(a.plaintext)) === 429, "甲自己的额度用尽后被拒");
  // 乙仍然可用 —— 这是「每把密钥各自一份」的全部含义。闸若错挂在用户身上，
  // 乙会跟着一起被拒；那在用户看来是「另一把密钥无缘无故不能用了」。
  expect(await fx.chatWith(b.plaintext)).toBe(200);

  // 和不得超过总额度：甲已占 $2，再给乙要满额就必须被拒。
  const r = await setKeyQuota(cookie, b.name, 1_000_000_000);
  expect(r.status).toBe(409);
  expect(r.body).toContain("available_micro_usd");
  // 被拒之后乙不该被改坏 —— 拒绝要是「先写再报错」，用户会莫名多出一道闸。
  expect(await fx.chatWith(b.plaintext)).toBe(200);

  // 吊销一把**带额度**的密钥必须成。它的策略要跟密钥同批消失：晚一轮撤的话，
  // 中间那份文档里策略指着已不存在的密钥，网关整份拒收 —— 生产上真发生过，
  // 是写前校验把它拦下来的（否则会静默冻住整个配置，含停用闸）。
  const del = await fetch(`${fx.portalUrl}/api/keys/${encodeURIComponent(a.name)}`, {
    method: "DELETE",
    headers: { cookie },
  });
  expect(del.status, await del.text()).toBe(200);
  const doc = fx.readResources();
  expect(doc).not.toContain(`portal-key-${a.name}`);
  expect(doc).not.toContain(a.name);
  // 配置仍是网关收得下的：乙照常可用，说明这次重载没有被整份拒收。
  await waitFor(async () => (await fx.chatWith(b.plaintext)) === 200, "吊销之后乙仍然可用");
});
