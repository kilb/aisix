/**
 * A link the Dashboard may follow, or null. Only `http:` and `https:` pass:
 * every other scheme (a `javascript:` URL runs script when clicked, a `data:`
 * one can render a document of its own) is refused, and so is text that is not
 * an absolute URL at all. So is one whose printed text names another host than
 * the one a click reaches: userinfo (`https://a.example@b.example/` goes to
 * `b.example`), a backslash, read as `/`, and a tab or line break, which the
 * parser drops. A caller renders the raw text unlinked instead.
 */
export function safeHttpUrl(value: string | null | undefined): string | null {
  if (!value || /[\\\t\n\r]/.test(value)) return null;
  try {
    const url = new URL(value);
    if (url.username !== "" || url.password !== "") return null;
    return url.protocol === "http:" || url.protocol === "https:" ? url.href : null;
  } catch {
    return null;
  }
}
