/**
 * Detail fields whose values are never shown. The control plane writes
 * field names and non-secret values only; this holds even for a row that
 * broke that rule. A name matches as a whole `_`-delimited word, so `keys`
 * and `fields` stay visible.
 */
export const SECRET_FIELD =
  /(^|_)(password|passphrase|secret|token|api_key|apikey|private_key|key_pem|hash|cookie|authorization)(_|$)/i;

/** What a hidden value reads as. */
export const HIDDEN = "（不显示）";

/** `value` with the value of every secret-named field, at any depth, replaced by {@link HIDDEN}. */
export function redacted(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(redacted);
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value).map(([k, v]) => [k, SECRET_FIELD.test(k) ? HIDDEN : redacted(v)]),
    );
  }
  return value;
}
