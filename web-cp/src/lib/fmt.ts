// Copied from web/src/lib/fmt.ts (fmtInt and maskUrl only); comments translated.

/** Display formatting. */

export function fmtInt(n: unknown): string {
  return (Number(n) || 0).toLocaleString("en-US");
}

/** An address for people to read: the password in its userinfo becomes `***`. */
export function maskUrl(v: unknown): string {
  return String(v ?? "").replace(/\/\/([^:@/]*):[^@/]*@/, "//$1:***@");
}
