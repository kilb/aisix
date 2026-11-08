/**
 * A link the Dashboard may follow, or null. Only `http:` and `https:` pass:
 * every other scheme (a `javascript:` URL runs script when clicked, a `data:`
 * one can render a document of its own) is refused, and so is text that is not
 * an absolute URL at all. A caller renders the raw text unlinked instead.
 */
export function safeHttpUrl(value: string | null | undefined): string | null {
  if (!value) return null;
  try {
    const url = new URL(value);
    return url.protocol === "http:" || url.protocol === "https:" ? url.href : null;
  } catch {
    return null;
  }
}
