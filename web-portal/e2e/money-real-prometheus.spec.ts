import { test, expect } from "@playwright/test";
import {
  startRealPromMoney,
  GRANT,
  PRICE_PER_CALL,
  type RealPromMoney,
} from "./money-real-prometheus.fixture";

/**
 * 钱路对着**真 Prometheus**：门户的计费查询（`resets()`、`offset`、`or`、
 * `and on(job, instance)`、回看窗口）由真 Prometheus 2.53.4 求值，它按固定间隔
 * 抓取 e2e 网关。`money.spec.ts` 那一套用的是 PromQL 垫片 —— 查询写错了垫片照样
 * 答得对，这一套量的就是垫片量不到的那部分。
 *
 * 每条用例的期望值只来自打出去的调用：每次 200 花 $1.00（固定 usage × 配置的
 * 定价），被拒的调用不花钱。断言的是用户在门户里看到的余额与消费流水。
 *
 * 每条用例自带一整套进程（门户、网关、Prometheus、新库、新用户），彼此不留残局。
 */

test.describe.configure({ timeout: 180_000 });

let fx: RealPromMoney | undefined;
test.afterEach(async () => {
  await fx?.stop();
  fx = undefined;
});

/** 抓取间隔不比对账周期（1 秒）短。当初「$5 只扣到 $0」就出在这种配比上。 */
const SCRAPE = "2s";

/** `label` 可以是函数：超时那一刻才求值，报出来的是最后一次看到的值。 */
async function waitFor(
  cond: () => Promise<boolean>,
  label: string | (() => string),
  ms = 30_000,
): Promise<void> {
  const until = Date.now() + ms;
  while (Date.now() < until) {
    if (await cond()) return;
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`超时等待：${typeof label === "string" ? label : label()}`);
}

function live(): RealPromMoney {
  if (!fx) throw new Error("夹具没起来");
  return fx;
}

/** 打 n 次，每次都必须被网关放行 —— 期望扣款按打通的次数算。 */
async function calls(n: number): Promise<void> {
  for (let i = 0; i < n; i++) expect(await live().chat()).toBe(200);
}

/** 等某个副本抓到「这个进程上这个用户累计花了 `value`」。 */
async function untilPromSees(
  replica: number,
  value: number,
  started: number,
  ms?: number,
): Promise<void> {
  const f = live();
  let seen: [number | null, number | null] = [null, null];
  await waitFor(
    async () => {
      seen = [await f.promSpend(replica), await f.promStarted(replica)];
      return seen[0] === value && seen[1] === started;
    },
    () => `第 ${replica} 个 Prometheus 抓到进程 ${started} 上的累计 ${value}（最后看到 ${seen}）`,
    ms,
  );
}

/**
 * 等门户扣到真实花费。**多扣是立刻失败**：扣款只进不退，多扣的那一刻就已经回不来
 * 了，接着等只会把它拖成一个超时。
 */
async function untilBilled(label: string): Promise<void> {
  const f = live();
  let consumed = -1;
  await waitFor(async () => {
    consumed = (await f.ledger()).consumed;
    if (consumed > f.spent()) {
      throw new Error(`${label}：门户扣了 ${consumed}，真实花费只有 ${f.spent()} —— 多扣了`);
    }
    return consumed === f.spent();
  }, () => `${label}：门户扣到真实花费 ${f.spent()}（最后扣到 ${consumed}）`);
}

/** 对账环再跑完 `n` 轮。 */
async function moreTicks(n: number): Promise<void> {
  const f = live();
  const t = await f.portalTicks();
  await waitFor(async () => (await f.portalTicks()) >= t + n, `对账环再跑 ${n} 轮`);
}

/**
 * 账已经停稳：Prometheus 早就看到了最终读数，对账环之后又跑了几轮，扣款仍然
 * 一分不差 —— 余额与消费流水两处都对得上。
 */
async function assertSettled(): Promise<void> {
  await moreTicks(3);
  const f = live();
  const l = await f.ledger();
  expect(l.consumed, "消费流水之和").toBe(f.spent());
  expect(l.balance, "余额").toBe(GRANT - f.spent());
}

test("稳定计费_抓取间隔不短于对账周期_每次调用都分文不差地扣到", async () => {
  fx = await startRealPromMoney([SCRAPE]);
  const started = await fx.gatewayStarted();
  // 分几批打，每批之间让 Prometheus 抓到、让门户扣到 —— 扣款跨过多次抓取、多轮
  // 对账累计起来，而不是一次读到一个总数。
  for (const n of [1, 2, 3]) {
    await calls(n);
    await untilPromSees(0, fx.spent(), started);
    await untilBilled(`又打了 ${n} 次之后`);
  }
  expect(fx.spent()).toBe(7 * PRICE_PER_CALL);
  await assertSettled();
});

test("网关重启_重启前后的花费都分文不差地扣到", async () => {
  fx = await startRealPromMoney([SCRAPE]);
  const before = await fx.gatewayStarted();
  await calls(3);
  await untilPromSees(0, fx.spent(), before);
  // 先等重启前的花费入账，再重启。这条量的是「重启前已入账的不重扣、重启后的
  // 不漏扣」，不是重启那一瞬间还没被读到的那一段。
  await untilBilled("重启前");
  const beforeRestart = fx.spent();

  await fx.restartGateway();
  const after = await fx.gatewayStarted();
  expect(after, "重启后是一个新进程").toBeGreaterThan(before);
  // 新进程只花 $1，比旧进程的累计小：计数器在 Prometheus 眼里是变小了。
  await calls(1);
  await untilPromSees(0, fx.spent() - beforeRestart, after);
  expect(await fx.promResets(0), "前提：Prometheus 看见了这次重置").toBeGreaterThanOrEqual(1);

  await untilBilled("重启后");
  await assertSettled();
});

test("重启后首次抓取前就花得比重启前多_resets_看不见_也分文不差", async () => {
  fx = await startRealPromMoney([SCRAPE]);
  // 旧进程只花了夹具打通密钥的那一次（$1）。新进程的花费序列在它的第一次调用之后
  // 才出现，所以 Prometheus 看到的新进程读数最小就是 $1 —— 不管抓取落在哪一刻，
  // 这条序列在它眼里都从没变小过，`resets()` 无从看见这次重启。
  expect(fx.spent(), "前提：旧进程只花了一次调用的钱").toBe(PRICE_PER_CALL);
  const before = await fx.gatewayStarted();
  await untilPromSees(0, fx.spent(), before);
  await untilBilled("重启前");
  const beforeRestart = fx.spent();

  await fx.restartGateway();
  const after = await fx.gatewayStarted();
  expect(after, "重启后是一个新进程").toBeGreaterThan(before);
  await calls(3);
  await untilPromSees(0, fx.spent() - beforeRestart, after);
  expect(await fx.promResets(0), "前提：Prometheus 从没见过这条序列变小").toBe(0);

  // 旧进程 $1 + 新进程 $3。只看「涨了多少」的话只会扣到 $1 + ($3 − $1)。
  await untilBilled("重启后");
  expect(fx.spent()).toBe(4 * PRICE_PER_CALL);
  await assertSettled();
});

test("高可用的落后副本跨过一次重启_总扣款等于真实花费_一分不多", async () => {
  // 两个 Prometheus 抓同一个网关，门户的查询在两者之间轮流。慢的那个每 30 秒才抓
  // 一次，重启之后有一整段时间它报的还是旧进程。
  const [FAST, SLOW] = [0, 1];
  fx = await startRealPromMoney([SCRAPE, "30s"]);
  const f = fx;
  const before = await f.gatewayStarted();
  await calls(2);
  await untilPromSees(FAST, f.spent(), before);
  await untilBilled("重启前");

  // 等慢副本**新**抓一次再重启：它的下一次抓取在 30 秒之后，落后窗口才确定存在。
  // 重启赶在它刚要抓之前的话，它几乎立刻就追上了，这条用例量不到任何东西。
  const t0 = Date.now();
  await waitFor(async () => (await f.lastScrape(SLOW)) > t0, "慢副本完成一次新的抓取", 45_000);
  await untilPromSees(SLOW, f.spent(), before);
  const beforeRestart = f.spent();

  await f.restartGateway();
  const after = await f.gatewayStarted();
  expect(after, "重启后是一个新进程").toBeGreaterThan(before);
  await calls(2);
  await untilPromSees(FAST, f.spent() - beforeRestart, after);
  await untilBilled("重启后，快副本已抓到新进程");

  // 落后窗口：快副本报新进程，慢副本还报旧进程（更早的启动时刻、旧的累计）。
  // 门户在这段时间里要从慢副本读到至少两次旧视图 —— 每一次都不能被当成又一次
  // 重启而把旧进程的累计再扣一遍。
  expect(await f.promStarted(SLOW), "前提：慢副本还停在重启前的进程上").toBe(before);
  const lagFrom = Date.now();
  await waitFor(async () => f.readsSince(SLOW, lagFrom) >= 2, "门户从慢副本读到两次");
  await moreTicks(1);
  expect(
    await f.promStarted(SLOW),
    "前提：门户那两次读慢副本时，它一直停在重启前的进程上",
  ).toBe(before);
  await untilBilled("门户读过落后副本的旧视图之后");

  // 慢副本追上之后，两边各再读几次：账仍然停在真实花费上。
  await untilPromSees(SLOW, f.spent() - beforeRestart, after, 45_000);
  const caughtUp = Date.now();
  await waitFor(
    async () => f.readsSince(FAST, caughtUp) >= 2 && f.readsSince(SLOW, caughtUp) >= 2,
    "慢副本追上之后门户从两边各读到两次",
  );
  await assertSettled();
});
