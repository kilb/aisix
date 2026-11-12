import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { beijingInputValue, beijingRange, bucketStart, budgetPeriodRange, defaultCalendarRange, fmtBeijing, metricPeriodLabel, parseBeijingInput, periodLabel, todayBeijing } from "./time.ts";

test("北京时间日期区间含首尾两天，API 保存 UTC instant", () => {
  assert.deepEqual(beijingRange("2026-11-01", "2026-11-07"), { from: "2026-10-31T16:00:00.000Z", to: "2026-11-07T16:00:00.000Z" });
  assert.equal(todayBeijing(new Date("2026-11-08T07:30:00+08:00")), "2026-11-08");
  assert.equal(todayBeijing(new Date("2026-11-07T15:59:59.999Z")), "2026-11-07");
  assert.equal(todayBeijing(new Date("2026-11-07T16:00:00Z")), "2026-11-08");
  assert.equal(beijingRange("2026-11-30", "2026-11-30").to, "2026-11-30T16:00:00.000Z");
  assert.equal(beijingRange("2026-12-31", "2026-12-31").to, "2026-12-31T16:00:00.000Z");
  assert.equal(beijingRange("2028-02-29", "2028-02-29").to, "2028-02-29T16:00:00.000Z");
});

test("时间和周期标签使用北京日历，跨月跨年不依赖 UTC 日期前缀", () => {
  assert.equal(fmtBeijing("2026-11-07T13:05:09.987654+00:00"), "2026-11-07 21:05:09");
  assert.equal(fmtBeijing("2026-11-08T01:05:09+08:00"), "2026-11-08 01:05:09");
  assert.equal(fmtBeijing("not-a-time"), "not-a-time");
  assert.equal(periodLabel("2026-12-31T16:00:00Z", "day"), "2027-01-01");
  assert.equal(periodLabel("2026-12-31T16:00:00Z", "month"), "2027-01");
  assert.equal(metricPeriodLabel("2026-12-31T16:05:00Z", "minute"), "01-01 00:05");
  assert.equal(metricPeriodLabel("2026-12-31T16:00:00Z", "day"), "2027-01-01");
});

test("北京墙上时间转成 UTC，真实午夜不格式化为24点", () => {
  assert.equal(beijingInputValue(new Date("2026-11-07T16:00:00Z")), "2026-11-08T00:00:00");
  assert.equal(parseBeijingInput("2026-11-08T00:00"), "2026-11-07T16:00:00.000Z");
  assert.equal(parseBeijingInput("2030-01-02 03:04:05"), "2030-01-01T19:04:05.000Z");
  assert.equal(parseBeijingInput("2028-02-29 00:00"), "2028-02-28T16:00:00.000Z");
  for (const invalid of ["", "2026-11-07", "2027-02-29 00:00", "2027-04-31 10:00", "2027-01-01 24:00", "2027-01-01 23:60", "2026-11-07T08:30Z", "2026-11-07T08:30+08:00"]) {
    assert.equal(parseBeijingInput(invalid), null, invalid);
  }
});

test("默认窗口是北京最近30自然日或12自然月，凌晨跨年使用新一年", () => {
  const now = new Date("2026-12-31T16:01:00Z");
  assert.deepEqual(defaultCalendarRange("day", now), { from: "2026-12-02T16:00:00.000Z", to: "2027-01-01T16:00:00.000Z" });
  assert.deepEqual(defaultCalendarRange("month", now), { from: "2026-01-31T16:00:00.000Z", to: "2027-01-01T16:00:00.000Z" });
});

test("北京日桶在16Z跨周月年闰日，细粒度边界保持绝对时刻", () => {
  for (const boundary of ["2026-11-08T16:00:00Z", "2026-12-31T16:00:00Z", "2028-02-28T16:00:00Z"]) {
    const epoch = Date.parse(boundary);
    assert.equal(bucketStart(boundary, "day"), epoch);
    assert.equal(bucketStart(new Date(epoch - 1).toISOString(), "day"), epoch - 86_400_000);
    assert.equal(bucketStart(boundary, "hour"), epoch);
    assert.equal(bucketStart(boundary, "minute"), epoch);
  }
});

test("预算真实UTC结算周期的北京起止仍为08点，不改成北京午夜", () => {
  assert.equal(budgetPeriodRange("2026-12-01T00:00:00Z", "month"), "2026-12-01 08:00:00 至 2027-01-01 08:00:00（不含结束时刻）");
  assert.equal(budgetPeriodRange("2026-11-09T00:00:00Z", "week"), "2026-11-09 08:00:00 至 2026-11-16 08:00:00（不含结束时刻）");
  assert.equal(budgetPeriodRange("2028-02-29T00:00:00Z", "day"), "2028-02-29 08:00:00 至 2028-03-01 08:00:00（不含结束时刻）");
});

test("显式Asia/Shanghai在不同宿主时区输出一致，去掉timeZone会失败", () => {
  const code = `import {fmtBeijing,parseBeijingInput} from ${JSON.stringify(new URL("./time.ts", import.meta.url).href)}; process.stdout.write(JSON.stringify([fmtBeijing('2026-12-31T16:00:00Z'),parseBeijingInput('2027-01-01 00:00')]));`;
  for (const zone of ["America/Los_Angeles", "Pacific/Kiritimati"]) {
    const result = spawnSync(process.execPath, ["--experimental-strip-types", "--input-type=module", "-e", code], { env: { ...process.env, TZ: zone }, encoding: "utf8" });
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(JSON.parse(result.stdout), ["2027-01-01 00:00:00", "2026-12-31T16:00:00.000Z"], zone);
  }
});

test("门户与运维台时间工具逐字节一致", () => {
  assert.deepEqual(readFileSync(new URL("./time.ts", import.meta.url)), readFileSync(new URL("../../../web-cp/src/lib/time.ts", import.meta.url)));
});
