// 两端共用同一时间口径；保存和传输 UTC，界面及自然日统计使用北京时间。
export type CalendarGroup = "day" | "month";
export type TimeBucket = "minute" | "hour" | "day";

const BEIJING_OFFSET_MS = 8 * 60 * 60 * 1000;
const DAY_MS = 24 * 60 * 60 * 1000;
const clock = new Intl.DateTimeFormat("en-CA", {
  timeZone: "Asia/Shanghai",
  year: "numeric",
  month: "2-digit",
  day: "2-digit",
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
  hourCycle: "h23",
});

/** 供 datetime-local 使用的北京墙上时间，不借用浏览器本地时区。 */
export function beijingInputValue(at: Date): string {
  const parts = clock.formatToParts(at);
  const part = (type: Intl.DateTimeFormatPartTypes) => parts.find(value => value.type === type)?.value ?? "";
  return `${part("year").padStart(4, "0")}-${part("month")}-${part("day")}T${part("hour")}:${part("minute")}:${part("second")}`;
}

/** 展示时标出时区；无法解析的值仍按原样展示。 */
export function fmtBeijing(iso: string): string {
  const at = new Date(iso);
  return Number.isNaN(at.getTime()) ? iso : `${beijingInputValue(at).replace("T", " ")} 北京时间`;
}

export function todayBeijing(now: Date = new Date()): string {
  return beijingInputValue(now).slice(0, 10);
}

/** 输入没有 offset，明确按东八区读取；日期或时分秒非法时不自动进位。 */
export function parseBeijingInput(value: string): string | null {
  const match = /^(\d{4}-\d{2}-\d{2})[T ](\d{2}:\d{2})(?::(\d{2}))?$/.exec(value.trim());
  if (!match) return null;
  const typed = `${match[1]}T${match[2]}:${match[3] ?? "00"}`;
  const at = new Date(`${typed}+08:00`);
  if (Number.isNaN(at.getTime())) return null;
  return beijingInputValue(at) === typed ? at.toISOString() : null;
}

/** 首尾日期都包含，传给 API 的边界仍是 UTC instant，范围左闭右开。 */
export function beijingRange(fromDate: string, toDate: string): { from: string; to: string } {
  const next = new Date(`${toDate}T00:00:00Z`);
  next.setUTCDate(next.getUTCDate() + 1);
  return {
    from: new Date(`${fromDate}T00:00:00+08:00`).toISOString(),
    to: new Date(`${next.toISOString().slice(0, 10)}T00:00:00+08:00`).toISOString(),
  };
}

/** 最近三十个北京自然日，或包含本月在内的十二个北京自然月。 */
export function defaultCalendarRange(group: CalendarGroup, now: Date = new Date()): { from: string; to: string } {
  const today = todayBeijing(now);
  const start = new Date(`${today}T00:00:00Z`);
  if (group === "day") start.setUTCDate(start.getUTCDate() - 29);
  else start.setUTCMonth(start.getUTCMonth() - 11, 1);
  return beijingRange(start.toISOString().slice(0, 10), today);
}

export function periodLabel(start: string, group: CalendarGroup): string {
  const at = new Date(start);
  const day = Number.isNaN(at.getTime()) ? start.slice(0, 10) : todayBeijing(at);
  return group === "day" ? day : day.slice(0, 7);
}

export function metricPeriodLabel(start: string, bucket: TimeBucket): string {
  const at = new Date(start);
  if (Number.isNaN(at.getTime())) return start;
  const local = beijingInputValue(at);
  return bucket === "day" ? local.slice(0, 10) : local.slice(5, 16).replace("T", " ");
}

/** 与服务端北京时间分桶一致；小时/分钟的 epoch 边界恰好不变。 */
export function bucketStart(at: string, bucket: TimeBucket): number {
  const duration = bucket === "minute" ? 60_000 : bucket === "hour" ? 3_600_000 : DAY_MS;
  const offset = bucket === "day" ? BEIJING_OFFSET_MS : 0;
  return Math.floor((Date.parse(at) + offset) / duration) * duration - offset;
}

/** 预算仍按原 UTC 日历结算，只把后端真实周期起止换成北京时间展示。 */
export function budgetPeriodRange(start: string, period: "day" | "week" | "month"): string {
  const end = new Date(start);
  if (Number.isNaN(end.getTime())) return start;
  if (period === "month") end.setUTCMonth(end.getUTCMonth() + 1);
  else end.setUTCDate(end.getUTCDate() + (period === "week" ? 7 : 1));
  return `${fmtBeijing(start)} 至 ${fmtBeijing(end.toISOString())}（不含结束时刻）`;
}
