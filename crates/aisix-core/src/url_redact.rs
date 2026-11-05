//! Keeping a credential embedded in a configured URL out of what is displayed.
//!
//! A configured URL may carry `user:pass@host`, and it is used as configured.
//! It is never shown that way: logs, `Debug` output and error text go through
//! one of the two functions here. They live in this crate because the crates
//! below the gateway (`aisix-redis`, `aisix-etcd`, the startup config) display
//! configured URLs too.

/// A configured URL reduced to `scheme://host[:port]/` for a log line or a
/// `Debug` rendering: userinfo, path, query, and fragment are all dropped,
/// since any of them can carry a credential (`user:pass@`, a tenant-secret
/// path segment, `?token=`). Nothing past the origin is kept — the value
/// identifies which upstream it is, and is never needed to reach it.
pub fn redact_url_for_log(raw: &str) -> String {
    let Ok(mut url) = url::Url::parse(raw) else {
        return "<invalid URL>".to_string();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

/// Replace any `userinfo@` prefix in an authority with `***@` before the value
/// goes into a message.
///
/// For a value being refused, which is often not a URL at all:
/// [`redact_url_for_log`] can only say `<invalid URL>` about it, while an
/// operator has to see what they wrote to fix it. A value holding no `@` comes
/// back unchanged.
///
/// THE implementation, for the same reason `cidr_allows` is: a redaction
/// copied per provider drifts. Vertex grew its own and applied it to one of
/// its four echo branches; Azure grew none and printed the whole value in the
/// branch that says the value embeds userinfo. This one also covers the
/// scheme-less shape — Vertex's returned the input untouched without a
/// `://`, which is precisely the case that reaches a no-scheme branch.
pub fn redact_userinfo(value: &str) -> std::borrow::Cow<'_, str> {
    // 只有出现在任何 `/` `?` `#` `@` 之前的 `://` 才是 scheme 分隔符；query 里的
    // `https://` 不算，否则 authority 会被认到它后面，前面的 userinfo 原样放过。
    let after_scheme = match value.find("://") {
        Some(i) if !value[..i].contains(['/', '?', '#', '@']) => i + 3,
        _ => 0,
    };
    // The last `@` anywhere after the scheme: a password may itself hold a
    // raw `/`, `?` or `#`, and ending the authority at the first of those
    // would leave the rest of the password in the message. A path or query
    // holding an `@` is over-redacted, which is harmless in a message.
    match value[after_scheme..].rfind('@') {
        Some(at) => std::borrow::Cow::Owned(format!(
            "{}***@{}",
            &value[..after_scheme],
            &value[after_scheme + at + 1..]
        )),
        None => std::borrow::Cow::Borrowed(value),
    }
}
