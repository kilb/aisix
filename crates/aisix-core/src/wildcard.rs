//! `*` wildcard matching, shared by the access lists (API-key
//! `allowed_models` / `allowed_tools` / `allowed_agents` / `allowed_routes`,
//! MCP policy allow and deny, forwarded client headers) and the proxy's
//! wildcard model resolution (`provider/*` aliases).
//!
//! [`wildcard_matches`] accepts any number of `*`, each matching any run of
//! characters (including none). [`wildcard_capture`] binds the text a `*`
//! stood for, which only has one answer with a single `*`, so it supports
//! exactly one. Neither pulls a regex engine into the hot path. The literal
//! text before the first `*` and after the last anchors the candidate at both
//! ends, so a pattern matches the *whole* name.

/// Match `candidate` against a `*`-glob `pattern` that contains **exactly one**
/// `*`, returning the substring bound to the `*` on success.
///
/// `openai/*` matches `openai/gpt-4o` (capture `gpt-4o`) but not
/// `azure/openai/x` (the prefix must anchor the start). A pattern with no `*`,
/// or with more than one `*`, is unsupported and returns `None` — callers treat
/// those as plain literals.
pub fn wildcard_capture(pattern: &str, candidate: &str) -> Option<String> {
    let star = pattern.find('*')?;
    if pattern[star + 1..].contains('*') {
        return None; // only a single '*' is supported
    }
    let prefix = &pattern[..star];
    let suffix = &pattern[star + 1..];
    if candidate.len() < prefix.len() + suffix.len() {
        return None; // prefix and suffix would overlap
    }
    if candidate.starts_with(prefix) && candidate.ends_with(suffix) {
        Some(candidate[prefix.len()..candidate.len() - suffix.len()].to_string())
    } else {
        None
    }
}

/// Whether `pattern` matches `candidate`. Each `*` matches any run of
/// characters, including none; a pattern without a `*` matches only an
/// exactly-equal candidate.
///
/// Every `*` must be honoured: these patterns guard deny lists too, and a
/// pattern that silently matched nothing there would let through exactly
/// what it was written to block (`"*delete*"`).
///
/// For a single `*` this is the same decision as
/// `wildcard_capture(..).is_some()`, but it runs on the per-request authz
/// path and never allocates.
pub fn wildcard_matches(pattern: &str, candidate: &str) -> bool {
    let Some((head, tail)) = pattern.split_once('*') else {
        return pattern == candidate;
    };
    let (middle, last) = tail.rsplit_once('*').unwrap_or(("", tail));
    // Strip the leading literal, then the trailing one from what is left,
    // so the two can never overlap.
    let Some(rest) = candidate.strip_prefix(head) else {
        return false;
    };
    let Some(mut rest) = rest.strip_suffix(last) else {
        return false;
    };
    // Take each inner literal at its leftmost occurrence, in order: with
    // `*` as the only wildcard, leftmost never rules out a match.
    for part in middle.split('*').filter(|p| !p.is_empty()) {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_star_captures_the_rest() {
        assert_eq!(
            wildcard_capture("openai/*", "openai/gpt-4o").as_deref(),
            Some("gpt-4o")
        );
        assert_eq!(
            wildcard_capture("openai/*", "openai/gpt-4o-mini").as_deref(),
            Some("gpt-4o-mini")
        );
    }

    #[test]
    fn prefix_must_anchor_the_start() {
        assert_eq!(wildcard_capture("openai/*", "azure/openai/x"), None);
        assert_eq!(wildcard_capture("openai/*", "opena"), None);
    }

    #[test]
    fn interior_star_binds_the_middle() {
        assert_eq!(
            wildcard_capture("gpt-*-preview", "gpt-4o-preview").as_deref(),
            Some("4o")
        );
        assert_eq!(wildcard_capture("gpt-*-preview", "gpt-4o-final"), None);
    }

    #[test]
    fn star_matches_empty_capture() {
        assert_eq!(wildcard_capture("openai/*", "openai/").as_deref(), Some(""));
    }

    #[test]
    fn multiple_stars_unsupported() {
        assert_eq!(wildcard_capture("a/*/*", "a/b/c"), None);
    }

    /// Differential oracle: for patterns with at most one `*`,
    /// `wildcard_matches` must agree with `wildcard_capture(..).is_some()` —
    /// the two are duplicate decision procedures guarding the authz
    /// allowlists, and a future edit to either must not let them diverge
    /// silently. Patterns with more `*` are checked against a regex below;
    /// `wildcard_capture` deliberately does not support them.
    #[test]
    fn matches_agrees_with_capture_oracle() {
        let pats = [
            "*",
            "openai/*",
            "*-sfx",
            "gpt-*-preview",
            "aa*aa",
            "lit",
            "",
        ];
        let cands = [
            "",
            "openai/",
            "openai/gpt-4o",
            "aaa",
            "aaaa",
            "gpt-4o-preview",
            "gpt-4o-final",
            "a/b/c",
            "-sfx",
            "lit",
        ];
        for p in pats {
            for c in cands {
                let expected = if p.contains('*') {
                    wildcard_capture(p, c).is_some()
                } else {
                    p == c
                };
                assert_eq!(wildcard_matches(p, c), expected, "p={p:?} c={c:?}");
            }
        }
    }

    /// Differential oracle for any number of `*`: the pattern compiled to an
    /// anchored regex where each `*` is `.*` and everything else is literal.
    #[test]
    fn matches_agrees_with_regex_oracle() {
        let pats = [
            "*", "**", "***", "a/*/*", "*delete*", "*__*cho", "a*b*c", "a*a*a", "*a*", "aa*aa",
            "*-*-*", "gpt-*-*", "x*", "*x", "lit", "", "*é*",
        ];
        let cands = [
            "",
            "a",
            "aa",
            "aaa",
            "aaaa",
            "abc",
            "aXbYc",
            "acb",
            "a/b/c",
            "a/b",
            "github__delete_repo",
            "delete",
            "deletx",
            "alpha__echo",
            "alpha__reverse",
            "gpt-4o-mini",
            "gpt-4o",
            "x",
            "café",
            "lit",
        ];
        for p in pats {
            let re = regex::Regex::new(&format!(
                "^(?s){}$",
                p.split('*')
                    .map(regex::escape)
                    .collect::<Vec<_>>()
                    .join(".*")
            ))
            .unwrap();
            for c in cands {
                assert_eq!(wildcard_matches(p, c), re.is_match(c), "p={p:?} c={c:?}");
            }
        }
    }

    #[test]
    fn several_stars_match_as_written() {
        assert!(wildcard_matches("*delete*", "github__delete_repo"));
        assert!(wildcard_matches("*delete*", "delete"));
        assert!(!wildcard_matches("*delete*", "github__create_repo"));
        // The literal runs must appear in order and must not overlap.
        assert!(wildcard_matches("a*b*c", "aXbYc"));
        assert!(!wildcard_matches("a*b*c", "acb"));
        assert!(!wildcard_matches("a*a*a", "aa"));
        assert!(wildcard_matches("a*a*a", "aaa"));
    }

    #[test]
    fn matches_handles_literals_and_globs() {
        assert!(wildcard_matches("*", "anything"));
        assert!(wildcard_matches("openai/*", "openai/gpt-4o"));
        assert!(wildcard_matches("gpt-4o", "gpt-4o"));
        assert!(!wildcard_matches("gpt-4o", "gpt-4o-mini"));
        assert!(!wildcard_matches("openai/*", "anthropic/claude"));
    }
}
