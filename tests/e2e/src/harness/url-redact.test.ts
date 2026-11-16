import { expect, test } from "vitest";
import { redactUrlForLog } from "./url-redact.js";

test.each([
  ["redis://user:password-canary@localhost:6377/9?token=query-canary#fragment-canary", "redis://localhost:6377/"],
  ["redis://user:p%40ssword-canary@[::1]:6377/0", "redis://[::1]:6377/"],
  ["redis://user:part@password-canary@localhost:6377", "redis://localhost:6377/"],
  ["https://user:password-canary@example.com/a", "https://example.com/"],
  ["not-a-url-password-canary", "<invalid URL>"],
  ["redis:password-canary", "<invalid URL>"],
])("configured URL display removes credentials from %s", (raw, shown) => {
  expect(redactUrlForLog(raw)).toBe(shown);
});
