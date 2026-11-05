//! The one scrubber an upstream's error passes on its way to a caller, a usage
//! row, an export or a log line.
//!
//! The upstream's message is relayed, because the caller needs the provider's
//! own words to fix a request ("invalid model", a parameter out of range). It
//! must not carry what the gateway knows and the caller does not. That means
//! the credential the gateway sent, which some upstreams quote back
//! ("Incorrect API key provided: …"), and the operator's infrastructure that a
//! provider names in its text. Every provider goes through this one module.
//! A bridge does not scrub on its own.
//!
//! Two passes:
//!
//! 1. **Exact values.** Every credential the gateway puts on an upstream
//!    request is noted for the caller's request
//!    ([`crate::response_tap::note_sent_credential`]). Each one is replaced
//!    wherever it occurs: as sent, JSON-escaped, or with `/` written `\/`.
//!    This pass also runs over the bytes of every non-2xx body the gateway
//!    reads or relays ([`scrub_upstream_error_bytes`]).
//! 2. **Patterns**, on extracted text only ([`scrub_upstream_error_text`]). A
//!    relayed body keeps its bytes apart from pass 1, because a pattern
//!    could match outside a JSON string and break the document. The patterns:
//!    - AWS ARNs, `arn:partition:service:region:account-id:resource`
//!      (<https://docs.aws.amazon.com/IAM/latest/UserGuide/reference-arns.html>),
//!      and bare 12-digit account ids
//!      (<https://docs.aws.amazon.com/accounts/latest/reference/manage-acct-identifiers.html>).
//!    - Google Cloud project ids in the shapes Google's errors carry them:
//!      a resource name `projects/{project}/…`
//!      (<https://cloud.google.com/vertex-ai/docs/reference/rest>), and the
//!      `?project=` link and "in project {number}" of a disabled-API error
//!      (<https://cloud.google.com/apis/design/errors#error_info>).
//!    - The host of a URL that names an internal address: an IP literal, a
//!      single-label name, or a name under a suffix reserved for private use
//!      (`.internal`, `.local` per RFC 6762, `.localhost` per RFC 6761,
//!      `.home.arpa` per RFC 8375, and the `.svc` names of a Kubernetes
//!      cluster). A public host such as a provider's documentation link stays.
//!    - A bearer token (`Bearer <token>`, RFC 6750 §2.1), and keys whose
//!      prefix names them: `sk-` (the OpenAI and Anthropic key shape, as in
//!      the OpenAI 401 fixture in `aisix-provider-openai`), AWS access key
//!      ids `AKIA` / `ASIA`
//!      (<https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_identifiers.html#identifiers-unique-ids>),
//!      and Google OAuth access tokens `ya29.` (the Vertex fixtures in
//!      `aisix-provider-vertex`).
//!
//! What is replaced reads `<redacted>`. Ordinary text passes unchanged: none
//! of these shapes occurs in a message such as "invalid model".

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex};

/// What a scrubbed value is replaced with.
pub const REDACTED: &str = "<redacted>";

/// Suffixes of host names that only resolve inside a private network.
const INTERNAL_HOST_SUFFIXES: &[&str] = &[
    ".internal",
    ".local",
    ".localhost",
    ".localdomain",
    ".home.arpa",
    ".svc",
];

static ARN: LazyLock<Regex> = LazyLock::new(|| re(r#"\barn:[^\s"'<>,;]+"#));
static ACCOUNT_ID: LazyLock<Regex> = LazyLock::new(|| re(r"\b[0-9]{12}\b"));
static GCP_PROJECT: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(\bprojects/|[?&]project=)[^/\s"'?#&,;`]+|(\bin project )[0-9]+\b"#));
static URL_AUTHORITY: LazyLock<Regex> =
    LazyLock::new(|| re(r#"(?i)\b(https?|wss?)://([^/\s"'?#<>\\]+)"#));
static BEARER: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)\b(bearer\s+)([A-Za-z0-9._~+/-]+=*)"));
static PREFIXED_KEY: LazyLock<Regex> = LazyLock::new(|| {
    re(r"\b(?:sk-[A-Za-z0-9_-]{16,}|A[KS]IA[0-9A-Z]{16}\b|ya29\.[A-Za-z0-9_.-]{8,})")
});

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("the scrubber's patterns are fixed and valid")
}

/// An upstream's error text with the credentials this request sent and the
/// infrastructure it names taken out. For a message read out of an error body
/// or an in-band error event; truncate after this, not before, or a cut could
/// leave half a credential that no longer matches.
pub fn scrub_upstream_error_text(text: &str) -> String {
    let mut text = text.to_owned();
    for needle in credential_needles() {
        if text.contains(needle.as_str()) {
            text = text.replace(needle.as_str(), REDACTED);
        }
    }
    let text = ARN.replace_all(&text, REDACTED);
    let text = ACCOUNT_ID.replace_all(&text, REDACTED);
    let text = GCP_PROJECT.replace_all(&text, |c: &Captures<'_>| {
        let lead = c.get(1).or_else(|| c.get(2)).map_or("", |m| m.as_str());
        format!("{lead}{REDACTED}")
    });
    let text = URL_AUTHORITY.replace_all(&text, |c: &Captures<'_>| {
        let authority = &c[2];
        let host_port = authority.rsplit('@').next().unwrap_or(authority);
        if is_internal_host(host_port) {
            format!("{}://{REDACTED}", &c[1])
        } else if authority.contains('@') {
            format!("{}://{REDACTED}@{host_port}", &c[1])
        } else {
            c[0].to_string()
        }
    });
    let text = BEARER.replace_all(&text, |c: &Captures<'_>| {
        let token = &c[2];
        let looks_like_a_token =
            token.len() >= 32 || (token.len() >= 16 && token.bytes().any(|b| b.is_ascii_digit()));
        if looks_like_a_token {
            format!("{}{REDACTED}", &c[1])
        } else {
            c[0].to_string()
        }
    });
    PREFIXED_KEY.replace_all(&text, REDACTED).into_owned()
}

/// `body` with every credential this request sent replaced, and nothing else
/// changed. For the bytes of an upstream's non-2xx body, kept for an export or
/// relayed to the caller as sent.
pub fn scrub_upstream_error_bytes(body: &[u8]) -> Cow<'_, [u8]> {
    let mut out = Cow::Borrowed(body);
    for needle in credential_needles() {
        if let Some(replaced) = replace_bytes(&out, needle.as_bytes(), REDACTED.as_bytes()) {
            out = Cow::Owned(replaced);
        }
    }
    out
}

/// [`scrub_upstream_error_bytes`] for one frame of a 2xx stream relayed as
/// sent, which an upstream may end with an error event. Only a frame that
/// carries an `"error"` key is touched: the rest is the upstream's answer.
pub fn scrub_upstream_error_frame(frame: &[u8]) -> Cow<'_, [u8]> {
    const KEY: &[u8] = b"\"error\"";
    if frame.windows(KEY.len()).any(|w| w == KEY) {
        scrub_upstream_error_bytes(frame)
    } else {
        Cow::Borrowed(frame)
    }
}

/// [`scrub_upstream_error_bytes`] for a shared buffer, which is only copied
/// when it held a credential.
pub fn scrub_upstream_error_body(body: &bytes::Bytes) -> bytes::Bytes {
    match scrub_upstream_error_bytes(body) {
        Cow::Borrowed(_) => body.clone(),
        Cow::Owned(scrubbed) => bytes::Bytes::from(scrubbed),
    }
}

/// Every form a credential sent on this request can take in an error body:
/// as sent, JSON-escaped, and with `/` escaped as JSON allows. Longest first,
/// so a form that contains another is replaced whole.
fn credential_needles() -> Vec<String> {
    let mut needles = Vec::new();
    for value in crate::response_tap::sent_credentials() {
        if let Ok(quoted) = serde_json::to_string(&value) {
            let escaped = &quoted[1..quoted.len() - 1];
            needles.push(escaped.replace('/', "\\/"));
            needles.push(escaped.to_owned());
        }
        needles.push(value);
    }
    needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
    needles.dedup();
    needles
}

fn replace_bytes(hay: &[u8], needle: &[u8], with: &[u8]) -> Option<Vec<u8>> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let mut out: Option<Vec<u8>> = None;
    let (mut i, mut copied) = (0, 0);
    while i + needle.len() <= hay.len() {
        if &hay[i..i + needle.len()] == needle {
            let buf = out.get_or_insert_with(|| Vec::with_capacity(hay.len()));
            buf.extend_from_slice(&hay[copied..i]);
            buf.extend_from_slice(with);
            i += needle.len();
            copied = i;
        } else {
            i += 1;
        }
    }
    out.map(|mut buf| {
        buf.extend_from_slice(&hay[copied..]);
        buf
    })
}

/// Whether `host_port` (an authority with no userinfo) names an address only
/// a private network reaches.
fn is_internal_host(host_port: &str) -> bool {
    let host = match host_port.strip_prefix('[') {
        // A bracketed IPv6 literal is an address, whatever it is.
        Some(_) => return true,
        None => host_port
            .rsplit_once(':')
            .map_or(host_port, |(host, port)| {
                if port.bytes().all(|b| b.is_ascii_digit()) {
                    host
                } else {
                    host_port
                }
            }),
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host.parse::<std::net::Ipv4Addr>().is_ok()
        || !host.contains('.')
        || INTERNAL_HOST_SUFFIXES.iter().any(|s| host.ends_with(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_sent<T>(credentials: &[&str], f: impl FnOnce() -> T) -> T {
        let tap = std::sync::Arc::new(crate::response_tap::ResponseHeaderTap::new(
            Vec::new(),
            false,
            String::new(),
        ));
        crate::response_tap::sync_scope(&tap, || {
            for c in credentials {
                crate::response_tap::note_sent_credential(c);
            }
            f()
        })
    }

    #[test]
    fn a_credential_sent_on_this_request_is_taken_out_whatever_its_shape() {
        const KEY: &str = "plainkey0Z9Y8X7W6V5U4T3S";
        let echoed = format!("Upstream saw Authorization: Bearer {KEY}, x-api-key: {KEY}.");
        let got = with_sent(&[KEY], || scrub_upstream_error_text(&echoed));
        assert_eq!(
            got,
            "Upstream saw Authorization: Bearer <redacted>, x-api-key: <redacted>."
        );

        // The same key in bytes, written by a JSON encoder that escapes `/`.
        const SLASHED: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEX";
        let body = br#"{"error":{"message":"bad key wJalrXUtnFEMI\/K7MDENG\/bPxRfiCYEX"}}"#;
        let got = with_sent(&[SLASHED], || scrub_upstream_error_bytes(body).into_owned());
        assert_eq!(got, br#"{"error":{"message":"bad key <redacted>"}}"#);

        // Nothing sent: the bytes are the upstream's.
        let untouched = with_sent(&[], || scrub_upstream_error_bytes(body));
        assert!(matches!(untouched, Cow::Borrowed(_)));
    }

    /// Every reader of an upstream's error text gets it scrubbed: the body's
    /// message, a parsed envelope's fields, and an in-band error event.
    #[test]
    fn every_shared_reader_of_an_error_hands_out_scrubbed_text() {
        const KEY: &str = "plainkey0Z9Y8X7W6V5U4T3S";
        let body = format!(
            r#"{{"error":{{"message":"Incorrect API key provided: {KEY}","type":"invalid_request_error","code":"x-api-key: {KEY}"}}}}"#
        );
        with_sent(&[KEY], || {
            assert_eq!(
                crate::upstream_error_message(body.as_bytes()),
                "Incorrect API key provided: <redacted>"
            );
            assert_eq!(
                crate::upstream_error_message(format!("plain {KEY}").as_bytes()),
                "plain <redacted>"
            );
            let parse = |b: &[u8]| {
                let v: serde_json::Value = serde_json::from_slice(b).ok()?;
                Some(crate::UpstreamErrorView {
                    message: v["error"]["message"].as_str().map(str::to_string),
                    kind: v["error"]["type"].as_str().map(str::to_string),
                    code: v["error"]["code"].as_str().map(str::to_string),
                    param: None,
                })
            };
            let err = crate::upstream_status_of_body(
                401,
                body.as_bytes(),
                None,
                crate::UpstreamWire::OpenAI,
                parse,
            );
            let crate::BridgeError::UpstreamStatus {
                status,
                message,
                parsed,
                ..
            } = err
            else {
                panic!("an upstream status");
            };
            assert_eq!(status, 401);
            assert_eq!(message, "Incorrect API key provided: <redacted>");
            let view = parsed.expect("parsed");
            assert_eq!(view.code.as_deref(), Some("x-api-key: <redacted>"));
            assert_eq!(view.kind.as_deref(), Some("invalid_request_error"));

            let in_band = crate::capture_in_band_error(&body, crate::UpstreamWire::OpenAI)
                .expect("an in-band error");
            assert!(!in_band.to_string().contains(KEY), "{in_band}");
            assert!(in_band.to_string().contains("Incorrect API key provided"));
        });
    }

    /// Credentials the gateway sends without naming them as one: a URL's
    /// userinfo, which goes out as `Basic`, and a `default_headers` value
    /// in a slot or under a name that says it is a credential.
    #[test]
    fn userinfo_and_credential_headers_are_noted_as_sent() {
        use base64::Engine as _;
        let basic = base64::engine::general_purpose::STANDARD.encode("svc-user:S3cret@Passw0rd99");
        let overrides = aisix_core::models::RequestOverrides {
            default_headers: [
                ("ocp-apim-subscription-key", "apimSub0123456789abcdef"),
                ("x-api-key", "Bearer vendorTok0123456789"),
                ("x-tenant", "tenant-acme-production"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            ..Default::default()
        };
        let got = with_sent(&[], || {
            crate::note_url_userinfo("http://svc-user:S3cret%40Passw0rd99@api.internal/v1");
            crate::resolve_default_headers(&crate::UpstreamHeaderContext::from_overrides(Some(
                &overrides,
            )));
            scrub_upstream_error_text(&format!(
                "saw Basic {basic}; password S3cret@Passw0rd99; sub apimSub0123456789abcdef; \
                 token vendorTok0123456789; tenant tenant-acme-production"
            ))
        });
        assert_eq!(
            got,
            "saw Basic <redacted>; password <redacted>; sub <redacted>; token <redacted>; \
             tenant tenant-acme-production"
        );
    }

    #[test]
    fn the_newest_credentials_are_the_ones_kept() {
        let first = "firstKey0123456789";
        let newest = "newestKey0123456789";
        let got = with_sent(&[], || {
            crate::response_tap::note_sent_credential(first);
            for i in 0..40 {
                crate::response_tap::note_sent_credential(&format!("fallbackKey{i:04}xyz"));
            }
            crate::response_tap::note_sent_credential(newest);
            scrub_upstream_error_text(&format!("bad key {newest}"))
        });
        assert_eq!(got, "bad key <redacted>");
    }

    #[test]
    fn infrastructure_is_taken_out_and_the_diagnostic_stays() {
        for (raw, want) in [
            (
                "User: arn:aws:iam::123456789012:user/x is not authorized",
                "User: <redacted> is not authorized",
            ),
            (
                "role \"arn:aws:sts::123456789012:assumed-role/r/s\", retry",
                "role \"<redacted>\", retry",
            ),
            (
                "Too many requests for account 123456789012",
                "Too many requests for account <redacted>",
            ),
            (
                "Publisher Model `projects/acme-prod-42/locations/us-central1/publishers/google/models/gemini-x` was not found",
                "Publisher Model `projects/<redacted>/locations/us-central1/publishers/google/models/gemini-x` was not found",
            ),
            (
                "API has not been used in project 31415926 before or it is disabled. Enable it by visiting https://console.developers.google.com/apis/api/aiplatform.googleapis.com/overview?project=31415926 then retry.",
                "API has not been used in project <redacted> before or it is disabled. Enable it by visiting https://console.developers.google.com/apis/api/aiplatform.googleapis.com/overview?project=<redacted> then retry.",
            ),
            (
                "upstream http://10.0.3.7:8000/v1/chat refused",
                "upstream http://<redacted>/v1/chat refused",
            ),
            (
                "connect to https://vllm.ml.svc:443 failed; see http://inference/health",
                "connect to https://<redacted> failed; see http://<redacted>/health",
            ),
            (
                "Invalid header Authorization: Bearer eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiIxIn0.c2ln",
                "Invalid header Authorization: Bearer <redacted>",
            ),
            (
                "Incorrect API key provided: sk-proj-AbCdEf0123456789xyz.",
                "Incorrect API key provided: <redacted>.",
            ),
            (
                "The security token AKIAIOSFODNN7EXAMPLE is invalid",
                "The security token <redacted> is invalid",
            ),
            ("token ya29.a0AfH6SMBx expired", "token <redacted> expired"),
        ] {
            assert_eq!(with_sent(&[], || scrub_upstream_error_text(raw)), want);
        }
        for kept in [
            "The model `gpt-9` does not exist or you do not have access to it.",
            "Invalid 'temperature': decimal above maximum value.",
            "Incorrect API key provided: sk-inval***c66a. You can find your API key at https://platform.openai.com/account/api-keys.",
            "Bearer authentication required",
            "max_tokens: 123456789012345 exceeds",
            "id 123456789012abc",
            "warn: learn:more about it",
            "Input is too long for requested model.",
            "this project is not configured",
            "the model is not enabled in project settings",
            "模型不存在，请检查模型代码。",
        ] {
            assert_eq!(with_sent(&[], || scrub_upstream_error_text(kept)), kept);
        }
    }
}
