//! The control plane's certificate authority.
//!
//! The first boot creates the CA (EC P-256, ten years) and the platform id
//! key; every boot loads them. The CA signs the gateway listener's server
//! certificate and the client certificate of every gateway credential. A
//! client private key is returned to the operator once and stored nowhere.
//! The CA certificate is kept in `cp_settings`; the CA key and the platform
//! id key are kept in `cp_secrets`, sealed with the master key.
//!
//! A gateway certificate carries two URI SANs, `x-aisix://env/<env_id>` and
//! `x-aisix://dp/<dp_id>`, as separate entries: the gateway reads each from
//! an entry of its own (`crates/aisix-server/src/cert_bundle.rs`).
//!
//! [`Pki`] has no `Debug`: it holds the CA key, and rcgen's `KeyPair` prints
//! its key bytes in `Debug`.

use std::fmt;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use futures::FutureExt;
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType, SerialNumber,
    PKCS_ECDSA_P256_SHA256,
};
use ring::rand::{SecureRandom, SystemRandom};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use x509_parser::extensions::GeneralName;
use x509_parser::pem::parse_x509_pem;
use x509_parser::prelude::{FromDer, X509Certificate};

use crate::crypto::{self, CryptoError, MasterKey};
use crate::db::{Db, DbError};
use crate::settings::{EnvSettings, BOOT_LOCK_KEY};

/// The `cp_settings` key of the CA certificate (PEM).
const CA_CERT_SETTING: &str = "ca_cert_pem";
/// The `cp_secrets` names of the CA key (PKCS#8 DER) and the platform id key.
const CA_KEY_SECRET: &str = "ca_key";
const PLATFORM_ID_KEY_SECRET: &str = "platform_id_key";

const PLATFORM_ID_KEY_LEN: usize = 32;

const CA_VALIDITY: time::Duration = time::Duration::days(3650);
const SERVER_VALIDITY: time::Duration = time::Duration::days(397);
/// Every certificate starts this long before it is made, so a peer whose
/// clock runs a little behind already accepts it.
const BACKDATE: time::Duration = time::Duration::minutes(5);
/// A CA with less than this left signs nothing: what it signed would be
/// unusable within hours, or end before it starts.
const MIN_CA_REMAINING: time::Duration = time::Duration::days(1);

const ENV_URI: &str = "x-aisix://env/";
const DP_URI: &str = "x-aisix://dp/";

/// Why the CA could not be loaded or used. No variant carries key material
/// or stored bytes.
#[derive(Debug, thiserror::Error)]
pub enum PkiError {
    #[error("database: {0}")]
    Db(#[from] DbError),
    /// A different master key than the one the CA was created under, or a
    /// damaged value: AES-GCM cannot tell these apart.
    #[error("the stored CA key or platform id key does not open under CP_MASTER_KEY_FILE")]
    Sealed,
    #[error("the stored CA is unusable: {0}")]
    StoredCa(&'static str),
    #[error("cannot create a certificate: {0}")]
    Sign(#[from] rcgen::Error),
    #[error("the system random number generator failed")]
    Random,
    /// Replacing the CA is the remedy.
    #[error(
        "the control-plane CA has expired or expires within a day; it signs no more \
         certificates until it is replaced"
    )]
    CaExpiring,
    #[error("not a gateway certificate: {0}")]
    NotGateway(&'static str),
}

impl From<CryptoError> for PkiError {
    fn from(e: CryptoError) -> PkiError {
        match e {
            CryptoError::Db(e) => PkiError::Db(e),
            CryptoError::Open | CryptoError::Format | CryptoError::Read(_) => PkiError::Sealed,
        }
    }
}

/// A gateway credential's certificate and private key, as issued.
pub struct IssuedCert {
    pub cert_pem: String,
    /// PKCS#8. Returned to the caller once; nothing stores it.
    pub key_pem: String,
    /// Lowercase hex SHA-256 of the certificate's DER.
    pub fingerprint_sha256: String,
    /// Lowercase hex.
    pub serial: String,
    pub not_before: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
}

/// The private key is not shown.
impl fmt::Debug for IssuedCert {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedCert")
            .field("fingerprint_sha256", &self.fingerprint_sha256)
            .field("serial", &self.serial)
            .field("not_before", &self.not_before)
            .field("not_after", &self.not_after)
            .finish_non_exhaustive()
    }
}

/// Who a gateway certificate names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DpCertIdentity {
    pub env_id: String,
    pub dp_id: String,
    /// Lowercase hex SHA-256 of the certificate's DER.
    pub fingerprint_sha256: String,
}

/// The control plane's CA and platform id key, loaded.
pub struct Pki {
    env_id: Uuid,
    /// The stored certificate, read back for signing: rcgen names the
    /// issuer from it. `ca_key` signs.
    ca: Certificate,
    ca_key: KeyPair,
    ca_pem: String,
    platform_id_key: [u8; PLATFORM_ID_KEY_LEN],
}

impl Pki {
    /// Loads the CA and the platform id key, creating them on the first
    /// boot, in one transaction under [`BOOT_LOCK_KEY`]: instances booting
    /// together on an empty database create one CA, which the first creates
    /// and the others find. The inserts do nothing on a conflict, and what
    /// is returned is always what was read back.
    ///
    /// A master key the stored CA does not open under refuses the boot, as
    /// does a stored certificate whose key is not the stored key.
    pub async fn load_or_init(db: &Db, mk: &MasterKey, env: &EnvSettings) -> Result<Pki, PkiError> {
        // `Db::tx` runs the work in a task of its own, which needs a copy.
        let mk = MasterKey(mk.0);
        let env_id = env.env_id;
        let (ca_pem, ca_key, id_key) = db
            .tx(move |conn| {
                async move {
                    sqlx::query("SELECT pg_advisory_xact_lock($1)")
                        .bind(BOOT_LOCK_KEY)
                        .execute(&mut *conn)
                        .await
                        .map_err(DbError::from)?;
                    if stored_ca_pem(&mut *conn).await?.is_none() {
                        let (pem, key) = new_ca(env_id)?;
                        sqlx::query(
                            "INSERT INTO cp_settings (key, value) VALUES ($1, to_jsonb($2::text)) \
                             ON CONFLICT (key) DO NOTHING",
                        )
                        .bind(CA_CERT_SETTING)
                        .bind(&pem)
                        .execute(&mut *conn)
                        .await
                        .map_err(DbError::from)?;
                        crypto::put_secret(&mut *conn, &mk, CA_KEY_SECRET, &key).await?;
                    }
                    let fresh_id_key = random::<PLATFORM_ID_KEY_LEN>()?;
                    crypto::put_secret(&mut *conn, &mk, PLATFORM_ID_KEY_SECRET, &fresh_id_key)
                        .await?;
                    let pem = stored_ca_pem(&mut *conn)
                        .await?
                        .ok_or(PkiError::StoredCa("its certificate is missing"))?;
                    let key = crypto::get_secret(&mut *conn, &mk, CA_KEY_SECRET)
                        .await?
                        .ok_or(PkiError::StoredCa("its key is missing"))?;
                    let id_key = crypto::get_secret(&mut *conn, &mk, PLATFORM_ID_KEY_SECRET)
                        .await?
                        .ok_or(PkiError::StoredCa("the platform id key is missing"))?;
                    Ok::<_, PkiError>((pem, key, id_key))
                }
                .boxed()
            })
            .await?;
        Pki::from_stored(env_id, ca_pem, &ca_key, &id_key)
    }

    /// The CA from what is stored: its certificate (PEM), its key (PKCS#8
    /// DER) and the platform id key.
    fn from_stored(
        env_id: Uuid,
        ca_pem: String,
        ca_key: &[u8],
        platform_id_key: &[u8],
    ) -> Result<Pki, PkiError> {
        let ca_key = KeyPair::try_from(ca_key)
            .map_err(|_| PkiError::StoredCa("its key is not a PKCS#8 key"))?;
        let (_, pem) = parse_x509_pem(ca_pem.as_bytes())
            .map_err(|_| PkiError::StoredCa("its certificate is not PEM"))?;
        let (_, cert) = X509Certificate::from_der(&pem.contents)
            .map_err(|_| PkiError::StoredCa("its certificate is not X.509"))?;
        if cert.public_key().raw != ca_key.public_key_der().as_slice() {
            return Err(PkiError::StoredCa(
                "its certificate and its key do not belong together",
            ));
        }
        let ca = CertificateParams::from_ca_cert_pem(&ca_pem)
            .and_then(|params| params.self_signed(&ca_key))
            .map_err(|_| PkiError::StoredCa("its certificate cannot be read for signing"))?;
        let platform_id_key = platform_id_key
            .try_into()
            .map_err(|_| PkiError::StoredCa("the platform id key is not 32 bytes"))?;
        Ok(Pki {
            env_id,
            ca,
            ca_key,
            ca_pem,
            platform_id_key,
        })
    }

    /// The CA certificate, PEM: what gateways trust the control plane by.
    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    /// The platform id key, as 64 hex characters: the value gateways are
    /// given as `PLATFORM_ID_KEY`.
    pub fn platform_id_key_hex(&self) -> String {
        hex::encode(self.platform_id_key)
    }

    /// A server certificate for the gateway listener, valid for 397 days:
    /// an IP SAN for each host that parses as an IP address, a DNS SAN for
    /// each other one. The chain is the leaf alone; gateways hold the CA.
    pub fn server_cert(
        &self,
        hosts: &[String],
    ) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), PkiError> {
        let mut params = CertificateParams::new(hosts.to_vec())?;
        params.distinguished_name = common_name("AISIX control plane");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let (cert, key) = self.sign_leaf(params, SERVER_VALIDITY)?;
        Ok((
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        ))
    }

    /// A client certificate for the gateway credential `dp_id`, valid for
    /// `validity_days`, with a fresh key.
    pub fn issue_client(&self, dp_id: Uuid, validity_days: u32) -> Result<IssuedCert, PkiError> {
        let mut params = CertificateParams::default();
        params.distinguished_name = common_name(&format!("dp-{dp_id}"));
        params.subject_alt_names = vec![
            SanType::URI(format!("{ENV_URI}{}", self.env_id).try_into()?),
            SanType::URI(format!("{DP_URI}{dp_id}").try_into()?),
        ];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let validity = time::Duration::days(i64::from(validity_days));
        let (cert, key) = self.sign_leaf(params, validity)?;
        let signed = cert.params();
        Ok(IssuedCert {
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
            fingerprint_sha256: fingerprint(cert.der()),
            serial: signed
                .serial_number
                .as_ref()
                .map(|s| hex::encode(s.to_bytes()))
                .unwrap_or_default(),
            not_before: to_chrono(signed.not_before),
            not_after: to_chrono(signed.not_after),
        })
    }

    /// Signs `params` as an end-entity certificate of this CA, with a fresh
    /// P-256 key and a random serial, valid from a little before now for
    /// `validity`, but never past the CA's own end: a chain is invalid once
    /// its CA expires, whatever the leaf says. A CA with less than a day
    /// left signs nothing ([`PkiError::CaExpiring`]).
    fn sign_leaf(
        &self,
        mut params: CertificateParams,
        validity: time::Duration,
    ) -> Result<(Certificate, KeyPair), PkiError> {
        let now = now();
        let ca_end = self.ca.params().not_after;
        if ca_end - now < MIN_CA_REMAINING {
            return Err(PkiError::CaExpiring);
        }
        params.not_before = now - BACKDATE;
        params.not_after = (now + validity).min(ca_end);
        params.serial_number = Some(SerialNumber::from_slice(&random_serial()?));
        params.is_ca = IsCa::ExplicitNoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.use_authority_key_identifier_extension = true;
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let cert = params.signed_by(&key, &self.ca, &self.ca_key)?;
        Ok((cert, key))
    }
}

/// Who a gateway certificate (DER) names, read the way the gateway reads it:
/// the URI SAN entries starting `x-aisix://env/` and `x-aisix://dp/`.
///
/// Refused: no SAN, no dp, more than one entry of either kind, and an
/// `env_id` that is empty or contains `/`, which could reach outside the
/// environment's key space. It does not check the chain: the listener's TLS
/// verifier does that first.
pub fn identity_of(cert_der: &[u8]) -> Result<DpCertIdentity, PkiError> {
    let (_, cert) = X509Certificate::from_der(cert_der)
        .map_err(|_| PkiError::NotGateway("not an X.509 certificate"))?;
    let san = cert
        .subject_alternative_name()
        .map_err(|_| PkiError::NotGateway("its SAN extension is unreadable"))?
        .ok_or(PkiError::NotGateway("it has no SAN extension"))?;
    let (mut env_id, mut dp_id) = (None, None);
    for name in &san.value.general_names {
        let GeneralName::URI(uri) = name else {
            continue;
        };
        let (slot, value) = if let Some(rest) = uri.strip_prefix(ENV_URI) {
            (&mut env_id, rest)
        } else if let Some(rest) = uri.strip_prefix(DP_URI) {
            (&mut dp_id, rest)
        } else {
            continue;
        };
        if slot.replace(value).is_some() {
            return Err(PkiError::NotGateway("it names more than one env or dp"));
        }
    }
    let env_id = env_id
        .filter(|e| !e.is_empty() && !e.contains('/'))
        .ok_or(PkiError::NotGateway("it names no valid env"))?;
    let dp_id = dp_id
        .filter(|d| !d.is_empty())
        .ok_or(PkiError::NotGateway("it names no dp"))?;
    Ok(DpCertIdentity {
        env_id: env_id.to_string(),
        dp_id: dp_id.to_string(),
        fingerprint_sha256: fingerprint(cert_der),
    })
}

/// A new CA for `env_id`: its certificate (PEM) and its key (PKCS#8 DER).
/// It signs end-entity certificates only (path length 0).
fn new_ca(env_id: Uuid) -> Result<(String, Vec<u8>), PkiError> {
    let now = now();
    let mut params = CertificateParams::default();
    params.distinguished_name = common_name(&format!("AISIX control plane CA {env_id}"));
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.serial_number = Some(SerialNumber::from_slice(&random_serial()?));
    params.not_before = now - BACKDATE;
    params.not_after = now + CA_VALIDITY;
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let cert = params.self_signed(&key)?;
    Ok((cert.pem(), key.serialize_der()))
}

async fn stored_ca_pem(conn: &mut PgConnection) -> Result<Option<String>, DbError> {
    Ok(
        sqlx::query_scalar("SELECT value #>> '{}' FROM cp_settings WHERE key = $1")
            .bind(CA_CERT_SETTING)
            .fetch_optional(conn)
            .await?,
    )
}

fn common_name(name: &str) -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, name);
    dn
}

fn fingerprint(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

/// Now, to the whole second a certificate records, so a time computed from
/// it is the certificate's own.
fn now() -> OffsetDateTime {
    let now = OffsetDateTime::now_utc();
    now - time::Duration::nanoseconds(i64::from(now.nanosecond()))
}

fn to_chrono(t: OffsetDateTime) -> DateTime<Utc> {
    DateTime::<Utc>::from(SystemTime::from(t))
}

fn random<const N: usize>() -> Result<[u8; N], PkiError> {
    let mut bytes = [0u8; N];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| PkiError::Random)?;
    Ok(bytes)
}

/// 16 random bytes, positive and with a non-zero first byte, so the DER
/// keeps all 16 and the hex stored is the certificate's serial.
fn random_serial() -> Result<[u8; 16], PkiError> {
    let mut serial = random::<16>()?;
    serial[0] = (serial[0] & 0x7f) | 0x01;
    Ok(serial)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rcgen::{CertificateParams, KeyPair, SanType};
    use rustls::client::danger::ServerCertVerifier;
    use rustls::client::WebPkiServerVerifier;
    use rustls::crypto::CryptoProvider;
    use rustls::server::WebPkiClientVerifier;
    use rustls::RootCertStore;
    use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
    use uuid::Uuid;
    use x509_parser::extensions::GeneralName;
    use x509_parser::pem::parse_x509_pem;
    use x509_parser::prelude::{FromDer, X509Certificate};

    use super::*;

    const ENV: Uuid = Uuid::from_u128(0x3f2b_6c1e_7a4d_4e8f_9b10_2c3d_4e5f_6a7b);
    const DP: Uuid = Uuid::from_u128(0x8d1c_0b2a_4f3e_4d5c_a6b7_c8d9_e0f1_0213);

    fn test_pki(env_id: Uuid) -> Pki {
        let (ca_pem, ca_key) = new_ca(env_id).expect("a CA");
        Pki::from_stored(env_id, ca_pem, &ca_key, &[0x42; PLATFORM_ID_KEY_LEN])
            .expect("the CA loads")
    }

    fn der(pem: &str) -> Vec<u8> {
        parse_x509_pem(pem.as_bytes())
            .expect("a PEM certificate")
            .1
            .contents
    }

    fn uri_sans(pem: &str) -> Vec<String> {
        let der = der(pem);
        let (_, cert) = X509Certificate::from_der(&der).expect("an X.509 certificate");
        let san = cert
            .subject_alternative_name()
            .expect("a readable SAN")
            .expect("a SAN extension");
        san.value
            .general_names
            .iter()
            .filter_map(|n| match n {
                GeneralName::URI(uri) => Some(uri.to_string()),
                _ => None,
            })
            .collect()
    }

    fn provider() -> Arc<CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    fn roots(pki: &Pki) -> Arc<RootCertStore> {
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(der(pki.ca_pem())))
            .expect("the CA as a root");
        Arc::new(roots)
    }

    /// A self-signed certificate with these URI SANs: what someone who does
    /// not hold the CA might present.
    fn with_uris(uris: &[String]) -> Vec<u8> {
        let mut params = CertificateParams::default();
        params.subject_alt_names = uris
            .iter()
            .map(|u| SanType::URI(u.as_str().try_into().expect("an IA5 string")))
            .collect();
        let key = KeyPair::generate().expect("a key");
        params
            .self_signed(&key)
            .expect("a certificate")
            .der()
            .to_vec()
    }

    #[test]
    fn an_issued_certificate_carries_exactly_the_two_uri_sans() {
        let c = test_pki(ENV).issue_client(DP, 30).unwrap();
        assert_eq!(
            uri_sans(&c.cert_pem),
            vec![format!("x-aisix://env/{ENV}"), format!("x-aisix://dp/{DP}")]
        );
    }

    #[test]
    fn an_issued_certificate_chains_to_the_ca_and_is_for_client_auth() {
        let pki = test_pki(ENV);
        let c = pki.issue_client(DP, 30).unwrap();
        let leaf = CertificateDer::from(der(&c.cert_pem));
        let verifier = WebPkiClientVerifier::builder_with_provider(roots(&pki), provider())
            .build()
            .expect("a client verifier");
        verifier
            .verify_client_cert(&leaf, &[], UnixTime::now())
            .expect("a client certificate of this CA");
        // Another CA's certificate is not.
        let foreign = test_pki(ENV).issue_client(DP, 30).unwrap();
        assert!(verifier
            .verify_client_cert(
                &CertificateDer::from(der(&foreign.cert_pem)),
                &[],
                UnixTime::now()
            )
            .is_err());

        let (_, cert) = X509Certificate::from_der(&leaf).expect("an X.509 certificate");
        let eku = cert
            .extended_key_usage()
            .expect("a readable EKU")
            .expect("an EKU")
            .value;
        assert!(eku.client_auth && !eku.server_auth, "{eku:?}");
        assert!(!cert.is_ca());
        let cn: Vec<&str> = cert
            .subject()
            .iter_common_name()
            .filter_map(|cn| cn.as_str().ok())
            .collect();
        assert_eq!(cn, vec![format!("dp-{DP}")]);
        // The validity reported is the certificate's own, 30 days ending.
        let validity = cert.validity();
        assert_eq!(validity.not_before.timestamp(), c.not_before.timestamp());
        assert_eq!(validity.not_after.timestamp(), c.not_after.timestamp());
        let days = (c.not_after - chrono::Utc::now()).num_hours();
        assert!((30 * 24 - 1..=30 * 24).contains(&days), "{days} hours");
        assert!(c.not_before <= chrono::Utc::now());
        // What the gateway listener reads off it.
        let id = identity_of(&leaf).expect("a gateway certificate");
        assert_eq!(id.env_id, ENV.to_string());
        assert_eq!(id.dp_id, DP.to_string());
        assert_eq!(id.fingerprint_sha256, c.fingerprint_sha256);
        assert_eq!(c.fingerprint_sha256.len(), 64);
        assert_eq!(
            hex::decode(&c.serial).expect("a hex serial"),
            cert.raw_serial()
        );
    }

    #[test]
    fn a_certificate_whose_env_contains_a_slash_has_no_identity() {
        let dp = format!("x-aisix://dp/{DP}");
        let good = with_uris(&[format!("x-aisix://env/{ENV}"), dp.clone()]);
        assert_eq!(
            identity_of(&good).expect("a well-formed identity").env_id,
            ENV.to_string()
        );
        for env in [format!("{ENV}/x"), "a/b".to_string(), String::new()] {
            let der = with_uris(&[format!("x-aisix://env/{env}"), dp.clone()]);
            assert!(identity_of(&der).is_err(), "env {env:?} has an identity");
        }
        // Nor one without its dp, or naming two environments.
        let no_dp = with_uris(&[format!("x-aisix://env/{ENV}")]);
        assert!(identity_of(&no_dp).is_err());
        let two_envs = with_uris(&[
            format!("x-aisix://env/{ENV}"),
            format!("x-aisix://env/{}", Uuid::new_v4()),
            dp,
        ]);
        assert!(identity_of(&two_envs).is_err());
    }

    #[test]
    fn the_ca_is_a_ten_year_ca_for_certificates_and_crls() {
        let pki = test_pki(ENV);
        let der = der(pki.ca_pem());
        let (_, cert) = X509Certificate::from_der(&der).expect("an X.509 certificate");
        assert!(cert.is_ca());
        // It signs end-entity certificates only, never another CA.
        let constraints = cert
            .basic_constraints()
            .expect("readable basic constraints")
            .expect("basic constraints")
            .value;
        assert!(constraints.ca);
        assert_eq!(constraints.path_len_constraint, Some(0));
        let cn: Vec<&str> = cert
            .subject()
            .iter_common_name()
            .filter_map(|cn| cn.as_str().ok())
            .collect();
        assert_eq!(cn, vec![format!("AISIX control plane CA {ENV}")]);
        let usage = cert
            .key_usage()
            .expect("a readable key usage")
            .expect("a key usage")
            .value;
        assert!(usage.key_cert_sign() && usage.crl_sign(), "{usage:?}");
        assert!(!usage.digital_signature(), "{usage:?}");
        let years = (cert.validity().not_after.timestamp() - chrono::Utc::now().timestamp()) as f64
            / (365.0 * 86_400.0);
        assert!((9.9..=10.1).contains(&years), "{years} years");
        // An EC P-256 key.
        let curve = cert
            .public_key()
            .algorithm
            .parameters
            .as_ref()
            .and_then(|p| p.as_oid().ok())
            .map(|oid| oid.to_id_string());
        assert_eq!(curve.as_deref(), Some("1.2.840.10045.3.1.7"));
        assert_eq!(pki.platform_id_key_hex(), "42".repeat(32));
    }

    /// A `Pki` whose CA, otherwise like a real one, ends at `not_after`.
    fn pki_with_ca_ending(not_after: OffsetDateTime) -> Pki {
        let mut params = CertificateParams::default();
        params.distinguished_name = common_name(&format!("AISIX control plane CA {ENV}"));
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.not_before = not_after - time::Duration::days(3650);
        params.not_after = not_after;
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("a key");
        let ca = params.self_signed(&key).expect("a CA");
        Pki::from_stored(ENV, ca.pem(), &key.serialize_der(), &[0x42; 32]).expect("the CA loads")
    }

    #[test]
    fn an_expired_or_expiring_ca_issues_nothing() {
        let hosts = ["cp.example.test".to_string()];
        for (left, what) in [
            (-time::Duration::days(30), "expired"),
            (-time::Duration::seconds(1), "just expired"),
            (time::Duration::hours(12), "half a day left"),
        ] {
            let pki = pki_with_ca_ending(now() + left);
            let refused = pki.issue_client(DP, 30);
            assert!(
                matches!(refused, Err(PkiError::CaExpiring)),
                "{what}: {refused:?}"
            );
            assert!(
                matches!(pki.server_cert(&hosts), Err(PkiError::CaExpiring)),
                "{what}: a server certificate"
            );
        }
        let message = PkiError::CaExpiring.to_string();
        assert!(message.contains("expire"), "{message}");
        // Two days left is enough, clamped to the CA's end.
        let pki = pki_with_ca_ending(now() + time::Duration::days(2));
        pki.issue_client(DP, 30).expect("two days left issues");
        pki.server_cert(&hosts).expect("two days left serves");
    }

    #[test]
    fn a_leaf_never_outlives_its_ca() {
        // A CA with ten days left.
        let now = now();
        let ca_end = now + time::Duration::days(10);
        let pki = pki_with_ca_ending(ca_end);
        let end_of = |der: &[u8]| {
            let (_, cert) = X509Certificate::from_der(der).expect("an X.509 certificate");
            cert.validity().not_after.timestamp()
        };

        let long = pki.issue_client(DP, 825).unwrap();
        assert_eq!(long.not_after, to_chrono(ca_end), "the reported end");
        assert_eq!(end_of(&der(&long.cert_pem)), ca_end.unix_timestamp());
        let (chain, _) = pki
            .server_cert(&["cp.example.test".to_string()])
            .expect("a server certificate");
        assert_eq!(end_of(&chain[0]), ca_end.unix_timestamp());
        // A leaf that ends first keeps its own end.
        let short = pki.issue_client(DP, 3).unwrap();
        let three_days = (now + time::Duration::days(3)).unix_timestamp();
        let end = end_of(&der(&short.cert_pem));
        assert!((three_days..three_days + 5).contains(&end), "{end}");
    }

    #[test]
    fn the_server_certificate_names_every_host_and_is_for_server_auth() {
        let pki = test_pki(ENV);
        let hosts = [
            "cp.example.test".to_string(),
            "127.0.0.1".to_string(),
            "::1".to_string(),
        ];
        let (chain, key) = pki.server_cert(&hosts).expect("a server certificate");
        assert_eq!(chain.len(), 1);
        assert!(matches!(key, rustls_pki_types::PrivateKeyDer::Pkcs8(_)));
        let verifier = WebPkiServerVerifier::builder_with_provider(roots(&pki), provider())
            .build()
            .expect("a server verifier");
        for host in ["cp.example.test", "127.0.0.1", "::1"] {
            let name = ServerName::try_from(host).expect("a server name");
            verifier
                .verify_server_cert(&chain[0], &[], &name, &[], UnixTime::now())
                .unwrap_or_else(|e| panic!("{host}: {e}"));
        }
        let other = ServerName::try_from("other.example.test").expect("a server name");
        assert!(verifier
            .verify_server_cert(&chain[0], &[], &other, &[], UnixTime::now())
            .is_err());

        let (_, cert) = X509Certificate::from_der(&chain[0]).expect("an X.509 certificate");
        let eku = cert
            .extended_key_usage()
            .expect("a readable EKU")
            .expect("an EKU")
            .value;
        assert!(eku.server_auth && !eku.client_auth, "{eku:?}");
        let hours =
            (cert.validity().not_after.timestamp() - chrono::Utc::now().timestamp()) / 3_600;
        assert!((397 * 24 - 1..=397 * 24).contains(&hours), "{hours} hours");
        // A gateway that presented it as a client would have no identity.
        assert!(identity_of(&chain[0]).is_err());
    }
}
