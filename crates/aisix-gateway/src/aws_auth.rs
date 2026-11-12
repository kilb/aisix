//! How a request to AWS Bedrock authenticates: a Bedrock API key sent as a
//! bearer token, or SigV4 with credentials that are either configured
//! explicitly, taken from the gateway's own AWS environment (the default
//! credential chain), and optionally exchanged for a role through
//! `sts:AssumeRole`.
//!
//! Temporary credentials refresh on their own: [`resolve`] hands every
//! caller with the same configuration one shared provider, which caches
//! what it fetched until shortly before it expires.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use aws_config::default_provider::credentials::DefaultCredentialsChain;
use aws_config::default_provider::{use_dual_stack, use_fips};
use aws_config::provider_config::ProviderConfig;
use aws_config::{BehaviorVersion, Region, SdkConfig};
use aws_credential_types::provider::{
    error::CredentialsError, future, ProvideCredentials, SharedCredentialsProvider,
};
use aws_credential_types::Credentials;

/// Credentials are refreshed this long before they expire, so a request
/// never signs with credentials that die while it is in flight.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// Credentials that carry no expiry (long-lived keys from the environment or
/// a credentials file) are fetched again after this long, so a rotated
/// credentials file is picked up.
const UNEXPIRING_REFRESH: Duration = Duration::from_secs(300);

/// The longest one credential fetch (default chain, STS) may take. Requests
/// on the same configuration wait for a fetch in progress, so it is bounded
/// on its own rather than by any one request's deadline.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Distinct fetched-credential configurations kept at once. Rotating a
/// configuration's secret leaves the old entry behind; past this many the
/// cache starts over, which costs each configuration one fresh fetch.
const MAX_PROVIDERS: usize = 256;

const PROVIDER_NAME: &str = "aisix-aws-auth";

/// Where the base credentials come from.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum AwsAuthMode {
    /// Explicitly configured access keys.
    AccessKey {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    },
    /// A Bedrock API key, sent as `Authorization: Bearer`.
    ApiKey(String),
    /// The gateway's own AWS environment: environment variables, web
    /// identity (IRSA), the ECS container endpoint, the instance profile.
    DefaultChain,
}

/// A role the base credentials are exchanged for.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AssumeRole {
    pub role_arn: String,
    pub external_id: Option<String>,
    pub role_session_name: Option<String>,
}

/// One configured way of authenticating to AWS in one region.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AwsAuthSpec {
    pub mode: AwsAuthMode,
    /// Never set together with [`AwsAuthMode::ApiKey`]; the parsers of
    /// each configuration surface reject that combination.
    pub assume_role: Option<AssumeRole>,
    pub region: String,
}

impl std::fmt::Debug for AwsAuthSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsAuthSpec")
            .field("mode", &self.mode_name())
            .field("role_arn", &self.assume_role.as_ref().map(|r| &r.role_arn))
            .field("region", &self.region)
            .finish()
    }
}

impl AwsAuthSpec {
    /// The `auth_type` this specification was configured with.
    pub fn mode_name(&self) -> &'static str {
        match self.mode {
            AwsAuthMode::AccessKey { .. } => "access_key",
            AwsAuthMode::ApiKey(_) => "api_key",
            AwsAuthMode::DefaultChain => "default_chain",
        }
    }
}

/// What a request carries to authenticate.
#[derive(Clone)]
pub enum AwsAuth {
    /// `Authorization: Bearer <token>`, no SigV4.
    Bearer(String),
    /// SigV4 with credentials from this provider.
    SigV4(SharedCredentialsProvider),
}

/// Whether a credential provider's error says the configured credentials
/// are missing or refused — the configuration's to fix — rather than that
/// a credential service failed and may answer the next attempt. Every
/// Bedrock consumer classifies a credential failure through this, so they
/// cannot drift.
pub fn credentials_refused(err: &CredentialsError) -> bool {
    matches!(
        err,
        CredentialsError::CredentialsNotLoaded(_) | CredentialsError::InvalidConfiguration(_)
    )
}

/// The authentication for `spec`. Credentials that are fetched rather than
/// configured come from one provider per distinct `spec`, shared by every
/// caller, so a refresh is paid once per configuration rather than once per
/// request.
pub fn resolve(spec: &AwsAuthSpec) -> AwsAuth {
    match (&spec.mode, &spec.assume_role) {
        (AwsAuthMode::ApiKey(key), _) => AwsAuth::Bearer(key.clone()),
        (
            AwsAuthMode::AccessKey {
                access_key_id,
                secret_access_key,
                session_token,
            },
            None,
        ) => AwsAuth::SigV4(SharedCredentialsProvider::new(Credentials::new(
            access_key_id,
            secret_access_key,
            session_token.clone(),
            None,
            PROVIDER_NAME,
        ))),
        _ => {
            static PROVIDERS: OnceLock<Mutex<HashMap<AwsAuthSpec, SharedCredentialsProvider>>> =
                OnceLock::new();
            let mut providers = PROVIDERS
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(provider) = providers.get(spec) {
                return AwsAuth::SigV4(provider.clone());
            }
            if providers.len() >= MAX_PROVIDERS {
                providers.clear();
            }
            let provider = SharedCredentialsProvider::new(RefreshingCredentials {
                spec: spec.clone(),
                cached: tokio::sync::Mutex::new(None),
                sdk_config: tokio::sync::OnceCell::new(),
                http_override: None,
            });
            providers.insert(spec.clone(), provider.clone());
            AwsAuth::SigV4(provider)
        }
    }
}

/// 为单个调用隔离凭据交换传输，不读取或复用其它调用的全局凭据缓存。
/// 只允许显式凭据；另一进程的环境凭据不能代表网关自身的身份。
pub fn resolve_with_http_client(
    spec: &AwsAuthSpec,
    http: aws_smithy_runtime_api::client::http::SharedHttpClient,
) -> Result<AwsAuth, CredentialsError> {
    match (&spec.mode, &spec.assume_role) {
        (AwsAuthMode::DefaultChain, _) => Err(CredentialsError::invalid_configuration(
            "process AWS credentials cannot use an isolated credential transport",
        )),
        (
            AwsAuthMode::AccessKey {
                access_key_id,
                secret_access_key,
                session_token,
            },
            Some(_),
        ) => {
            // 基底会话令牌先于角色凭据发往 STS，必须在任何交换或错误日志之前登记。
            if let Some(token) = session_token {
                crate::response_tap::note_sent_credential(token);
            }
            Ok(AwsAuth::SigV4(SharedCredentialsProvider::new(
                RefreshingCredentials {
                    spec: spec.clone(),
                    cached: tokio::sync::Mutex::new(None),
                    sdk_config: tokio::sync::OnceCell::new(),
                    http_override: Some(ExplicitCredentialTransport {
                        http,
                        base: Credentials::new(
                            access_key_id,
                            secret_access_key,
                            session_token.clone(),
                            None,
                            PROVIDER_NAME,
                        ),
                    }),
                },
            )))
        }
        _ => Ok(resolve(spec)),
    }
}

struct ExplicitCredentialTransport {
    http: aws_smithy_runtime_api::client::http::SharedHttpClient,
    base: Credentials,
}

/// Fetches credentials for the default chain and for AssumeRole, and keeps
/// them until [`EXPIRY_MARGIN`] before they expire.
struct RefreshingCredentials {
    spec: AwsAuthSpec,
    /// The last credentials fetched, and when.
    cached: tokio::sync::Mutex<Option<(Credentials, SystemTime)>>,
    sdk_config: tokio::sync::OnceCell<SdkConfig>,
    http_override: Option<ExplicitCredentialTransport>,
}

impl std::fmt::Debug for RefreshingCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshingCredentials")
            .field("spec", &self.spec)
            .finish()
    }
}

impl ProvideCredentials for RefreshingCredentials {
    fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        future::ProvideCredentials::new(self.credentials())
    }
}

impl RefreshingCredentials {
    fn error_for_log(&self, error: impl std::fmt::Display) -> String {
        let detail = error.to_string();
        if self.http_override.is_some() {
            crate::upstream_error_scrub::scrub_upstream_error_text(&detail)
        } else {
            detail
        }
    }

    async fn credentials(&self) -> Result<Credentials, CredentialsError> {
        let mut cached = self.cached.lock().await;
        let now = SystemTime::now();
        if let Some((c, fetched_at)) = cached.as_ref() {
            let fresh = match c.expiry() {
                Some(exp) => exp > now + EXPIRY_MARGIN,
                None => now < *fetched_at + UNEXPIRING_REFRESH,
            };
            if fresh {
                return Ok(c.clone());
            }
        }
        let fetched = tokio::time::timeout(FETCH_TIMEOUT, self.fetch())
            .await
            .unwrap_or_else(|_| Err(CredentialsError::provider_timed_out(FETCH_TIMEOUT)));
        match fetched {
            Ok(c) => {
                *cached = Some((c.clone(), now));
                Ok(c)
            }
            Err(e) => {
                let error =
                    self.error_for_log(aws_smithy_types::error::display::DisplayErrorContext(&e));
                tracing::warn!(
                    auth_type = self.spec.mode_name(),
                    role_arn = self.spec.assume_role.as_ref().map(|r| r.role_arn.as_str()),
                    region = %self.spec.region,
                    error = %error,
                    "could not obtain AWS credentials"
                );
                // A refresh that fails inside the margin keeps serving the
                // credentials it was meant to replace until they expire.
                match cached.as_ref() {
                    Some((c, _)) if c.expiry().is_none_or(|exp| exp > now) => Ok(c.clone()),
                    _ => Err(e),
                }
            }
        }
    }

    /// The SDK configuration the gateway's AWS environment describes:
    /// the default credential chain, and the STS endpoint
    /// (`AWS_ENDPOINT_URL_STS` / `AWS_ENDPOINT_URL` when set).
    async fn sdk_config(&self) -> &SdkConfig {
        self.sdk_config
            .get_or_init(|| async {
                let mut timeouts = aws_smithy_types::timeout::TimeoutConfig::builder();
                match crate::upstream_http::config().connect_timeout {
                    Some(d) => timeouts = timeouts.connect_timeout(d),
                    None => timeouts = timeouts.disable_connect_timeout(),
                }
                let region = Region::new(self.spec.region.clone());
                if let Some(transport) = &self.http_override {
                    // 调用方逐请求检查真实 STS 地址；显式基底不加载环境凭据链。
                    return aws_config::defaults(BehaviorVersion::latest())
                        .region(region)
                        .http_client(transport.http.clone())
                        .credentials_provider(transport.base.clone())
                        .retry_config(aws_smithy_types::retry::RetryConfig::disabled())
                        .timeout_config(timeouts.build())
                        .load()
                        .await;
                }
                // The chain reaches its own credential endpoints directly
                // and everything else — its STS calls included — through
                // the proxy environment; its instance-metadata client is
                // never proxied, wherever aws-config resolves the endpoint
                // (the default, the IPv6 endpoint mode, or a configured
                // one). Building the chain here, rather than letting the
                // loader do it, is what lets it take that client, so the
                // FIPS / dual-stack settings the loader would have read for
                // it are read here the same way.
                let imds = aws_config::imds::Client::builder()
                    .configure(&ProviderConfig::default().with_http_client(
                        crate::upstream_tls::aws_credential_endpoint_http_client(),
                    ))
                    .build();
                let conf = ProviderConfig::default()
                    .with_http_client(crate::upstream_tls::aws_credential_chain_http_client());
                let use_fips = use_fips::use_fips_provider(&conf).await;
                let use_dual_stack = use_dual_stack::use_dual_stack_provider(&conf).await;
                let conf = conf
                    .with_use_fips(use_fips)
                    .with_use_dual_stack(use_dual_stack);
                // A profile's `credential_source = Ec2InstanceMetadata`
                // builds an instance-metadata provider of its own; this
                // one replaces it so that call is direct too.
                let profile_imds =
                    aws_config::imds::credentials::ImdsCredentialsProvider::builder()
                        .imds_client(imds.clone())
                        .configure(&conf)
                        .build();
                let chain = DefaultCredentialsChain::builder()
                    .configure(conf)
                    .region(region.clone())
                    .imds_client(imds)
                    .with_custom_credential_source("Ec2InstanceMetadata", profile_imds)
                    .build()
                    .await;
                aws_config::defaults(BehaviorVersion::latest())
                    .region(region)
                    .http_client(crate::upstream_tls::aws_http_client())
                    .credentials_provider(chain)
                    .timeout_config(timeouts.build())
                    .load()
                    .await
            })
            .await
    }

    async fn fetch(&self) -> Result<Credentials, CredentialsError> {
        let base = match &self.spec.mode {
            AwsAuthMode::AccessKey {
                access_key_id,
                secret_access_key,
                session_token,
            } => Credentials::new(
                access_key_id,
                secret_access_key,
                session_token.clone(),
                None,
                PROVIDER_NAME,
            ),
            AwsAuthMode::DefaultChain => {
                let chain = self
                    .sdk_config()
                    .await
                    .credentials_provider()
                    .ok_or_else(|| {
                        CredentialsError::not_loaded(
                            "the default AWS credential chain is unavailable",
                        )
                    })?;
                chain.provide_credentials().await?
            }
            AwsAuthMode::ApiKey(_) => {
                return Err(CredentialsError::invalid_configuration(
                    "a Bedrock API key does not sign requests",
                ))
            }
        };
        let Some(role) = &self.spec.assume_role else {
            return Ok(base);
        };
        let sts_conf = aws_sdk_sts::config::Builder::from(self.sdk_config().await)
            .credentials_provider(SharedCredentialsProvider::new(base.clone()))
            .build();
        let sts = aws_sdk_sts::Client::from_conf(sts_conf);
        if self.already_is_role(&sts, &role.role_arn).await {
            tracing::debug!(
                role_arn = %role.role_arn,
                "the current AWS identity already is the configured role; using its credentials"
            );
            return Ok(base);
        }
        let session_name = role.role_session_name.clone().unwrap_or_else(|| {
            let secs = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default();
            format!("aisix-session-{secs}")
        });
        let out = sts
            .assume_role()
            .role_arn(&role.role_arn)
            .role_session_name(session_name)
            .set_external_id(role.external_id.clone())
            .send()
            .await
            .map_err(|e| {
                let msg = format!(
                    "sts:AssumeRole for {} failed: {}",
                    role.role_arn,
                    aws_sdk_sts::error::DisplayErrorContext(&e)
                );
                // A refusal (access denied, malformed role) is the
                // configuration's to fix; anything else may pass.
                let status = e.raw_response().map(|r| r.status().as_u16());
                let throttled = e.as_service_error().and_then(|s| {
                    use aws_sdk_sts::error::ProvideErrorMetadata as _;
                    s.code().map(|c| c.contains("Throttl"))
                }) == Some(true);
                match status {
                    Some(400..=499) if !throttled => CredentialsError::invalid_configuration(msg),
                    _ => CredentialsError::provider_error(msg),
                }
            })?;
        let c = out.credentials().ok_or_else(|| {
            CredentialsError::unhandled("sts:AssumeRole answered without credentials")
        })?;
        let expiry = SystemTime::try_from(*c.expiration()).map_err(|_| {
            CredentialsError::unhandled("sts:AssumeRole answered with an unusable expiration")
        })?;
        Ok(Credentials::new(
            c.access_key_id(),
            c.secret_access_key(),
            Some(c.session_token().to_string()),
            Some(expiry),
            PROVIDER_NAME,
        ))
    }

    /// Whether the base credentials already are `role_arn`, in which case
    /// there is nothing to assume — a role is often not trusted to assume
    /// itself. Under web identity (IRSA) the role is named in the
    /// environment (unless environment access keys, which the chain
    /// prefers, are set); otherwise `sts:GetCallerIdentity` says who the
    /// credentials belong to. Asked once per credential refresh. An
    /// identity that cannot be determined is treated as a different one,
    /// so the role is assumed.
    async fn already_is_role(&self, sts: &aws_sdk_sts::Client, role_arn: &str) -> bool {
        let Some(target) = role_identity(role_arn) else {
            return false;
        };
        if self.spec.mode == AwsAuthMode::DefaultChain
            && std::env::var_os("AWS_ACCESS_KEY_ID").is_none()
        {
            if let (Ok(web_role), Ok(_)) = (
                std::env::var("AWS_ROLE_ARN"),
                std::env::var("AWS_WEB_IDENTITY_TOKEN_FILE"),
            ) {
                return role_identity(&web_role) == Some(target);
            }
        }
        match sts.get_caller_identity().send().await {
            Ok(out) => out.arn().and_then(role_identity) == Some(target),
            Err(e) => {
                let error = self.error_for_log(aws_sdk_sts::error::DisplayErrorContext(&e));
                tracing::debug!(
                    error = %error,
                    "sts:GetCallerIdentity failed; assuming the configured role"
                );
                false
            }
        }
    }
}

/// `(partition, account, role name)` of an IAM role ARN
/// (`arn:aws:iam::123:role/path/Name`) or an assumed-role session ARN
/// (`arn:aws:sts::123:assumed-role/Name/session`).
fn role_identity(arn: &str) -> Option<(&str, &str, &str)> {
    let mut parts = arn.splitn(6, ':');
    if parts.next()? != "arn" {
        return None;
    }
    let partition = parts.next()?;
    let _service = parts.next()?;
    let _region = parts.next()?;
    let account = parts.next()?;
    let resource = parts.next()?;
    let name = if let Some(path) = resource.strip_prefix("role/") {
        path.rsplit('/').next()?
    } else {
        resource.strip_prefix("assumed-role/")?.split('/').next()?
    };
    (!name.is_empty()).then_some((partition, account, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn isolated_role_auth_notes_the_base_session_token_before_fetching() {
        let token = "vF9qzP6mW3rB8cD1xK2sL7nJ4hG0yT5u";
        let tap = std::sync::Arc::new(crate::response_tap::ResponseHeaderTap::new(
            vec![],
            false,
            "probe-base-credentials".into(),
        ));
        crate::response_tap::scope(tap, async {
            let spec = AwsAuthSpec {
                mode: AwsAuthMode::AccessKey {
                    access_key_id: "probe-access-key".into(),
                    secret_access_key: "probe-secret-key".into(),
                    session_token: Some(token.into()),
                },
                assume_role: Some(AssumeRole {
                    role_arn: "arn:aws:iam::123456789012:role/probe".into(),
                    external_id: None,
                    role_session_name: None,
                }),
                region: "us-east-1".into(),
            };
            let http = aws_smithy_runtime_api::client::http::http_client_fn(|_, _| {
                panic!("登记凭据的检查不发起网络请求")
            });
            assert!(resolve_with_http_client(&spec, http).is_ok());
            let detail = format!("STS echoed {token}");
            let scrubbed = crate::upstream_error_scrub::scrub_upstream_error_text(&detail);
            assert!(
                !scrubbed.contains(token),
                "基底会话令牌必须在任何交换之前登记"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn an_injected_credential_transport_is_isolated_and_disables_sdk_retries() {
        use aws_smithy_runtime_api::client::connector_metadata::ConnectorMetadata;
        use aws_smithy_runtime_api::client::http::{
            HttpClient, HttpConnectorSettings, SharedHttpClient, SharedHttpConnector,
        };
        use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;

        #[derive(Debug)]
        struct InspectionClient;
        impl HttpClient for InspectionClient {
            fn http_connector(
                &self,
                _: &HttpConnectorSettings,
                _: &RuntimeComponents,
            ) -> SharedHttpConnector {
                panic!("纯配置检查不允许发起网络请求")
            }

            fn connector_metadata(&self) -> Option<ConnectorMetadata> {
                Some(ConnectorMetadata::new("isolated-credential-policy", None))
            }
        }

        let _ = rustls::crypto::ring::default_provider().install_default();
        let provider = RefreshingCredentials {
            spec: AwsAuthSpec {
                mode: AwsAuthMode::AccessKey {
                    access_key_id: "probe-explicit-access-key".into(),
                    secret_access_key: "probe-explicit-secret-key".into(),
                    session_token: None,
                },
                assume_role: Some(AssumeRole {
                    role_arn: "arn:aws:iam::123456789012:role/probe".into(),
                    external_id: None,
                    role_session_name: None,
                }),
                region: "us-east-1".into(),
            },
            cached: tokio::sync::Mutex::new(None),
            sdk_config: tokio::sync::OnceCell::new(),
            http_override: Some(ExplicitCredentialTransport {
                http: SharedHttpClient::new(InspectionClient),
                base: Credentials::new(
                    "probe-explicit-access-key",
                    "probe-explicit-secret-key",
                    None,
                    None,
                    PROVIDER_NAME,
                ),
            }),
        };
        let config = provider.sdk_config().await;
        assert_eq!(
            config
                .http_client()
                .and_then(|client| client.connector_metadata())
                .map(|metadata| metadata.name()),
            Some("isolated-credential-policy".into()),
            "凭据请求必须使用调用方的地址检查传输"
        );
        assert_eq!(
            config.retry_config().map(|retry| retry.max_attempts()),
            Some(1),
            "探测的凭据交换也不能有 SDK 隐式重试"
        );
        let configured = config
            .credentials_provider()
            .expect("显式凭据提供者")
            .provide_credentials()
            .await
            .expect("静态凭据不发网络请求");
        assert_eq!(configured.access_key_id(), "probe-explicit-access-key");
        assert_eq!(configured.secret_access_key(), "probe-explicit-secret-key");
        let process_spec = AwsAuthSpec {
            mode: AwsAuthMode::DefaultChain,
            assume_role: None,
            region: "us-east-1".into(),
        };
        assert!(matches!(
            resolve_with_http_client(&process_spec, SharedHttpClient::new(InspectionClient)),
            Err(CredentialsError::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn role_and_assumed_role_arns_name_the_same_identity() {
        assert_eq!(
            role_identity("arn:aws:iam::123456789012:role/team/Bedrock"),
            Some(("aws", "123456789012", "Bedrock"))
        );
        assert_eq!(
            role_identity("arn:aws:sts::123456789012:assumed-role/Bedrock/aisix-session-1"),
            Some(("aws", "123456789012", "Bedrock"))
        );
        assert_eq!(role_identity("arn:aws:iam::123456789012:user/alice"), None);
        assert_eq!(role_identity("not-an-arn"), None);
    }

    #[test]
    fn debug_never_prints_secrets() {
        let spec = AwsAuthSpec {
            mode: AwsAuthMode::AccessKey {
                access_key_id: "AKIA-visible-id".into(),
                secret_access_key: "very-secret-value".into(),
                session_token: Some("very-secret-token".into()),
            },
            assume_role: None,
            region: "us-east-1".into(),
        };
        let printed = format!("{spec:?}");
        assert!(!printed.contains("very-secret"), "{printed}");
    }
}
