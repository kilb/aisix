//! The master key, and sealing values at rest with it.
//!
//! A sealed value is `nonce(12) || ciphertext || tag(16)` under AES-256-GCM.
//! Every seal draws a fresh random nonce. Random 96-bit nonces stay within
//! the collision bound for far more seals than the control plane makes (it
//! seals CA keys and credentials, not traffic). The additional authenticated
//! data (`aad`) binds a sealed value to the slot it was sealed for, so a
//! value copied into another slot does not open.

use std::fmt;
use std::io::Read;
use std::path::Path;

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};

/// The key's file holds 64 hex characters, optionally followed by one newline.
const HEX_LEN: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// The file could not be read. Carries the OS error, never the path or
    /// the contents.
    #[error("cannot read the master key file: {0}")]
    Read(#[source] std::io::Error),
    /// Deliberately carries nothing from the file: a near-miss key is still
    /// most of a key.
    #[error("the master key file must hold exactly 64 hexadecimal characters, optionally followed by one newline")]
    Format,
    /// The wrong key, the wrong `aad`, or a damaged value: AES-GCM cannot
    /// tell these apart, and nothing here should try.
    #[error("the sealed value does not open under this key")]
    Open,
}

/// The 32-byte key that seals values at rest.
pub struct MasterKey(pub [u8; 32]);

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
}

impl MasterKey {
    /// Reads the key from `path`: 64 hex characters and an optional newline.
    pub fn from_file(path: &Path) -> Result<MasterKey, CryptoError> {
        let mut contents = Vec::with_capacity(HEX_LEN + 2);
        // Read one byte past the longest valid file, so an oversized file is
        // refused without reading all of it.
        std::fs::File::open(path)
            .and_then(|f| f.take(HEX_LEN as u64 + 2).read_to_end(&mut contents))
            .map_err(CryptoError::Read)?;
        let hex_part = match contents.strip_suffix(b"\n") {
            Some(rest) => rest,
            None => &contents[..],
        };
        if hex_part.len() != HEX_LEN {
            return Err(CryptoError::Format);
        }
        let mut key = [0u8; 32];
        // `FromHexError` names the offending character, so it is dropped.
        hex::decode_to_slice(hex_part, &mut key).map_err(|_| CryptoError::Format)?;
        Ok(MasterKey(key))
    }

    fn aead_key(&self) -> LessSafeKey {
        // The only failure is a key of the wrong length, which `[u8; 32]`
        // rules out for AES-256.
        let unbound = UnboundKey::new(&AES_256_GCM, &self.0)
            .expect("a 32-byte key is a valid AES-256-GCM key");
        LessSafeKey::new(unbound)
    }

    /// Seals `plaintext` for the slot named by `aad`.
    pub fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        // Without randomness no seal is safe: reusing a nonce under AES-GCM
        // gives the key away. A failing system RNG is not something to carry
        // on through.
        SystemRandom::new()
            .fill(&mut nonce)
            .expect("the system random number generator failed");
        let mut body = plaintext.to_vec();
        // Fails only for inputs of tens of gigabytes.
        self.aead_key()
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut body,
            )
            .expect("the plaintext is within AES-GCM's length limit");
        let mut out = Vec::with_capacity(NONCE_LEN + body.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&body);
        out
    }

    /// Opens a value [`MasterKey::seal`] produced for the same `aad`.
    pub fn open(&self, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if sealed.len() < NONCE_LEN + AES_256_GCM.tag_len() {
            return Err(CryptoError::Open);
        }
        let (nonce, body) = sealed.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| CryptoError::Open)?;
        let mut body = body.to_vec();
        let plaintext_len = self
            .aead_key()
            .open_in_place(nonce, Aad::from(aad), &mut body)
            .map_err(|_| CryptoError::Open)?
            .len();
        body.truncate(plaintext_len);
        Ok(body)
    }
}
