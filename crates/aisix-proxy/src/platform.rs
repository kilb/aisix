//! Platform mode: the settings a multi-tenant deployment adds to the gateway.

use std::sync::Arc;

use ring::{aead, hmac};
use sha2::{Digest, Sha256};

/// Settings read once at startup when `proxy.platform.enabled` is true.
///
/// Every key an id is mapped or sealed under is derived here, once, from
/// [`Self::id_key`], and shared by every request: setting a key up costs
/// more than the one id most requests map or seal with it.
#[derive(Clone)]
pub struct PlatformSettings {
    /// Key derived from the platform secret: SHA-256 of its bytes.
    pub id_key: [u8; 32],
    /// The HMAC key tool-call ids are mapped under
    /// ([`crate::opaque_id::map_tool_call_id`]).
    tool_id_key: hmac::Key,
    /// The keys Responses item and call ids are sealed under
    /// ([`crate::opaque_id::Sealer`]).
    seal_keys: Arc<crate::opaque_id::SealKeys>,
    /// The cipher Responses response ids and ciphertext tags are sealed
    /// under ([`crate::responses_affinity::TenantSeal`]).
    response_cipher: Arc<aead::LessSafeKey>,
}

impl PlatformSettings {
    /// 由平台密钥派生 id_key（SHA-256），并一次派生出各个映射、封装所用的密钥。
    pub fn from_secret(secret: &str) -> PlatformSettings {
        let id_key: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
        PlatformSettings {
            tool_id_key: hmac::Key::new(hmac::HMAC_SHA256, &id_key),
            seal_keys: Arc::new(crate::opaque_id::SealKeys::new(
                &crate::opaque_id::seal_key(&id_key),
            )),
            response_cipher: Arc::new(crate::responses_affinity::cipher(
                &crate::responses_affinity::response_id_key(&id_key),
            )),
            id_key,
        }
    }

    pub(crate) fn tool_id_key(&self) -> &hmac::Key {
        &self.tool_id_key
    }

    pub(crate) fn seal_keys(&self) -> &Arc<crate::opaque_id::SealKeys> {
        &self.seal_keys
    }

    pub(crate) fn response_cipher(&self) -> &Arc<aead::LessSafeKey> {
        &self.response_cipher
    }
}

/// The content type of a server-sent event stream.
pub(crate) const SSE_CONTENT_TYPE: &str = "text/event-stream";
/// The content type of a JSON body.
pub(crate) const JSON_CONTENT_TYPE: &str = "application/json";

/// The `content-type` an answer relayed from an upstream goes out under:
/// the upstream's own, as it wrote it, or, in platform mode, `fixed` (one of
/// [`SSE_CONTENT_TYPE`] and [`JSON_CONTENT_TYPE`]). How an upstream spells
/// its content type, a charset parameter or its case, differs by channel, so
/// in platform mode every answer of one kind goes out under one value.
/// `None` when the upstream sent none outside platform mode.
pub(crate) fn relayed_content_type(
    platform: Option<&PlatformSettings>,
    upstream: &axum::http::HeaderMap,
    fixed: &'static str,
) -> Option<axum::http::HeaderValue> {
    if platform.is_some() {
        return Some(axum::http::HeaderValue::from_static(fixed));
    }
    upstream
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| axum::http::HeaderValue::from_bytes(v.as_bytes()).ok())
}

/// The tenant a platform-mode request belongs to: `team:<team id>` for a key
/// in a team, `key:<key entry id>` for one without. Every id platform mode
/// seals for a caller is sealed to it, and opens only for it. The kind tag
/// keeps a team id that equals some key's entry id from naming that key's
/// tenant.
pub(crate) fn tenant_owner(auth: &crate::auth::AuthenticatedKey) -> String {
    match auth.key().team_id.as_deref() {
        Some(team) => format!("team:{team}"),
        None => format!("key:{}", auth.entry.id),
    }
}

// 不派生 Debug：id_key 和由它派生的各个密钥都不能进日志。
impl std::fmt::Debug for PlatformSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformSettings")
            .field("id_key", &"***")
            .field("derived_keys", &"***")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_secret_is_the_sha256_of_the_secret() {
        // SHA-256("abc")
        let expected = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        assert_eq!(PlatformSettings::from_secret("abc").id_key, expected);
    }

    #[test]
    fn debug_output_hides_the_key() {
        let settings = PlatformSettings::from_secret("abc");
        let shown = format!("{settings:?}");
        assert_eq!(
            shown,
            r#"PlatformSettings { id_key: "***", derived_keys: "***" }"#
        );
        assert!(!shown.contains(&format!("{:?}", settings.id_key)));
    }

    /// Every key is set up once, when the settings are built, and never
    /// again for a request.
    #[test]
    fn the_derived_keys_are_set_up_once() {
        let built = || crate::opaque_id::tests::SEAL_KEYS_BUILT.with(std::cell::Cell::get);
        let before = built();
        let settings = PlatformSettings::from_secret("abc");
        assert_eq!(built() - before, 1);
        for owner in ["team:a", "team:b", "key:c"] {
            let sealer = crate::opaque_id::Sealer::of(&settings, owner);
            let sealed = sealer.seal(crate::opaque_id::IdKind::Item, "fc_1");
            assert_eq!(
                sealer
                    .unseal(crate::opaque_id::IdKind::Item, &sealed)
                    .as_deref(),
                Some("fc_1")
            );
        }
        assert_eq!(built() - before, 1, "no key is set up per sealer");
        // The keys are the ones derived from the id key, under their own labels.
        assert_eq!(
            crate::opaque_id::seal_id(
                &crate::opaque_id::seal_key(&settings.id_key),
                "team:a",
                "fc_1"
            ),
            crate::opaque_id::Sealer::of(&settings, "team:a")
                .seal(crate::opaque_id::IdKind::Item, "fc_1")
        );
    }
}
