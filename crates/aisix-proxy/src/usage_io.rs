//! 历史 v1 客户端边界的解析回归；生产路径只观察 gateway 中的实际上游交换。

#[allow(dead_code)]
mod parser;

use aisix_core::usage_io::{TextMeasurement, UsageIoAudit};
pub(crate) use parser::Protocol;

#[derive(Default)]
pub(crate) struct Exchange {
    websocket: bool,
    input: Option<TextMeasurement>,
    output: Option<parser::ResponseObserver>,
    finished_output: Option<TextMeasurement>,
    response_complete: bool,
    response_failed: bool,
    formatted_transcript: bool,
}

impl Exchange {
    pub(crate) fn mark_websocket(&mut self) {
        self.websocket = true;
    }

    pub(crate) fn set_input(&mut self, input: Option<TextMeasurement>) {
        // 只记录第一次解码的客户端输入；重试和协议变换不能覆盖它。
        if self.input.is_none() {
            self.input = input;
        }
    }

    pub(crate) fn observe_response(&mut self, protocol: Protocol, sse: bool) {
        if matches!(
            protocol,
            Protocol::Chat
                | Protocol::Completions
                | Protocol::Messages
                | Protocol::Responses
                | Protocol::Transcription
                | Protocol::Images
        ) {
            self.output = Some(parser::ResponseObserver::new(protocol, sse));
        }
    }

    pub(crate) fn observe_plain_response(&mut self) {
        if !self.formatted_transcript {
            self.output = Some(parser::ResponseObserver::plain_text());
        }
    }

    pub(crate) fn formatted_transcript(&mut self) {
        self.formatted_transcript = true;
    }

    pub(crate) fn feed(&mut self, data: &[u8]) {
        if let Some(observer) = self.output.as_mut() {
            observer.feed(data);
        }
    }

    pub(crate) fn response_complete(&mut self) {
        self.response_complete = !self.response_failed;
    }

    pub(crate) fn response_failed(&mut self) {
        self.response_failed = true;
        self.response_complete = false;
    }

    pub(crate) fn finish(&mut self) {
        if let Some(observer) = self.output.take() {
            self.finished_output = observer.finish(self.response_complete);
        }
    }

    pub(crate) fn snapshot(&self, sizes: (Option<u64>, Option<u64>)) -> UsageIoAudit {
        UsageIoAudit {
            version: 1,
            boundary: "client_exchange".to_owned(),
            request_complete: None,
            exchange_count: None,
            transport: None,
            request_body_bytes: (!self.websocket).then_some(sizes.0).flatten(),
            response_body_bytes: (!self.websocket).then_some(sizes.1).flatten(),
            response_complete: self.response_complete,
            input_text: self.input,
            output_text: self.finished_output,
        }
    }
}

/// 驱动真正的正文观察适配器，测试不直接构造审计值或修改 attribution 内部状态。
pub(crate) async fn observe_test_exchange(
    protocol: aisix_gateway::upstream_io::Protocol,
    request: &[u8],
    response: &[u8],
) {
    use aisix_gateway::upstream_io::{ObservedBody, UpstreamIoObserver};
    use axum::body::{to_bytes, Body};

    let observer = UpstreamIoObserver::observed(protocol);
    let request_body = Body::new(ObservedBody::request(
        Body::from(request.to_vec()),
        observer.clone(),
    ));
    assert_eq!(
        to_bytes(request_body, usize::MAX).await.unwrap().as_ref(),
        request,
        "观察不改变实际上游请求正文"
    );
    let response_body = Body::new(ObservedBody::response(
        Body::from(response.to_vec()),
        observer,
    ));
    assert_eq!(
        to_bytes(response_body, usize::MAX).await.unwrap().as_ref(),
        response,
        "观察不改变实际上游响应正文"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_rounds_keep_separate_text_and_never_publish_synthetic_http_sizes() {
        let mut first = Exchange::default();
        first.mark_websocket();
        first.set_input(Some(TextMeasurement {
            utf8_bytes: 8,
            unicode_scalars: 3,
            complete: true,
        }));
        first.observe_response(Protocol::Responses, false);
        first.feed(br#"{"output":[{"content":[{"text":"x"}]}]}"#);
        first.response_complete();
        first.finish();
        let first = first.snapshot((Some(999), Some(777)));
        assert_eq!(first.request_body_bytes, None);
        assert_eq!(first.response_body_bytes, None);
        assert_eq!(first.input_text.map(|text| text.utf8_bytes), Some(8));
        assert_eq!(first.output_text.map(|text| text.utf8_bytes), Some(1));
        let mut second = Exchange::default();
        second.mark_websocket();
        second.set_input(Some(TextMeasurement {
            utf8_bytes: 0,
            unicode_scalars: 0,
            complete: true,
        }));
        second.finish();
        let second = second.snapshot((Some(111), Some(222)));
        assert_eq!(second.input_text.map(|text| text.utf8_bytes), Some(0));
        assert_eq!(second.output_text, None);
        assert!(!second.response_complete);
    }

    #[test]
    fn retry_and_guardrail_rewrite_cannot_replace_original_input() {
        let mut exchange = Exchange::default();
        let original = TextMeasurement {
            utf8_bytes: 8,
            unicode_scalars: 3,
            complete: true,
        };
        exchange.set_input(Some(original));
        exchange.set_input(Some(TextMeasurement {
            utf8_bytes: 1,
            unicode_scalars: 1,
            complete: true,
        }));
        assert_eq!(exchange.snapshot((None, None)).input_text, Some(original));
    }

    #[test]
    fn transport_error_cannot_become_complete_after_later_eof() {
        let mut exchange = Exchange::default();
        exchange.response_failed();
        exchange.response_complete();
        exchange.finish();
        assert!(!exchange.snapshot((None, Some(0))).response_complete);
    }

    #[test]
    fn subtitle_framing_is_not_counted_even_with_plain_text_content_type() {
        let mut exchange = Exchange::default();
        exchange.formatted_transcript();
        exchange.observe_plain_response();
        exchange.feed(b"1\n00:00:00,000 --> 00:00:01,000\nhello\n");
        exchange.response_complete();
        exchange.finish();
        assert_eq!(exchange.snapshot((None, Some(37))).output_text, None);
    }
}
