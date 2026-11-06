//! Platform mode: the settings a multi-tenant deployment adds to the gateway.

use sha2::{Digest, Sha256};

/// Settings read once at startup when `proxy.platform.enabled` is true.
#[derive(Clone)]
pub struct PlatformSettings {
    /// Key derived from the platform secret: SHA-256 of its bytes.
    pub id_key: [u8; 32],
}

impl PlatformSettings {
    /// 由平台密钥派生 id_key（SHA-256）。
    pub fn from_secret(secret: &str) -> PlatformSettings {
        PlatformSettings {
            id_key: Sha256::digest(secret.as_bytes()).into(),
        }
    }
}

/// The tenant a platform-mode request belongs to: its API key's team, or the
/// key itself when it has none. Every id platform mode seals for a caller is
/// sealed to it, and opens only for it.
pub(crate) fn tenant_owner(auth: &crate::auth::AuthenticatedKey) -> &str {
    auth.key().team_id.as_deref().unwrap_or(&auth.entry.id)
}

// 不派生 Debug：id_key 是密钥派生值，不能进日志。
impl std::fmt::Debug for PlatformSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformSettings")
            .field("id_key", &"***")
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
        let shown = format!("{:?}", PlatformSettings::from_secret("abc"));
        assert!(!shown.contains("186"), "{shown}");
        assert!(shown.contains("***"));
    }
}
