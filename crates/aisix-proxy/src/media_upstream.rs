//! 原生媒体请求与有界 SSE 旁路观察；客户端字节与记账事实各走各的通道。

use aisix_gateway::{BridgeError, UsageStats};
use axum::body::Bytes;
use axum::http::{header, HeaderMap};
use serde_json::Value;

/// 只保留一个受限帧；超限后跳过至空行，下一帧仍可报告终态与数量。
/// 终态名称和 usage 字段来自各媒体端点的官方流事件契约。
pub(crate) struct UsageObserver {
    terminal: &'static str,
    parse_usage: fn(&Value) -> Option<UsageStats>,
    frame: Vec<u8>,
    line: Vec<u8>,
    line_len: usize,
    frame_len: usize,
    skipped: bool,
    after_cr: bool,
    in_string: bool,
    escaped: bool,
    binary_string: bool,
    string_token: Vec<u8>,
    previous: u8,
    last_string: Option<String>,
    pub(crate) completed: bool,
    pub(crate) usage: Option<UsageStats>,
    pub(crate) text: String,
    pub(crate) text_cap: usize,
    text_truncated: bool,
    summary: Option<String>,
}

impl UsageObserver {
    pub(crate) fn new(
        terminal: &'static str,
        parse_usage: fn(&Value) -> Option<UsageStats>,
    ) -> Self {
        Self {
            terminal,
            parse_usage,
            frame: Vec::new(),
            line: Vec::new(),
            line_len: 0,
            frame_len: 0,
            skipped: false,
            after_cr: false,
            in_string: false,
            escaped: false,
            binary_string: false,
            string_token: Vec::new(),
            previous: 0,
            last_string: None,
            completed: false,
            usage: None,
            text: String::new(),
            text_cap: 0,
            text_truncated: false,
            summary: None,
        }
    }

    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.after_cr {
                self.after_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.finish_line();
                self.after_cr = byte == b'\r';
                continue;
            }
            self.line_len = self.line_len.saturating_add(1);
            // 二进制字符串只在观察副本中省略；完成帧的巨大图片不能吞掉后面的 usage。
            // https://developers.openai.com/api/reference/resources/images/generation-streaming-events
            if !self.keep_metadata_byte(byte) {
                continue;
            }
            self.frame_len = self.frame_len.saturating_add(1);
            if self.frame_len > aisix_gateway::sse::MAX_SSE_FRAME_BYTES {
                self.skipped = true;
                self.frame.clear();
                self.line.clear();
            } else if !self.skipped {
                self.line.push(byte);
            }
        }
    }

    fn keep_metadata_byte(&mut self, byte: u8) -> bool {
        if self.in_string {
            let keep = !self.binary_string || (!self.escaped && byte == b'"');
            if self.string_token.len() < 128 {
                self.string_token.push(byte);
            }
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == b'"' {
                self.in_string = false;
                self.last_string = serde_json::from_slice::<String>(&self.string_token).ok();
                self.previous = b'"';
            }
            return keep;
        }
        if byte == b'"' {
            self.binary_string = self.previous == b':'
                && self
                    .last_string
                    .as_deref()
                    .is_some_and(|key| matches!(key, "b64_json" | "audio"));
            self.in_string = true;
            self.escaped = false;
            self.string_token.clear();
            self.string_token.push(byte);
        } else if !byte.is_ascii_whitespace() {
            if byte != b':' {
                self.last_string = None;
            }
            self.previous = byte;
        }
        true
    }

    fn finish_line(&mut self) {
        if self.line_len == 0 {
            if !self.skipped && !self.completed {
                let fields = aisix_gateway::sse::parse_frame(&self.frame);
                if let Some(data) = fields.data {
                    if let Ok(mut value) = serde_json::from_slice::<Value>(&data) {
                        if let Some(text) = value.get("revised_prompt").and_then(Value::as_str) {
                            let remaining = self.text_cap.saturating_sub(self.text.len());
                            let mut end = text.len().min(remaining);
                            while !text.is_char_boundary(end) {
                                end -= 1;
                            }
                            self.text_truncated |= end < text.len();
                            self.text.push_str(&text[..end]);
                        }
                        if value.get("type").and_then(Value::as_str) == Some(self.terminal) {
                            self.usage = (self.parse_usage)(&value);
                            self.completed = true;
                            if self.terminal.starts_with("image_") {
                                if let Some(object) = value.as_object_mut() {
                                    object.remove("b64_json");
                                }
                                self.summary = Some(value.to_string());
                            }
                        }
                    }
                }
            }
            self.frame.clear();
            self.frame_len = 0;
            self.skipped = false;
            self.in_string = false;
            self.escaped = false;
            self.binary_string = false;
            self.previous = 0;
            self.last_string = None;
            self.string_token.clear();
        } else if !self.skipped {
            self.frame.append(&mut self.line);
            self.frame.push(b'\n');
            self.frame_len = self.frame_len.saturating_add(1);
        }
        self.line.clear();
        self.line_len = 0;
    }
}

pub(crate) fn event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            ct.split(';')
                .next()
                .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
        })
}

/// 原生 OpenAI 媒体字段保持开放；只改模型别名和操作员明确配置的覆盖。
pub(crate) fn json_body(
    body: &Value,
    ctx: &aisix_gateway::BridgeContext,
) -> Result<Bytes, crate::error::ProxyError> {
    let model = crate::dispatch::require_upstream_model(&ctx.model)?;
    // 只替换模型时借用原请求，避免大段语音输入在序列化前整份复制。
    if let Some(fields) = crate::util::model_only_fields(body, &ctx.provider_key, None) {
        return crate::util::outbound_json(&crate::util::ModelRenamed { fields, model });
    }
    let mut outbound = body.clone();
    if let Some(object) = outbound.as_object_mut() {
        object.insert("model".into(), Value::String(model.to_string()));
    }
    if let Some(overrides) = ctx.provider_key.request.as_ref() {
        aisix_provider_openai::overrides::validate_content_safe_request_overrides(overrides)
            .map_err(|message| {
                crate::error::ProxyError::Bridge(BridgeError::InvalidUpstreamConfig(message))
            })?;
        aisix_provider_openai::overrides::apply_param_renames(
            &mut outbound,
            &overrides.param_renames,
        );
        if let Some(constraints) = &overrides.param_constraints {
            aisix_provider_openai::overrides::apply_param_constraints(&mut outbound, constraints);
        }
        aisix_provider_openai::overrides::apply_default_body_fields(
            &mut outbound,
            &overrides.default_body_fields,
        );
    }
    crate::util::outbound_json(&outbound)
}

/// 路由、鉴权和客户端头来自实际目标，表单边界最后由 reqwest 生成。
pub(crate) async fn request(
    ctx: &aisix_gateway::BridgeContext,
    path: &str,
    json: Option<&Value>,
) -> Result<reqwest::RequestBuilder, crate::error::ProxyError> {
    let multipart = json.is_none();
    let (url, mut headers) = if ctx.provider_key.adapter == Some(aisix_core::Adapter::AzureOpenai) {
        aisix_provider_azure_openai::media_request_parts(ctx, path, multipart)
            .await
            .map_err(crate::error::ProxyError::Bridge)?
    } else {
        let base = crate::dispatch::resolve_base_url(&ctx.provider_key)?;
        let resolved = if path == "/audio/transcriptions"
            && aisix_provider_openai::cohere::is_cohere(&ctx.provider_key.provider)
        {
            format!(
                "{}{}/audio/transcriptions",
                aisix_provider_openai::cohere::api_root(&base),
                aisix_provider_openai::cohere::COMPATIBILITY_PATH
            )
        } else {
            crate::dispatch::build_openai_url(&base, path)
        };
        let url = aisix_gateway::url_cache::EndpointUrl::Unparsed(resolved);
        let key = crate::dispatch::require_api_key(&ctx.provider_key, &ctx.model)?;
        let mut headers = HeaderMap::new();
        let invalid = |error| {
            crate::error::ProxyError::Bridge(BridgeError::Config(format!(
                "invalid media request header: {error}"
            )))
        };
        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Bearer {key}")).map_err(invalid)?,
        );
        headers.insert(
            header::HeaderName::from_static("x-aisix-request-id"),
            header::HeaderValue::from_str(&ctx.request_id).map_err(invalid)?,
        );
        if !multipart {
            headers.insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            );
        }
        aisix_gateway::apply_request_headers(&mut headers, &ctx.header_ctx());
        (url, headers)
    };
    headers.remove(header::CONTENT_LENGTH);
    if multipart {
        headers.remove(header::CONTENT_TYPE);
    }
    let client = crate::http_client::client_for(ctx.provider_key.upstream_connection().as_ref());
    let mut builder = url.post_on(&client).headers(headers);
    if let Some(body) = json {
        builder = builder.body(json_body(body, ctx)?);
    }
    Ok(builder)
}

#[derive(Default)]
struct ImageEnd {
    usage: Option<UsageStats>,
    completed: bool,
    failure: Option<BridgeError>,
    refusal: Option<crate::error::ProxyError>,
    hits: Vec<aisix_core::GuardrailMonitorHit>,
    redactions: crate::redact::RedactionCounts,
    content: Option<aisix_obs::CapturedContent>,
    first_frame: Option<std::time::Instant>,
}

struct ImageGuard {
    end: ImageEnd,
    finish: Option<Box<dyn FnOnce(ImageEnd) + Send>>,
}
impl Drop for ImageGuard {
    fn drop(&mut self) {
        if let Some(finish) = self.finish.take() {
            finish(std::mem::take(&mut self.end));
        }
    }
}

/// 两条图像路由的原生流共用终态结算；模型限额同时覆盖请求入口和实际目标。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn image_response(
    state: &crate::ProxyState,
    snapshot: &aisix_core::AisixSnapshot,
    auth: &crate::auth::AuthenticatedKey,
    client: &crate::client_ip::ClientContext,
    endpoint: &'static str,
    surface: crate::operation::Surface,
    request_id: &str,
    requested_model: &str,
    model_id: &str,
    model: std::sync::Arc<aisix_core::Model>,
    provider: &str,
    pk_id: &str,
    upstream_model: &str,
    applied: Vec<aisix_core::AppliedGuardrail>,
    chain: aisix_guardrails::GuardrailChain,
    audit: crate::usage_attr::GuardrailAudit,
    redactions: crate::redact::RedactionCounts,
    monitor_hits: Vec<aisix_core::GuardrailMonitorHit>,
    response: reqwest::Response,
    reservation: aisix_ratelimit::MultiReservation,
    in_flight: crate::health::InFlightGuard,
    deferred: Option<crate::attempt::DeferredAttempt>,
    started: std::time::Instant,
    attempt_started: std::time::Instant,
    routing: &crate::attempt::RoutingTelemetry,
    captured_prompt: Option<String>,
    content_cap: Option<u32>,
    input_capture_safe: bool,
    stream_budget: Option<std::time::Duration>,
    request_budget: Option<std::time::Duration>,
) -> axum::response::Response {
    use aisix_guardrails::Guardrail as _;
    use axum::response::IntoResponse as _;
    use futures::StreamExt as _;
    let pk = crate::usage_attr::ResolvedPk::resolve(snapshot, pk_id);
    crate::attribution::defer_access_log(
        crate::attribution::PendingAccessLog::new(
            "POST",
            endpoint,
            request_id,
            &auth.entry.id,
            started,
        )
        .with_model(provider, requested_model)
        .with_routing(routing),
        crate::request_metrics::PendingRequestMetrics::at_stream_end(
            state,
            endpoint,
            crate::request_metrics::Caller::new(auth),
            crate::request_metrics::Upstream {
                provider,
                model: requested_model,
                upstream_model,
                pk: pk.labels(),
                stream: true,
                is_fallback: routing.fallback_count() > 0,
            },
            started,
        ),
    );
    crate::usage_attr::emit_failed_attempts(
        state,
        snapshot,
        surface,
        request_id,
        requested_model,
        &auth.entry.id,
        client,
        &applied,
        &routing.attempts,
        false,
        false,
        monitor_hits.clone(),
        redactions.clone(),
        &audit,
    );
    let post_stream_keys = reservation.keys();
    let hold = reservation.into_stream_hold();
    let state = state.clone();
    let client = client.clone();
    let request_id = request_id.to_string();
    let requested_model = requested_model.to_string();
    let model_id = model_id.to_string();
    let provider = provider.to_string();
    let pk_id = pk_id.to_string();
    let upstream_model = upstream_model.to_string();
    let api_key_id = auth.entry.id.clone();
    let winner = routing.winner().cloned();
    let review_model = requested_model.clone();
    let finish = Box::new(move |mut end: ImageEnd| {
        let failure = end
            .failure
            .as_ref()
            .map(crate::attempt::StreamFailure::from_bridge);
        let target = crate::stream_timeout::finish_bridge_stream(
            &state.health,
            &state.runtime_status,
            &model.display_name,
            &model_id,
            model.cooldown.as_ref(),
            end.failure.take(),
            end.completed,
            200,
            attempt_started,
        );
        if let Some(deferred) = &deferred {
            deferred.finish(
                &state,
                target.status,
                target.status < 400 && target.error_class.is_empty(),
            );
        }
        let usage = end.usage.unwrap_or_default();
        state.limiter.add_tokens_post_stream_all(
            &post_stream_keys,
            u64::from(usage.prompt_tokens) + u64::from(usage.completion_tokens),
        );
        let status = end
            .refusal
            .as_ref()
            .map_or(target.status, |error| error.status().as_u16());
        let snap = state.snapshot.load();
        let pk = crate::usage_attr::ResolvedPk::resolve(&snap, &pk_id);
        let mut hits = monitor_hits;
        hits.extend(end.hits);
        let mut counts = redactions;
        crate::redact::merge_counts(&mut counts, end.redactions);
        let winner = winner.map(|mut winner| {
            winner.latency_ms = crate::attempt::ms_since(attempt_started);
            winner
        });
        if let Some(first) = end.first_frame {
            let caller = crate::request_metrics::Caller::from_api_key_id(&snap, &api_key_id);
            crate::request_metrics::record_stream_ttft(
                &state,
                endpoint,
                caller.as_caller(),
                crate::request_metrics::Upstream {
                    provider: &provider,
                    model: &requested_model,
                    upstream_model: &upstream_model,
                    pk: pk.labels(),
                    stream: true,
                    ..Default::default()
                },
                status,
                first.saturating_duration_since(attempt_started),
                first.saturating_duration_since(started),
            );
        }
        crate::images::emit_usage_event(
            &state,
            &snap,
            &pk,
            endpoint,
            surface,
            &request_id,
            &model_id,
            &requested_model,
            &api_key_id,
            &provider,
            &upstream_model,
            &applied,
            status,
            started.elapsed(),
            &usage,
            &client,
            counts,
            hits,
            end.refusal.is_some(),
            end.content.as_ref(),
            &audit,
            true,
            winner.as_ref(),
            failure.as_ref(),
        );
        drop((hold, in_flight));
    });
    let terminal = if endpoint == "/v1/images/edits" {
        "image_edit.completed"
    } else {
        "image_generation.completed"
    };
    let mut observer = UsageObserver::new(terminal, crate::images::image_usage_stats);
    observer.text_cap = chain
        .stream_output_policy()
        .hold_cap()
        .map_or(content_cap.unwrap_or(0) as usize, |(cap, _)| {
            cap.saturating_mul(crate::held_content::RAW_HOLD_FACTOR)
        });
    let chain = std::sync::Arc::new(chain);
    let holding = chain
        .stream_output_policy()
        .hold_cap()
        .filter(|_| aisix_guardrails::Guardrail::runs_on_output(&*chain));
    let headers = response.headers().clone();
    let upstream =
        crate::stream_timeout::with_read_timeout_bytes(response.bytes_stream(), stream_budget);
    let mut upstream = Box::pin(upstream);
    let mut prefix = Vec::new();
    let mut released = false;
    let mut seed = ImageEnd::default();
    let mut finish: Option<Box<dyn FnOnce(ImageEnd) + Send>> = Some(finish);
    if let Some((cap, fail_open)) = holding {
        let mut guard = ImageGuard {
            end: ImageEnd::default(),
            finish: finish.take(),
        };
        let mut held = crate::held_content::HeldBuffer::default();
        let mut exceeded = false;
        let deadline = request_budget.map(|budget| attempt_started + budget);
        loop {
            let next = if let Some(deadline) = deadline {
                match tokio::time::timeout_at(
                    tokio::time::Instant::from_std(deadline),
                    upstream.next(),
                )
                .await
                {
                    Ok(next) => next,
                    Err(_) => {
                        guard.end.failure = Some(BridgeError::Timeout {
                            elapsed_ms: attempt_started.elapsed().as_millis() as u64,
                            cause: "held image stream".into(),
                        });
                        break;
                    }
                }
            } else {
                upstream.next().await
            };
            let Some(item) = next else { break };
            match item {
                Ok(bytes) => {
                    guard
                        .end
                        .first_frame
                        .get_or_insert_with(std::time::Instant::now);
                    let before = observer.text.len();
                    observer.feed(&bytes);
                    guard.end.usage = observer.usage.clone();
                    guard.end.completed = observer.completed;
                    if !exceeded {
                        held.hold(observer.text.len().saturating_sub(before), bytes.len());
                        prefix.extend_from_slice(&bytes);
                        if held.exceeds(cap) {
                            exceeded = true;
                            if fail_open {
                                crate::guardrail_stream::release_output_buffer_unscanned(&*chain);
                                released = true;
                                break;
                            }
                            prefix.clear();
                            aisix_guardrails::Guardrail::record_output_buffer_exceeded(&*chain);
                        }
                    }
                    if observer.completed {
                        break;
                    }
                }
                Err(error) => {
                    guard.end.failure = Some(error.into_bridge(attempt_started));
                    break;
                }
            }
        }
        if released {
            finish = guard.finish.take();
            seed = std::mem::take(&mut guard.end);
        } else {
            if !observer.completed && guard.end.failure.is_none() {
                guard.end.failure = Some(BridgeError::StreamAborted);
            }
            if exceeded {
                guard.end.refusal = Some(crate::error::guardrail_block_error(
                    "response",
                    None,
                    Some("output_buffer_exceeded"),
                ));
            } else if guard.end.failure.is_none() {
                let mut splitter = aisix_gateway::sse::SseFrameSplitter::new(usize::MAX);
                splitter.push(&prefix);
                let mut frames = Vec::new();
                let mut values = Vec::new();
                let mut positions = Vec::new();
                while let Ok(Some(frame)) = splitter.next_frame() {
                    if let Some(data) = aisix_gateway::sse::parse_frame(&frame).data {
                        if let Ok(value) = serde_json::from_slice::<Value>(&data) {
                            positions.push(frames.len());
                            values.push(value);
                        }
                    }
                    frames.push(frame);
                }
                let mut document = serde_json::json!({"data":values});
                let original = document.clone();
                let review = crate::images::review_image_output(
                    &chain,
                    &review_model,
                    &mut document,
                    &mut guard.end.redactions,
                    &mut guard.end.hits,
                )
                .await;
                guard.end.refusal = review.refusal;
                if document != original {
                    for (position, value) in positions
                        .into_iter()
                        .zip(document["data"].as_array().into_iter().flatten())
                    {
                        let ranges = aisix_gateway::sse::data_ranges(&frames[position]);
                        if let Ok(serialized) = serde_json::to_vec(value) {
                            for (index, range) in ranges.into_iter().enumerate().rev() {
                                frames[position]
                                    .splice(
                                        range,
                                        if index == 0 {
                                            serialized.as_slice()
                                        } else {
                                            &[]
                                        }
                                        .iter()
                                        .copied(),
                                    )
                                    .for_each(drop);
                            }
                        }
                    }
                    prefix = frames.concat();
                }
                if input_capture_safe && review.capture_safe {
                    if let (Some(prompt), Some(cap)) = (&captured_prompt, content_cap) {
                        guard.end.content = Some(aisix_obs::CapturedContent::new(
                            prompt,
                            &crate::images::captured_image_response(&document),
                            cap as usize,
                        ));
                    }
                }
            }
            let response = if let Some(refusal) = guard.end.refusal.as_ref() {
                // 错误对象留给结算，响应通过共享协议渲染。
                (refusal.status(), axum::Json(refusal.envelope())).into_response()
            } else if let Some(error) = guard.end.failure.as_ref() {
                let error = crate::attempt::StreamFailure::from_bridge(error);
                (
                    axum::http::StatusCode::from_u16(error.status)
                        .unwrap_or(axum::http::StatusCode::BAD_GATEWAY),
                    axum::Json(crate::error::ErrorEnvelope::new(
                        "upstream stream ended before completion",
                        "api_error",
                    )),
                )
                    .into_response()
            } else {
                let mut response = axum::response::Response::new(axum::body::Body::from(prefix));
                if let Some(content_type) = headers.get(header::CONTENT_TYPE) {
                    response
                        .headers_mut()
                        .insert(header::CONTENT_TYPE, content_type.clone());
                }
                response
            };
            drop(guard);
            return response;
        }
    }
    let stream = crate::request_id::in_request_span(async_stream::stream! {
        let mut guard=ImageGuard {end:seed,finish};
        if !prefix.is_empty() {
            crate::attribution::note_upstream_first_frame();
            guard.end.usage=observer.usage.clone(); guard.end.completed=observer.completed;
            yield Ok::<_,crate::stream_timeout::RawStreamError>(Bytes::from(prefix));
        }
        while let Some(item)=upstream.next().await {
            match &item {
                Ok(bytes) => {
                    if guard.end.first_frame.is_none() {
                        guard.end.first_frame=Some(std::time::Instant::now());
                        crate::attribution::note_upstream_first_frame();
                    }
                    let was_completed = observer.completed;
                    observer.feed(bytes);
                    guard.end.usage=observer.usage.clone(); guard.end.completed=observer.completed;
                    if observer.completed && !was_completed && !released && aisix_guardrails::Guardrail::runs_on_output(&*chain) {
                        let mut document=serde_json::json!({"data":[{"revised_prompt":observer.text.clone()}]});
                        let review=crate::images::review_image_output(&chain, &review_model, &mut document, &mut guard.end.redactions,&mut guard.end.hits).await;
                        if review.refusal.is_some() { guard.end.refusal=review.refusal; }
                        if observer.text_truncated {crate::guardrail_stream::release_output_buffer_unscanned(&*chain);}
                        if input_capture_safe && review.capture_safe && !observer.text_truncated {
                            if let (Some(prompt),Some(cap),Some(summary))=(&captured_prompt,content_cap,&observer.summary) {
                                guard.end.content=Some(aisix_obs::CapturedContent::new(prompt,summary,cap as usize));
                            }
                        }
                    } else if observer.completed && !was_completed && !released && input_capture_safe {
                        if let (Some(prompt),Some(cap),Some(summary))=(&captured_prompt,content_cap,&observer.summary) {
                            guard.end.content=Some(aisix_obs::CapturedContent::new(prompt,summary,cap as usize));
                        }
                    }
                }
                Err(error) => {
                    if !guard.end.completed { guard.end.failure=Some(error.failure().bridge_error()); }
                }
            }
            let failed=item.is_err();
            yield item;
            if failed { break; }
        }
        if !guard.end.completed && guard.end.failure.is_none() { guard.end.failure=Some(BridgeError::StreamAborted); }
    });
    let mut response = axum::response::Response::new(axum::body::Body::from_stream(
        crate::sse_keepalive::with_heartbeat(Box::pin(stream), crate::sse_keepalive::interval()),
    ));
    if let Some(content_type) = headers.get(header::CONTENT_TYPE) {
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type.clone());
    }
    response
}

#[cfg(test)]
mod tests {
    use super::json_body;
    use aisix_gateway::BridgeContext;
    use serde_json::{json, Value};
    use std::sync::Arc;

    fn context(request: Value) -> BridgeContext {
        BridgeContext::new(
            "media-body",
            Arc::new(
                serde_json::from_value(json!({
                    "display_name":"speaker", "provider":"openai",
                    "model_name":"upstream-tts", "provider_key_id":"pk"
                }))
                .unwrap(),
            ),
            Arc::new(
                serde_json::from_value(json!({
                    "display_name":"media", "api_key":"test-key", "request":request
                }))
                .unwrap(),
            ),
        )
    }

    #[test]
    fn media_json_body_keeps_all_fields_with_only_model_or_header_changes() {
        let body: Value = serde_json::from_str(
            r#"{"custom":{"model":"nested-model"},"input":"quote: \"; backslash: \\; newline: \n; Unicode: 猫","model":"speaker","voice":{"id":"voice-custom"}}"#,
        )
        .unwrap();
        let expected = r#"{"custom":{"model":"nested-model"},"input":"quote: \"; backslash: \\; newline: \n; Unicode: 猫","model":"upstream-tts","voice":{"id":"voice-custom"}}"#;
        for request in [
            Value::Null,
            json!({}),
            json!({"default_headers":{"x-custom":"custom-value"}}),
            json!({"forward_client_headers":["x-request-purpose"]}),
        ] {
            let outbound = json_body(&body, &context(request)).unwrap();
            assert_eq!(outbound.as_ref(), expected.as_bytes());
            assert_eq!(body["model"], "speaker");
        }
    }

    #[test]
    fn media_json_body_applies_renames_constraints_and_defaults_without_losing_custom_fields() {
        let body = json!({
            "model":"speaker", "input":"hello", "voice":"alloy",
            "custom":{"model":"nested-model"}, "temperature":0.9,
            "source_knob":"from-client", "target_knob":"old",
            "existing_option":"client"
        });
        let ctx = context(json!({
            "param_renames":{"source_knob":"target_knob"},
            "param_constraints":{"temperature_max":0.2},
            "default_body_fields":{"existing_option":"provider","required_option":"applied"}
        }));
        let outbound = json_body(&body, &ctx).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&outbound).unwrap(),
            json!({
                "model":"upstream-tts", "input":"hello", "voice":"alloy",
                "custom":{"model":"nested-model"}, "temperature":0.2,
                "target_knob":"from-client", "existing_option":"client",
                "required_option":"applied"
            })
        );
        assert_eq!(body["model"], "speaker");
        assert_eq!(body["temperature"], 0.9);
        assert_eq!(body["source_knob"], "from-client");
    }
}
