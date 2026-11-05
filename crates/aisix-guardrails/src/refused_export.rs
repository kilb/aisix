//! The copy of a refused request that a full-content export may carry.
//!
//! An auditor reads a refused request's body to see what was refused, and what
//! it was refused for must not enter the logstore: a blocked keyword, or an
//! identity number a `pii` rule blocks, in the clear would be the guardrail
//! writing down what it exists to keep out. So the export carries a copy with
//! every hit masked, the mask rules' and the block rules' alike, or no copy at
//! all when that cannot be proved:
//!
//! - a member cannot locate its hits ([`Guardrail::locates_input_hits`]): a
//!   remote verdict names no span, and a remote mask skipped because the
//!   request was already dead was never applied;
//! - a check failed open, so it read nothing;
//! - nothing could be located although something refused the request: whatever
//!   decided read a form of the text this walk does not see;
//! - masking left a hit behind, as a replacement can complete a match.
//!
//! Nothing here touches what the caller is told or what an upstream receives.

use serde_json::Value;

use crate::Guardrail;

/// `body`, in the representation the route captures, with every hit of the
/// chain's input rules masked; `None` when the row must carry no body (module
/// docs). A JSON body is walked as parsed, because an escape hides a match from
/// its text and keys and numbers are scanned too; anything else is masked as
/// plain text. Run this before any truncation, which can cut a match in half.
pub fn mask_refused_request(chain: &dyn Guardrail, body: &str) -> Option<String> {
    if !chain.locates_input_hits() {
        return None;
    }
    let mut pass = MaskPass {
        chain,
        located: false,
        left_behind: false,
    };
    let masked = match serde_json::from_str::<Value>(body) {
        Ok(mut doc) => {
            pass.value(&mut doc);
            doc.to_string()
        }
        Err(_) => pass.text(body),
    };
    (pass.located && !pass.left_behind).then_some(masked)
}

struct MaskPass<'a> {
    chain: &'a dyn Guardrail,
    located: bool,
    left_behind: bool,
}

impl MaskPass<'_> {
    fn text(&mut self, text: &str) -> String {
        let Some(masked) = self.chain.mask_input_hits(text) else {
            return text.to_owned();
        };
        self.located = true;
        if self
            .chain
            .mask_input_hits(&masked)
            .is_some_and(|again| again != masked)
        {
            self.left_behind = true;
        }
        masked
    }

    fn value(&mut self, value: &mut Value) {
        match value {
            Value::String(s) => {
                // A tool call's arguments are JSON inside a string, where an
                // escape hides a match from the text pass below.
                if let Ok(mut nested @ (Value::Object(_) | Value::Array(_))) =
                    serde_json::from_str::<Value>(s)
                {
                    let located_before = std::mem::replace(&mut self.located, false);
                    self.value(&mut nested);
                    if self.located {
                        *s = nested.to_string();
                    }
                    self.located |= located_before;
                }
                *s = self.text(s);
            }
            Value::Number(_) | Value::Bool(_) => {
                let text = value.to_string();
                let masked = self.text(&text);
                if masked != text {
                    *value = Value::String(masked);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| self.value(item)),
            Value::Object(map) => {
                for (key, mut item) in std::mem::take(map) {
                    self.value(&mut item);
                    map.insert(self.text(&key), item);
                }
            }
            Value::Null => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aisix_core::models::GuardrailHookPoint;
    use aisix_gateway::ChatFormat;
    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::{
        builtin_rule, GuardrailAuditLog, GuardrailChain, GuardrailVerdict, KeywordBlocklist,
        KeywordRule, PiiAction, PiiGuardrail,
    };

    const ID: &str = "11010519491231002X";

    fn keyword(word: &str) -> Arc<dyn Guardrail> {
        Arc::new(KeywordBlocklist::new(vec![KeywordRule::literal(word)]))
    }

    fn pii() -> Arc<dyn Guardrail> {
        Arc::new(PiiGuardrail::new(
            vec![
                builtin_rule("china_id_card", PiiAction::Block).unwrap(),
                builtin_rule("email", PiiAction::Mask).unwrap(),
            ],
            GuardrailHookPoint::Input,
            1024,
            false,
        ))
    }

    fn chain(members: Vec<Arc<dyn Guardrail>>) -> GuardrailChain {
        GuardrailChain::new(members)
    }

    fn masked_json(chain: &dyn Guardrail, body: serde_json::Value) -> serde_json::Value {
        let masked = mask_refused_request(chain, &body.to_string()).expect("a body to export");
        serde_json::from_str(&masked).unwrap()
    }

    #[test]
    fn block_and_mask_hits_go_and_the_rest_stays() {
        let c = chain(vec![keyword("forbidden"), pii()]);
        let out = masked_json(
            &c,
            json!({"messages": [{"role": "user", "content":
                format!("keep this FORBIDDEN text, write to a@b.co about {ID}")}]}),
        );
        assert_eq!(
            out["messages"][0]["content"],
            "keep this [KEYWORD_REDACTED] text, write to [EMAIL_REDACTED] about \
             [CHINA_ID_CARD_REDACTED]"
        );
        assert_eq!(out["messages"][0]["role"], "user");
    }

    /// The escape is how a caller writes it, not how the check read it.
    #[test]
    fn a_json_escape_does_not_hide_a_hit() {
        let c = chain(vec![keyword("forbidden")]);
        let masked = mask_refused_request(&c, r#"{"prompt":"keep forbidden"}"#).unwrap();
        assert_eq!(masked, r#"{"prompt":"keep [KEYWORD_REDACTED]"}"#);
    }

    #[test]
    fn json_inside_a_string_is_walked_as_json() {
        let c = chain(vec![keyword("forbidden")]);
        let out = masked_json(
            &c,
            json!({"arguments": r#"{"note":"keep forbidden","n":1}"#}),
        );
        let nested: serde_json::Value =
            serde_json::from_str(out["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(nested, json!({"note": "keep [KEYWORD_REDACTED]", "n": 1}));
    }

    #[test]
    fn keys_and_numbers_are_masked_too() {
        let c = chain(vec![keyword("forbidden"), keyword("4111")]);
        let out = masked_json(&c, json!({"forbidden-key": "kept", "card": 41110000}));
        assert_eq!(
            out,
            json!({"[KEYWORD_REDACTED]-key": "kept", "card": "[KEYWORD_REDACTED]0000"})
        );
    }

    #[test]
    fn a_body_that_is_not_json_is_masked_as_text() {
        let c = chain(vec![keyword("forbidden")]);
        assert_eq!(
            mask_refused_request(&c, "keep forbidden text").as_deref(),
            Some("keep [KEYWORD_REDACTED] text"),
        );
    }

    /// Lowercasing `İ` makes it longer, so a match past it must not land on
    /// the wrong bytes of the original.
    #[test]
    fn a_character_that_lowercases_longer_does_not_shift_the_match() {
        let c = chain(vec![keyword("forbidden")]);
        assert_eq!(
            mask_refused_request(&c, "İİ FORBIDDEN İ").as_deref(),
            Some("İİ [KEYWORD_REDACTED] İ"),
        );
        // The literal matches inside the expansion of a single character.
        let c = chain(vec![keyword("i")]);
        assert_eq!(
            mask_refused_request(&c, "aİb").as_deref(),
            Some("a[KEYWORD_REDACTED]b"),
        );
    }

    #[test]
    fn adjacent_and_regex_matches_are_each_masked() {
        let c = chain(vec![keyword("ab")]);
        assert_eq!(
            mask_refused_request(&c, "ababx").as_deref(),
            Some("[KEYWORD_REDACTED][KEYWORD_REDACTED]x"),
        );
        let re: Arc<dyn Guardrail> =
            Arc::new(KeywordBlocklist::new(vec![
                KeywordRule::regex(r"sec\w+$").unwrap()
            ]));
        // A `$` in the replacement text would be a group reference.
        assert_eq!(
            mask_refused_request(&chain(vec![re]), "a secret").as_deref(),
            Some("a [KEYWORD_REDACTED]"),
        );
    }

    /// What decided read a form of the text the walk cannot see.
    #[test]
    fn a_refusal_with_nothing_located_exports_nothing() {
        let c = chain(vec![keyword("forbidden")]);
        assert_eq!(mask_refused_request(&c, r#"{"prompt":"clean text"}"#), None);
        // Base64 of the word: what a decoding scan reads, this walk does not.
        assert_eq!(
            mask_refused_request(&c, r#"{"bytes":"Zm9yYmlkZGVu"}"#),
            None
        );
    }

    #[test]
    fn a_replacement_that_completes_a_match_exports_nothing() {
        // The second rule's token contains what the first one looks for.
        let c = chain(vec![keyword("redacted"), keyword("keep")]);
        assert_eq!(mask_refused_request(&c, "keep"), None);
    }

    /// A kind that reads the request and names no span: every remote one.
    struct Remote;

    #[async_trait]
    impl Guardrail for Remote {
        fn name(&self) -> &'static str {
            "remote"
        }

        async fn check_input(&self, _req: &ChatFormat) -> GuardrailVerdict {
            GuardrailVerdict::block("flagged")
        }
    }

    #[test]
    fn a_member_that_cannot_locate_its_hits_withholds_the_body() {
        let body = r#"{"prompt":"keep forbidden"}"#;
        assert_eq!(mask_refused_request(&Remote, body), None);
        // Beside a local rule too: its masks say nothing about what the remote
        // member refused, or would have masked had the request lived.
        let c = chain(vec![keyword("forbidden"), Arc::new(Remote)]);
        assert_eq!(mask_refused_request(&c, body), None);
    }

    #[test]
    fn a_failed_open_check_withholds_the_body() {
        let body = r#"{"prompt":"keep forbidden"}"#;
        let log = Arc::new(GuardrailAuditLog::default());
        let c = chain(vec![keyword("forbidden")]).with_audit_log(Some(log.clone()));
        assert!(mask_refused_request(&c, body).is_some());
        log.record_bypass("remote_5xx");
        assert_eq!(mask_refused_request(&c, body), None);
    }

    #[test]
    fn a_rule_on_the_output_hook_masks_nothing_here() {
        let output_only: Arc<dyn Guardrail> =
            Arc::new(KeywordBlocklist::output_only(vec![KeywordRule::literal(
                "answer",
            )]));
        let c = chain(vec![output_only, keyword("forbidden")]);
        assert_eq!(
            mask_refused_request(&c, "answer forbidden").as_deref(),
            Some("answer [KEYWORD_REDACTED]"),
        );
    }
}
