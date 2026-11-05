//! Guard that every provider bridge validates `ProviderKey.api_base`.
//!
//! `api_base` decides where a ProviderKey's secret is sent, and the bridge
//! appends a fixed path to it, so a query or fragment in the base would swallow
//! that path. #390 added the check to the Vertex and Azure bridges and stopped
//! there — the OpenAI-compatible and Anthropic bridges carried the gap silently
//! until the 2026-08-19 pass, because nothing fails when a sibling is missed.
//!
//! Userinfo is no longer part of this guard: the project owner decided a URL
//! with `user:pass@` is accepted and used as configured, and its password is
//! kept out of everything displayed.
//!
//! This is a source scan over the bridge crates rather than a behavioural
//! test: a behavioural one would need a live `BridgeContext` per provider, and
//! what actually recurs is a NEW bridge landing without the check at all.

use std::path::{Path, PathBuf};

/// Bridges that resolve an operator-supplied base URL and send a credential
/// to it. Bedrock is absent on purpose: it resolves AWS endpoints through the
/// SDK rather than composing a base itself.
const BRIDGES_RESOLVING_A_BASE: &[(&str, &str)] = &[
    ("aisix-provider-openai", "src/bridge.rs"),
    ("aisix-provider-anthropic", "src/bridge.rs"),
    ("aisix-provider-vertex", "src/bridge.rs"),
    ("aisix-provider-azure-openai", "src/bridge.rs"),
];

/// Evidence that a bridge checks the shape. Either it calls the shared
/// validator, or it carries its own query rejection (Vertex and Azure predate
/// the shared one and keep their provider-specific messages).
const ACCEPTED_EVIDENCE: &[&str] = &["validate_api_base", "contains('?')"];

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/<name> has a parent")
        .to_path_buf()
}

#[test]
fn every_base_resolving_bridge_validates_its_api_base() {
    let root = crates_dir();
    let mut unguarded = Vec::new();

    for (krate, file) in BRIDGES_RESOLVING_A_BASE {
        let path = root.join(krate).join(file);
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} is readable: {e}", path.display()));
        if !ACCEPTED_EVIDENCE.iter().any(|marker| src.contains(marker)) {
            unguarded.push(format!("  {krate}/{file}"));
        }
    }

    assert!(
        unguarded.is_empty(),
        "these bridges resolve an operator-supplied api_base without validating its \
         shape, so a query or fragment in the base would swallow the path the bridge \
         appends:\n{}\n\nCall `aisix_gateway::validate_api_base` from the base \
         resolution.",
        unguarded.join("\n"),
    );
}

#[test]
fn the_bridge_list_matches_what_is_on_disk() {
    let root = crates_dir();
    for (krate, file) in BRIDGES_RESOLVING_A_BASE {
        assert!(
            root.join(krate).join(file).exists(),
            "BRIDGES_RESOLVING_A_BASE lists {krate}/{file}, which no longer exists"
        );
    }
}
