//! Upstream response headers an observability export asked for.
//!
//! An exporter may list `upstream_response_headers`; the values come off the
//! upstream's HTTP response, which only the code that sent the request ever
//! sees — a provider bridge, or a proxy route that talks to its upstream
//! itself. Each of those calls [`note_upstream_response`] right after the
//! response head arrives. The proxy installs one [`ResponseHeaderTap`] per
//! request (carrying the names the request's exporters want) and drains it
//! per attempt; outside such a scope the call is a no-op, which is what a
//! background poll or a gateway-initiated sub-call relies on.
//!
//! The provider's own request id is read off the same head whether or not
//! an exporter asked for any header: it is what a provider's support asks
//! for about a failed call, and an error response carries no body id.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// The longest value recorded per header, in characters.
const MAX_VALUE_CHARS: usize = 256;

/// The values of `names` on `headers`, keyed by name: repeated occurrences
/// joined with `", "` in arrival order, bytes decoded lossily as UTF-8,
/// control characters stripped and the result capped at 256 characters. A
/// header that is absent, or whose value strips to nothing, has no key.
/// Shared by the request-header and response-header captures so both follow
/// one rule.
pub fn capture_header_values(
    headers: &http::HeaderMap,
    names: &[String],
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for name in names {
        let joined = headers
            .get_all(name.as_str())
            .iter()
            .map(|v| String::from_utf8_lossy(v.as_bytes()))
            .collect::<Vec<_>>()
            .join(", ");
        let value: String = joined
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_VALUE_CHARS)
            .collect();
        if !value.is_empty() {
            out.insert(name.clone(), value);
        }
    }
    out
}

/// The headers a provider names its own request id in, in the order they
/// are preferred when a response carries more than one (Azure sends both
/// `x-request-id` and `apim-request-id`).
pub const PROVIDER_REQUEST_ID_HEADERS: &[&str] = &[
    "x-amzn-requestid",
    "x-request-id",
    "request-id",
    "x-ms-request-id",
    "apim-request-id",
    "x-goog-request-id",
    "cf-ray",
];

/// The provider's request id on `headers`, under the value rules of
/// [`capture_header_values`]; empty when none of
/// [`PROVIDER_REQUEST_ID_HEADERS`] carries one.
pub fn provider_request_id(headers: &http::HeaderMap) -> String {
    PROVIDER_REQUEST_ID_HEADERS
        .iter()
        .find_map(|name| {
            let value: String = headers
                .get(*name)?
                .to_str()
                .ok()?
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_VALUE_CHARS)
                .collect();
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_string())
        })
        .unwrap_or_default()
}

/// What one upstream response head left for the request's telemetry.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UpstreamResponseCapture {
    /// The headers the request's exporters asked for.
    pub headers: BTreeMap<String, String>,
    /// See [`provider_request_id`].
    pub request_id: String,
    /// The body of a non-2xx response, as read, when the request has a
    /// full-content export to hand it to.
    pub error_body: Option<bytes::Bytes>,
    /// 流在首个可转译内容前已报告的用量；句柄随本次 attempt 保存，不随 take 换绑。
    pub stream_usage: Option<StreamUsageObserver>,
    /// 真实上游交换的长度观察；与正文导出开关无关。
    pub io: Option<crate::upstream_io::UpstreamIoObserver>,
}

impl UpstreamResponseCapture {
    pub fn is_empty(&self) -> bool {
        self.headers.is_empty()
            && self.request_id.is_empty()
            && self.error_body.is_none()
            && self.stream_usage.is_none()
            && self.io.is_none()
    }
}

/// 仅保存上游已报告的计数与实际服务档位，不保留请求或输出内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedStreamUsage {
    pub stats: crate::UsageStats,
    pub prompt_reported: bool,
    /// 初始 output_tokens 是占位累计值，只有后续用量报告才能结算输出。
    pub completion_reported: bool,
    pub service_tier: Option<crate::ServiceTier>,
}

/// 绑定一次上游响应的共享观测；捕获被移入已结算 attempt 后仍写入原句柄。
#[derive(Debug, Default, Clone)]
pub struct StreamUsageObserver(Arc<Mutex<Option<ObservedStreamUsage>>>);

impl PartialEq for StreamUsageObserver {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for StreamUsageObserver {}

impl StreamUsageObserver {
    pub fn record(&self, mut observed: ObservedStreamUsage) {
        let mut current = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(previous) = current.as_ref() {
            observed.stats = previous.stats.max_fieldwise(&observed.stats);
            observed.prompt_reported |= previous.prompt_reported;
            observed.completion_reported |= previous.completion_reported;
            if observed.service_tier.is_none() {
                observed.service_tier.clone_from(&previous.service_tier);
            }
        }
        *current = Some(observed);
    }

    pub fn get(&self) -> Option<ObservedStreamUsage> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// 同一次事件发布只领取一次；非消费读取用于 handler 被取消时的归属判断。
    pub fn take(&self) -> Option<ObservedStreamUsage> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).take()
    }
}

/// One request's response-header capture: the names its exporters asked
/// for, and what the latest upstream response carried.
#[derive(Default)]
pub struct ResponseHeaderTap {
    names: Vec<String>,
    /// Whether an exporter of this request runs at `content_mode: full`.
    /// Without one no error body is kept, nor read where nothing else needs it.
    keeps_error_bodies: bool,
    request_id: String,
    latest: Mutex<UpstreamResponseCapture>,
    /// Every credential the gateway put on an upstream request for this
    /// caller's request, for [`crate::upstream_error_scrub`]. Kept across
    /// attempts: an error a fallback relays may quote an earlier one.
    sent_credentials: Mutex<Vec<String>>,
    /// See [`note_served_input_tokens`].
    served_input_tokens: Mutex<Option<u32>>,
}

impl std::fmt::Debug for ResponseHeaderTap {
    // By hand: the credentials must not reach a `Debug` rendering.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseHeaderTap")
            .field("names", &self.names)
            .field("keeps_error_bodies", &self.keeps_error_bodies)
            .field("request_id", &self.request_id)
            .finish_non_exhaustive()
    }
}

impl ResponseHeaderTap {
    pub fn new(names: Vec<String>, keeps_error_bodies: bool, request_id: String) -> Self {
        Self {
            names,
            keeps_error_bodies,
            request_id,
            latest: Mutex::default(),
            sent_credentials: Mutex::default(),
            served_input_tokens: Mutex::default(),
        }
    }

    fn record(&self, headers: &http::HeaderMap) {
        let mut current = self.lock();
        current.headers = capture_header_values(headers, &self.names);
        current.request_id = provider_request_id(headers);
        current.error_body = None;
    }

    /// What [`note_served_input_tokens`] last noted and nothing has taken,
    /// left in place. For a reader outside the request's scope (the cancel
    /// path runs from `Drop`).
    pub fn served_input_tokens(&self) -> Option<u32> {
        *self
            .served_input_tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 当前 attempt 的非消费读取；原观察者在 take 后仍更新原句柄。
    pub fn capture(&self) -> UpstreamResponseCapture {
        self.lock().clone()
    }

    pub fn upstream_io_observer(&self) -> Option<crate::upstream_io::UpstreamIoObserver> {
        self.lock().io.clone()
    }

    pub fn stream_usage_observer(&self) -> Option<StreamUsageObserver> {
        self.lock().stream_usage.clone()
    }

    pub fn observed_stream_usage(&self) -> Option<ObservedStreamUsage> {
        self.lock()
            .stream_usage
            .as_ref()
            .and_then(StreamUsageObserver::get)
    }

    /// Move the latest capture out, leaving nothing behind.
    pub fn take(&self) -> UpstreamResponseCapture {
        std::mem::take(&mut *self.lock())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, UpstreamResponseCapture> {
        self.latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

tokio::task_local! {
    static TAP: Arc<ResponseHeaderTap>;
}

/// 在创建流、尚未交还给路由前绑定；仅 poll 时访问“最新响应”会串到另一 attempt。
pub fn stream_usage_observer() -> Option<StreamUsageObserver> {
    TAP.try_with(|tap| {
        tap.lock()
            .stream_usage
            .get_or_insert_with(StreamUsageObserver::default)
            .clone()
    })
    .ok()
}

pub(crate) fn note_upstream_io(observer: &crate::upstream_io::UpstreamIoObserver) {
    let _ = TAP.try_with(|tap| {
        let mut current = tap.lock();
        if let Some(aggregate) = &current.io {
            aggregate.append(observer);
        } else {
            current.io = Some(crate::upstream_io::UpstreamIoObserver::aggregate(observer));
        }
    });
}

/// Install `tap` for everything `fut` does.
pub fn scope<F: std::future::Future>(
    tap: Arc<ResponseHeaderTap>,
    fut: F,
) -> impl std::future::Future<Output = F::Output> {
    TAP.scope(tap, fut)
}

/// Install `tap` for one synchronous call (a polled response body).
pub fn sync_scope<T>(tap: &Arc<ResponseHeaderTap>, f: impl FnOnce() -> T) -> T {
    TAP.sync_scope(tap.clone(), f)
}

/// Record an upstream response head for the current request's exporters.
/// Call it once per upstream HTTP response that answers a dispatched
/// attempt, as soon as the head is in hand — success or failure.
pub fn note_upstream_response(headers: &http::HeaderMap) {
    let _ = TAP.try_with(|tap| tap.record(headers));
}

/// Keep the non-2xx body of the response last noted, for the same attempt's
/// failure row, less the credentials this request sent
/// ([`crate::upstream_error_scrub::scrub_upstream_error_body`]). Shared with
/// `body` unless it held one.
pub fn note_upstream_error_body(body: &bytes::Bytes) {
    let _ = TAP.try_with(|tap| {
        if tap.keeps_error_bodies {
            let body = crate::upstream_error_scrub::scrub_upstream_error_body(body);
            tap.lock().error_body = Some(body);
        }
    });
}

/// Note what the attempt in progress has been served, and billed, so far: an
/// operation the gateway makes in several upstream calls (one Titan
/// embedding per text) notes the input its answered calls reported after
/// each of them, and clears it ([`take_served_input_tokens`]) once the whole
/// operation succeeded. The proxy takes it when the attempt fails, to bill it
/// and to not replay the calls that answered, and reads it
/// ([`ResponseHeaderTap::served_input_tokens`]) when the caller hangs up
/// before the attempt ends, so the row of a request nobody waited for still
/// bills the calls the upstream answered. Outside a request scope it is a
/// no-op.
pub fn note_served_input_tokens(tokens: u32) {
    let _ = TAP.try_with(|tap| {
        *tap.served_input_tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(tokens);
    });
}

/// What [`note_served_input_tokens`] noted and nothing has taken, left in
/// place. The reader for a caller that must not consume it — a request whose
/// caller hung up still needs it for its usage row, and its limit
/// reservation for the tokens it owes. `None` outside a request scope.
pub fn served_input_tokens() -> Option<u32> {
    TAP.try_with(|tap| tap.served_input_tokens()).ok().flatten()
}

/// What [`note_served_input_tokens`] noted since the last take, leaving
/// nothing behind. `None` outside a request scope.
pub fn take_served_input_tokens() -> Option<u32> {
    TAP.try_with(|tap| {
        tap.served_input_tokens
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    })
    .ok()
    .flatten()
}

/// The most credentials one request keeps: a request reaches a handful of
/// upstreams at most (retries, fallbacks, a judge). Past it the oldest goes, so
/// the attempt whose error is relayed always has its own.
const MAX_SENT_CREDENTIALS: usize = 16;

/// Note a credential the gateway is putting on an upstream request, so that an
/// error the upstream answers with cannot hand it to the caller. Call it where
/// the credential goes on the request: a provider key, a minted token, a
/// route's injected secret. Outside a request scope it is a no-op, and there
/// is no caller to hand anything to.
pub fn note_sent_credential(value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    let _ = TAP.try_with(|tap| {
        let mut sent = tap
            .sent_credentials
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(at) = sent.iter().position(|v| v == value) {
            sent.remove(at);
        } else if sent.len() == MAX_SENT_CREDENTIALS {
            sent.remove(0);
        }
        sent.push(value.to_owned());
    });
}

/// A tap for work the request hands to a task of its own (a realtime session
/// after its upgrade): the request's id and the credentials it noted, so that
/// an error read there is scrubbed as one read on the request would be.
pub fn detached() -> Arc<ResponseHeaderTap> {
    let request_id = TAP
        .try_with(|tap| tap.request_id.clone())
        .unwrap_or_default();
    let tap = ResponseHeaderTap::new(Vec::new(), false, request_id);
    *tap.sent_credentials
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = sent_credentials();
    Arc::new(tap)
}

/// Note the credential a configured URL carries in its userinfo. reqwest sends
/// it as `Authorization: Basic base64(user:password)`, the password
/// percent-decoded, so the password is noted as written and decoded, and so is
/// the header's token: an upstream that quotes what it received quotes that.
pub fn note_url_userinfo(url: &str) {
    use base64::Engine as _;
    let Ok(url) = url::Url::parse(url) else {
        return;
    };
    let Some(password) = url.password() else {
        return;
    };
    let decode = |s: &str| {
        percent_encoding::percent_decode_str(s)
            .decode_utf8_lossy()
            .into_owned()
    };
    let (user, decoded) = (decode(url.username()), decode(password));
    note_sent_credential(password);
    note_sent_credential(&decoded);
    note_sent_credential(
        &base64::engine::general_purpose::STANDARD.encode(format!("{user}:{decoded}")),
    );
}

/// What [`note_sent_credential`] noted for the current request.
pub(crate) fn sent_credentials() -> Vec<String> {
    TAP.try_with(|tap| {
        tap.sent_credentials
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    })
    .unwrap_or_default()
}

/// Log an error response whose body could not be read. Names the request
/// itself: the request span is off at the `warn` level this is meant to reach.
pub fn warn_error_body_unread(status: u16, reason: &str) {
    let request_id = TAP
        .try_with(|tap| tap.request_id.clone())
        .unwrap_or_default();
    tracing::warn!(
        status,
        %request_id,
        %reason,
        "failed to read upstream error response body"
    );
}

/// Whether a route that does not otherwise need a non-2xx body should read it.
pub fn wants_upstream_error_body() -> bool {
    TAP.try_with(|tap| tap.keeps_error_bodies).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stream_usage_stays_with_its_capture_across_take_and_detached_attempts() {
        // take 后旧流仍只能写旧句柄；不保存空句柄或复用 latest 都会使断言失败。
        let tap = Arc::new(ResponseHeaderTap::new(Vec::new(), false, "r".into()));
        let (first, first_capture, second, second_capture) = scope(tap.clone(), async {
            let first = stream_usage_observer().unwrap();
            let first_capture = tap.take();
            assert!(!first_capture.is_empty(), "首帧尚未到来也必须保存绑定");
            let second = stream_usage_observer().unwrap();
            let second_capture = tap.take();
            (first, first_capture, second, second_capture)
        })
        .await;
        let reported = |prompt| ObservedStreamUsage {
            stats: crate::UsageStats::with_cache(prompt, 0, 11, 23),
            prompt_reported: true,
            completion_reported: false,
            service_tier: crate::ServiceTier::native(Some("fast".into())),
        };
        first.record(reported(37));
        second.record(reported(53));
        first.record(reported(41));
        let old = first_capture.stream_usage.as_ref().unwrap();
        let new = second_capture.stream_usage.as_ref().unwrap();
        assert_eq!(old.get().unwrap().stats.prompt_tokens, 41);
        assert_eq!(new.get().unwrap().stats.prompt_tokens, 53);
        assert_eq!(old.take().unwrap().stats.prompt_tokens, 41);
        assert!(old.take().is_none(), "同一结算不能重复领取");
        assert_eq!(new.get().unwrap().stats.cache_read_tokens, 23);
        assert!(tap.observed_stream_usage().is_none());
        let child = scope(tap, async { detached() }).await;
        assert!(child.observed_stream_usage().is_none());
        assert!(
            stream_usage_observer().is_none(),
            "没有请求 scope 时不绑定他人的观测"
        );
    }

    /// The value rules the e2e cannot drive through a real HTTP client:
    /// non-UTF-8 bytes, a value that sanitises to nothing, and the cap.
    #[test]
    fn captured_header_values_follow_the_value_rules() {
        let names: Vec<String> = ["x-a", "x-b", "x-c", "x-d", "x-absent"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut h = http::HeaderMap::new();
        h.append("x-a", "one".parse().unwrap());
        h.append("x-a", "two".parse().unwrap());
        h.append(
            "x-b",
            http::HeaderValue::from_bytes(b"caf\xe9\tbar").unwrap(),
        );
        h.append("x-c", http::HeaderValue::from_bytes(b"\t").unwrap());
        h.append("x-d", "y".repeat(300).parse().unwrap());
        h.append("x-other", "not listed".parse().unwrap());
        let got = capture_header_values(&h, &names);
        assert_eq!(got.get("x-a").map(String::as_str), Some("one, two"));
        assert_eq!(got.get("x-b").map(String::as_str), Some("caf\u{fffd}bar"));
        assert!(
            !got.contains_key("x-c"),
            "a value sanitised to nothing has no key"
        );
        assert_eq!(got["x-d"].chars().count(), 256);
        assert!(!got.contains_key("x-absent"));
        assert!(!got.contains_key("x-other"));
        assert_eq!(got.len(), 3);
    }

    #[tokio::test]
    async fn a_response_is_recorded_only_inside_a_scope() {
        let mut h = http::HeaderMap::new();
        h.insert("x-upstream-id", "u-1".parse().unwrap());
        h.insert("request-id", "req_1".parse().unwrap());
        note_upstream_response(&h);

        let tap = Arc::new(ResponseHeaderTap::new(
            vec!["x-upstream-id".into()],
            false,
            String::new(),
        ));
        scope(tap.clone(), async { note_upstream_response(&h) }).await;
        let got = tap.take();
        assert_eq!(
            got.headers.get("x-upstream-id").map(String::as_str),
            Some("u-1")
        );
        assert_eq!(got.request_id, "req_1");
        assert!(tap.take().is_empty(), "take leaves nothing behind");

        // No exporter asked for a header: the request id is still read.
        let idle = Arc::new(ResponseHeaderTap::new(Vec::new(), false, String::new()));
        scope(idle.clone(), async { note_upstream_response(&h) }).await;
        let got = idle.take();
        assert!(got.headers.is_empty());
        assert_eq!(got.request_id, "req_1");
    }

    #[tokio::test]
    async fn served_input_tokens_are_taken_once_and_only_inside_a_scope() {
        note_served_input_tokens(7);
        assert_eq!(take_served_input_tokens(), None);

        let tap = Arc::new(ResponseHeaderTap::new(Vec::new(), false, String::new()));
        let taken = scope(tap, async {
            assert_eq!(take_served_input_tokens(), None);
            note_served_input_tokens(8);
            (take_served_input_tokens(), take_served_input_tokens())
        })
        .await;
        assert_eq!(taken, (Some(8), None));
    }

    #[test]
    fn the_request_id_follows_the_header_preference_order() {
        let mut h = http::HeaderMap::new();
        h.insert("apim-request-id", "apim-1".parse().unwrap());
        assert_eq!(provider_request_id(&h), "apim-1");
        h.insert("x-request-id", "xr-1".parse().unwrap());
        assert_eq!(provider_request_id(&h), "xr-1");
        h.insert("x-request-id", http::HeaderValue::from_static(" "));
        assert_eq!(
            provider_request_id(&h),
            "apim-1",
            "a blank value is skipped"
        );
        assert_eq!(provider_request_id(&http::HeaderMap::new()), "");
    }
}
