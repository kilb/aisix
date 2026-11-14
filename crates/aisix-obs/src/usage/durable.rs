//! 持久终态只重放已保存的字节；易失队列和网络写入都不能充当接收确认。

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::oneshot;
use uuid::Uuid;

use super::UsageEvent;

const BODY_PREFIX: &str = "{\"events\":[";
const BODY_SUFFIX: &str = "]}";
// 与控制面 dp::telemetry::MAX_BODY_BYTES 一致；独立单条发送没有普通队列的 256KiB 丢弃线。
const MAX_BODY_BYTES: usize = 8 << 20;
const MAX_RECEIPT_BYTES: usize = 64 << 10;
const BATCH_ID_HEADER: &str = "x-aisix-usage-batch-id";
const DEDUP_HEADER: &str = "x-aisix-usage-batch-dedup";

/// 持久终态没有收到控制面确认，原 outbox 必须保留并重试。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DurableUsageError {
    #[error("durable usage delivery is unavailable")]
    Unavailable,
    #[error("the canonical durable usage event is invalid")]
    InvalidEvent,
    #[error("the durable usage queue is full")]
    QueueFull,
    #[error("the durable usage queue is closed")]
    QueueClosed,
    #[error("the durable usage receipt was lost")]
    ReceiptLost,
    #[error("the durable usage receipt timed out")]
    TimedOut,
}

/// 持久 outbox 的原始字节；构造后只能读取，交付不能重新填字段或改写事件。
#[derive(Clone)]
pub struct DurableUsageEvent {
    event_id: String,
    json: Arc<str>,
}

impl std::fmt::Debug for DurableUsageEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableUsageEvent")
            .field("bytes", &self.json.len())
            .finish_non_exhaustive()
    }
}

impl DurableUsageEvent {
    pub fn from_event(event: &UsageEvent) -> Result<Self, DurableUsageError> {
        let json = serde_json::to_string(event).map_err(|_| DurableUsageError::InvalidEvent)?;
        Self::from_saved_json(&event.event_id, json)
    }

    pub fn from_saved_json(event_id: &str, json: String) -> Result<Self, DurableUsageError> {
        if event_id.is_empty()
            || event_id.chars().count() > 128
            || json.len() > MAX_BODY_BYTES - BODY_PREFIX.len() - BODY_SUFFIX.len()
        {
            return Err(DurableUsageError::InvalidEvent);
        }
        let value: serde_json::Value =
            serde_json::from_str(&json).map_err(|_| DurableUsageError::InvalidEvent)?;
        if value.get("event_id").and_then(serde_json::Value::as_str) != Some(event_id)
            || !value
                .get("occurred_at")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|at| chrono::DateTime::parse_from_rfc3339(at).is_ok())
        {
            return Err(DurableUsageError::InvalidEvent);
        }
        Ok(Self {
            event_id: event_id.to_owned(),
            json: json.into(),
        })
    }

    pub fn event_id(&self) -> &str {
        &self.event_id
    }

    pub fn as_json(&self) -> &str {
        &self.json
    }
}

/// 逐条接收回执；发送端只能通过实际 HTTP 接收确认完成它。
#[derive(Debug)]
pub struct DurableUsageRequest {
    event: DurableUsageEvent,
    receipt: oneshot::Sender<Result<(), DurableUsageError>>,
}

impl DurableUsageRequest {
    pub(super) fn new(
        event: DurableUsageEvent,
    ) -> (Self, oneshot::Receiver<Result<(), DurableUsageError>>) {
        let (receipt, received) = oneshot::channel();
        (Self { event, receipt }, received)
    }

    pub async fn deliver(self, client: &reqwest::Client, url: &str) {
        if self.receipt.is_closed() {
            return;
        }
        let result = send(client, url, &self.event).await;
        let _ = self.receipt.send(result);
    }
}

#[derive(Deserialize)]
struct Receipt {
    #[serde(default)]
    accepted: Option<u64>,
    #[serde(default)]
    duplicates: Option<u64>,
    #[serde(default)]
    rejected: Option<u64>,
}

async fn send(
    client: &reqwest::Client,
    url: &str,
    event: &DurableUsageEvent,
) -> Result<(), DurableUsageError> {
    // 控制面只按批次 id 去重、不校验正文；绑定全部原始字节，避免不同正文借旧批次被确认。
    let batch_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, event.as_json().as_bytes());
    let body = format!("{BODY_PREFIX}{}{BODY_SUFFIX}", event.as_json());
    let mut response = client
        .post(url)
        .header(BATCH_ID_HEADER, batch_id.to_string())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .timeout(Duration::from_secs(10))
        .body(body)
        .send()
        .await
        .map_err(|_| DurableUsageError::Unavailable)?;
    if !response.status().is_success()
        || !response
            .headers()
            .get(DEDUP_HEADER)
            .is_some_and(|value| value.as_bytes() == b"1")
    {
        return Err(DurableUsageError::Unavailable);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| DurableUsageError::Unavailable)?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RECEIPT_BYTES {
            return Err(DurableUsageError::Unavailable);
        }
        body.extend_from_slice(&chunk);
    }
    let receipt: Receipt =
        serde_json::from_slice(&body).map_err(|_| DurableUsageError::Unavailable)?;
    match (receipt.accepted, receipt.duplicates, receipt.rejected) {
        // 同批次重放明确回 0/0/0，另一个批次已存同 event 则回 0/1/0。
        (Some(accepted), Some(duplicates), Some(0))
            if accepted
                .checked_add(duplicates)
                .is_some_and(|count| count <= 1) =>
        {
            Ok(())
        }
        _ => Err(DurableUsageError::Unavailable),
    }
}
