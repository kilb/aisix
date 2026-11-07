// fmtInt and maskUrl are copied from web/src/lib/fmt.ts; comments translated.

import { ApiError } from "./api";
import { messageFor } from "./errors";

/** Display formatting. */

export function fmtInt(n: unknown): string {
  return (Number(n) || 0).toLocaleString("en-US");
}

/** An address for people to read: the password in its userinfo becomes `***`. */
export function maskUrl(v: unknown): string {
  return String(v ?? "").replace(/\/\/([^:@/]*):[^@/]*@/, "//$1:***@");
}

/** A failure as the operator reads it: the Dashboard's text for an API error, anything else as it is. */
export function errorText(e: unknown): string {
  return e instanceof ApiError ? messageFor(e) : String(e);
}

/** A timestamp in local time, on a 24-hour clock. */
export function fmtTime(iso: string): string {
  return new Date(iso).toLocaleString("zh-CN", { hour12: false });
}
