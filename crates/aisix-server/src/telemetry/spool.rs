//! 普通用量的本地持久队列。磁盘写入和 HTTP 重试分别运行；只重放已经密封的批次。

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};

use super::{
    batch_body, fill_ready_batch, is_retryable, non_empty, random_jitter, remaining_budget, Buffer,
    Bytes, Instant, Metrics, Prepared, TelemetryConfig, UsageEvent, UsageEventLabels, Uuid,
    MAX_ANSWERED_FAILURES, MAX_BATCH, MAX_BATCH_BYTES, RETRY_INITIAL_BACKOFF, RETRY_MAX_BACKOFF,
    USAGE_BATCH_DEDUP_ENABLED, USAGE_BATCH_DEDUP_HEADER, USAGE_BATCH_ID_HEADER,
};

const MAX_SPOOL_BYTES: u64 = 1 << 30;
const MAX_SPOOL_BATCHES: usize = 16_384;
const MAX_RECEIPT_BYTES: u64 = 64 << 10;
const LOCK: &str = ".lock";
const WRITING: &str = ".writing";

pub(super) struct Spool {
    path: PathBuf,
    directory: File,
    _lock: File,
    sequence: u64,
    pending: VecDeque<Entry>,
    reconciliation: usize,
    bytes: u64,
    unsynced: bool,
    last_disk_warning: Option<Instant>,
}

#[derive(Clone, Copy)]
struct Entry {
    sequence: u64,
    id: Uuid,
    count: usize,
    bytes: u64,
    deadline_ms: i64,
    retry_at: Instant,
    backoff: Duration,
    answered_failures: usize,
}

impl Entry {
    fn name(self, state: &str) -> String {
        format!(
            "{:020}-{}.{}.{}.{}",
            self.sequence, self.id, self.count, self.deadline_ms, state
        )
    }
}

struct Delivery {
    entry: Entry,
    body: Bytes,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Disposition {
    Confirmed,
    Reconcile(&'static str),
    Rejected(u64),
    Retry { answered: bool },
}

impl Spool {
    pub(super) fn open(path: &Path, url: &str, env_id: &str, dp_id: &str) -> Result<Self> {
        let url = reqwest::Url::parse(url).map_err(|_| anyhow::anyhow!("invalid telemetry URL"))?;
        let scope = serde_json::to_vec(&(
            url.origin().ascii_serialization(),
            url.path(),
            env_id,
            dp_id,
        ))?;
        let scope = format!(".scope-{}", Uuid::new_v5(&Uuid::NAMESPACE_OID, &scope));
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
            "usage spool must be an owner-only directory"
        );
        if let Some(parent) = path.parent() {
            File::open(parent)?.sync_all()?;
        }
        let directory = File::open(path)?;
        let lock = private_file()
            .create(true)
            .truncate(false)
            .open(path.join(LOCK))?;
        lock.try_lock().context("usage spool is already in use")?;

        let mut spool = Self {
            path: path.to_owned(),
            directory,
            _lock: lock,
            sequence: 0,
            pending: VecDeque::new(),
            reconciliation: 0,
            bytes: 0,
            unsynced: false,
            last_disk_warning: None,
        };
        let mut same_scope = false;
        let mut partial = None;
        for item in fs::read_dir(path)? {
            let item = item?;
            let name = item.file_name();
            let name = name.to_str().context("invalid usage spool file name")?;
            match name {
                LOCK => continue,
                WRITING => {
                    partial = Some(item.path());
                    continue;
                }
                name if name == scope => {
                    same_scope = true;
                    continue;
                }
                name if name.starts_with(".scope-") => {
                    anyhow::bail!("usage spool belongs to a different control plane or gateway");
                }
                _ => {}
            }
            let (prefix, state) = name
                .rsplit_once('.')
                .context("invalid usage spool file name")?;
            let (prefix, deadline) = prefix
                .rsplit_once('.')
                .context("invalid usage spool file name")?;
            let (identity, count) = prefix
                .rsplit_once('.')
                .context("invalid usage spool file name")?;
            let (sequence, id) = identity
                .split_once('-')
                .context("invalid usage spool file name")?;
            let metadata = fs::symlink_metadata(item.path())?;
            let entry = Entry {
                sequence: sequence.parse()?,
                id: Uuid::parse_str(id)?,
                count: count.parse()?,
                bytes: metadata.len(),
                deadline_ms: deadline.parse()?,
                retry_at: Instant::now(),
                backoff: RETRY_INITIAL_BACKOFF,
                answered_failures: 0,
            };
            ensure!(
                metadata.is_file()
                    && metadata.permissions().mode() & 0o077 == 0
                    && matches!(state, "ready" | "reconcile")
                    && entry.name(state) == name
                    && (1..=MAX_BATCH).contains(&entry.count)
                    && (1..=MAX_BATCH_BYTES as u64).contains(&entry.bytes),
                "invalid usage spool entry"
            );
            ensure!(
                spool.has_room(entry.bytes),
                "usage spool exceeds its storage limit"
            );
            spool.sequence = spool.sequence.max(entry.sequence);
            spool.bytes += entry.bytes;
            if state == "ready" {
                spool.pending.push_back(entry);
            } else {
                spool.reconciliation += 1;
            }
        }
        ensure!(
            same_scope || (spool.bytes == 0 && partial.is_none()),
            "usage spool is missing its gateway identity"
        );
        if !same_scope {
            private_file()
                .create_new(true)
                .open(path.join(scope))?
                .sync_all()?;
        }
        // 先核对目录归属，再移除从未取得落盘确认的临时文件。
        if let Some(partial) = partial {
            fs::remove_file(partial)?;
        }
        spool
            .pending
            .make_contiguous()
            .sort_unstable_by_key(|entry| entry.sequence);
        spool.directory.sync_all()?;
        Ok(spool)
    }

    fn has_room(&self, bytes: u64) -> bool {
        self.pending.len() + self.reconciliation < MAX_SPOOL_BATCHES
            && bytes <= MAX_SPOOL_BYTES - self.bytes
    }

    pub(super) fn append(
        &mut self,
        body: &Bytes,
        count: usize,
        deadline_ms: i64,
    ) -> io::Result<()> {
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("usage spool sequence exhausted"))?;
        let temporary = self.path.join(WRITING);
        let mut file = private_file()
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(body)?;
        file.sync_all()?;
        let entry = Entry {
            sequence,
            id: Uuid::new_v4(),
            count,
            bytes: body.len() as u64,
            deadline_ms,
            retry_at: Instant::now(),
            backoff: RETRY_INITIAL_BACKOFF,
            answered_failures: 0,
        };
        fs::rename(temporary, self.path.join(entry.name("ready")))?;
        self.sequence = sequence;
        self.bytes += entry.bytes;
        self.pending.push_back(entry);
        self.unsynced = true;
        Ok(())
    }

    pub(super) fn sync_new_entries(&mut self) -> io::Result<()> {
        if self.unsynced {
            self.directory.sync_all()?;
            self.unsynced = false;
        }
        Ok(())
    }

    fn load_front(&mut self) -> io::Result<Option<Delivery>> {
        self.sync_new_entries()?;
        let now = Instant::now();
        let Some(index) = self.pending.iter().position(|entry| entry.retry_at <= now) else {
            return Ok(None);
        };
        self.pending.rotate_left(index);
        let Some(&entry) = self.pending.front() else {
            return Ok(None);
        };
        if chrono::Utc::now().timestamp_millis() >= entry.deadline_ms {
            return Ok(Some(Delivery {
                entry,
                body: Bytes::new(),
            }));
        }
        match self.read_body(entry) {
            Ok(body) => Ok(Some(Delivery { entry, body })),
            Err(error) => {
                self.defer_front(false);
                Err(error)
            }
        }
    }

    fn read_body(&self, entry: Entry) -> io::Result<Bytes> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(self.path.join(entry.name("ready")))?;
        let mut body = Vec::with_capacity(entry.bytes as usize);
        file.take(entry.bytes + 1).read_to_end(&mut body)?;
        if body.len() as u64 != entry.bytes {
            return Err(io::Error::other("usage spool entry size changed"));
        }
        Ok(body.into())
    }

    fn defer_front(&mut self, answered: bool) {
        if let Some(mut entry) = self.pending.pop_front() {
            entry.retry_at = Instant::now() + random_jitter(entry.backoff);
            entry.backoff = (entry.backoff * 2).min(RETRY_MAX_BACKOFF);
            entry.answered_failures += usize::from(answered);
            self.pending.push_back(entry);
        }
    }

    fn finish(&mut self, entry: Entry, disposition: Disposition) -> io::Result<()> {
        let path = self.path.join(entry.name("ready"));
        match disposition {
            Disposition::Confirmed => match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            },
            Disposition::Retry { answered } => {
                self.defer_front(answered);
                return Ok(());
            }
            Disposition::Reconcile(_) | Disposition::Rejected(_) => {
                let kept = self.path.join(entry.name("reconcile"));
                match fs::rename(path, &kept) {
                    Ok(()) => {}
                    // 上一次可能已改名，只有目录同步没有成功。
                    Err(error) if error.kind() == io::ErrorKind::NotFound && kept.is_file() => {}
                    Err(error) => return Err(error),
                }
            }
        }
        self.directory.sync_all()?;
        self.unsynced = false;
        self.pending.pop_front();
        match disposition {
            Disposition::Confirmed => self.bytes -= entry.bytes,
            Disposition::Reconcile(_) | Disposition::Rejected(_) => self.reconciliation += 1,
            Disposition::Retry { .. } => {}
        }
        Ok(())
    }

    fn report_depth(&self, metrics: &Metrics) {
        metrics.record_usage_spool_depth(self.pending.len(), self.reconciliation, self.bytes);
        if !self.pending.is_empty() {
            metrics.usage_delivery().note_waiting();
        }
    }

    fn disk_failure(&mut self, metrics: &Metrics, reason: &'static str, error: &io::Error) {
        metrics.record_usage_spool_failure(reason);
        metrics.usage_delivery().note_waiting();
        let now = Instant::now();
        if self
            .last_disk_warning
            .is_none_or(|last| now - last >= Duration::from_secs(1))
        {
            tracing::error!(%error, reason, "usage spool filesystem operation failed");
            self.last_disk_warning = Some(now);
        }
    }

    fn persist_buffer(&mut self, buffer: &mut Buffer, metrics: &Metrics) {
        if !buffer.is_empty() {
            metrics.usage_delivery().note_waiting();
        }
        while !buffer.is_empty() {
            let events = buffer.take_batch();
            let body = batch_body(&events);
            let deadline_ms = chrono::Utc::now().timestamp_millis()
                + remaining_budget(&events).as_millis() as i64;
            if !self.has_room(body.len() as u64) {
                metrics.record_usage_spool_failure("full");
                metrics.usage_delivery().note_waiting();
                record_drop(metrics, &events, "spool_full");
            } else if let Err(error) = self.append(&body, events.len(), deadline_ms) {
                self.disk_failure(metrics, "write", &error);
                let _ = fs::remove_file(self.path.join(WRITING));
                record_drop(metrics, &events, "spool_write_failed");
            }
        }
        if let Err(error) = self.sync_new_entries() {
            self.disk_failure(metrics, "sync", &error);
        }
        self.report_depth(metrics);
    }

    async fn write_loop(
        mut self,
        mut rx: mpsc::Receiver<UsageEvent>,
        outgoing: mpsc::Sender<Delivery>,
        mut receipts: mpsc::Receiver<(Entry, Disposition)>,
        metrics: Metrics,
        interval: Duration,
        mut stop: watch::Receiver<bool>,
    ) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut buffer = Buffer::default();
        let mut in_flight = false;
        let mut receipt = None;
        let mut receipts_open = true;

        self.report_depth(&metrics);
        while !*stop.borrow() {
            tokio::select! {
                event = rx.recv() => match event {
                    Some(event) => {
                        if buffer.push(event, &metrics) {
                            metrics.usage_delivery().note_waiting();
                        }
                        if !buffer.holds_a_full_batch() {
                            continue;
                        }
                        self.persist_buffer(&mut buffer, &metrics);
                    }
                    None => break,
                },
                _ = ticker.tick() => {
                    fill_ready_batch(&mut buffer, &mut rx, &metrics);
                    self.persist_buffer(&mut buffer, &metrics);
                },
                result = receipts.recv(), if receipts_open && in_flight && receipt.is_none() => {
                    receipts_open = result.is_some();
                    receipt = result;
                },
                _ = stop.wait_for(|stopping| *stopping) => break,
            }

            if let Some((entry, disposition)) = receipt {
                if let Err(error) = self.finish(entry, disposition) {
                    self.disk_failure(&metrics, "finalize", &error);
                    continue;
                }
                match disposition {
                    Disposition::Confirmed => metrics.usage_delivery().note_reached(),
                    Disposition::Reconcile(reason) => {
                        metrics.record_usage_spool_failure(reason);
                        tracing::warn!(batch_id = %entry.id, reason, "usage spool batch requires reconciliation");
                    }
                    Disposition::Rejected(rejected) => {
                        metrics.record_usage_events_rejected(rejected);
                        metrics.record_usage_spool_failure("rejected");
                        metrics.usage_delivery().note_reached();
                        tracing::warn!(batch_id = %entry.id, rejected, "usage spool batch requires reconciliation");
                    }
                    Disposition::Retry { .. } => {}
                }
                self.report_depth(&metrics);
                receipt = None;
                in_flight = false;
            }
            if receipts_open && !in_flight {
                match self.load_front() {
                    Ok(Some(delivery)) => {
                        // 同一时刻只交付一个头部批次，确保重试和重启都沿用顺序。
                        in_flight = outgoing.try_send(delivery).is_ok();
                        receipts_open = in_flight;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        self.disk_failure(&metrics, "read", &error);
                    }
                }
            }
        }

        // 代理已停止产生用量；将内存队列的尾部留给下次启动继续投递。
        rx.close();
        loop {
            fill_ready_batch(&mut buffer, &mut rx, &metrics);
            if buffer.is_empty() {
                break;
            }
            self.persist_buffer(&mut buffer, &metrics);
        }
    }
}

fn private_file() -> OpenOptions {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    options
}

fn record_drop(metrics: &Metrics, events: &[Prepared], reason: &'static str) {
    metrics.record_usage_event_drops(
        reason,
        events.iter().map(|event| UsageEventLabels {
            user_id: non_empty(&event.user_id),
            user_name: non_empty(&event.user_name),
            ..UsageEventLabels::default()
        }),
    );
}

#[derive(Deserialize)]
struct Receipt {
    #[serde(default)]
    accepted: Option<usize>,
    #[serde(default)]
    duplicates: Option<usize>,
    #[serde(default)]
    rejected: Option<usize>,
}

fn supports_dedup(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(USAGE_BATCH_DEDUP_HEADER)
        .is_some_and(|header| header.as_bytes() == USAGE_BATCH_DEDUP_ENABLED.as_bytes())
}

async fn probe_dedup(client: &reqwest::Client, cfg: &TelemetryConfig) -> bool {
    // 控制面先认证，再用 400 拒绝缺少批次 ID 的请求，此时尚未读取或入账。
    let Ok(response) = client.post(&cfg.url).send().await else {
        return false;
    };
    if response.status() != reqwest::StatusCode::BAD_REQUEST || !supports_dedup(&response) {
        return false;
    }
    let Ok(Some(body)) = receipt_body(response).await else {
        return false;
    };
    serde_json::from_slice::<serde_json::Value>(&body)
        .is_ok_and(|answer| answer["code"] == "bad_request")
}

async fn receipt_body(mut response: reqwest::Response) -> Result<Option<Vec<u8>>, reqwest::Error> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) as u64 > MAX_RECEIPT_BYTES {
            return Ok(None);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Some(bytes))
}

fn read_receipt(bytes: &[u8], count: usize) -> Option<Disposition> {
    let receipt: Receipt = serde_json::from_slice(bytes).ok()?;
    let rejected = receipt.rejected?;
    let stored = receipt.accepted?.checked_add(receipt.duplicates?)?;
    let total = stored.checked_add(rejected)?;
    // 首次投递计数之和等于事件数；同 ID 重放只返回首次拒收数。
    if rejected > 0 && (stored == 0 || total == count) && rejected <= count {
        Some(Disposition::Rejected(rejected as u64))
    } else if rejected == 0 && (stored == 0 || stored == count) {
        Some(Disposition::Confirmed)
    } else {
        None
    }
}

async fn deliver(
    client: &reqwest::Client,
    cfg: &TelemetryConfig,
    metrics: &Metrics,
    delivery: &Delivery,
    verified_dedup: &mut bool,
) -> Disposition {
    if delivery.body.is_empty()
        || chrono::Utc::now().timestamp_millis() >= delivery.entry.deadline_ms
    {
        return Disposition::Reconcile("expired");
    }
    if !*verified_dedup {
        *verified_dedup = probe_dedup(client, cfg).await;
        if !*verified_dedup {
            metrics.record_usage_spool_failure("capability");
            return Disposition::Retry { answered: false };
        }
        if chrono::Utc::now().timestamp_millis() >= delivery.entry.deadline_ms {
            return Disposition::Reconcile("expired");
        }
    }
    let response = client
        .post(&cfg.url)
        .header(USAGE_BATCH_ID_HEADER, delivery.entry.id.to_string())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(delivery.body.clone())
        .send()
        .await;
    let Ok(response) = response else {
        // 重新建立传输后先验证接收端仍支持同一去重契约。
        *verified_dedup = false;
        return Disposition::Retry { answered: false };
    };
    if !supports_dedup(&response) {
        *verified_dedup = false;
        return Disposition::Reconcile("unconfirmed");
    }
    let status = response.status();
    if status.is_success() {
        return match receipt_body(response).await {
            Ok(Some(bytes)) => read_receipt(&bytes, delivery.entry.count)
                .unwrap_or(Disposition::Reconcile("unconfirmed")),
            Ok(None) => Disposition::Reconcile("unconfirmed"),
            Err(_) => {
                *verified_dedup = false;
                Disposition::Retry { answered: false }
            }
        };
    }
    if !is_retryable(status) {
        return Disposition::Reconcile("refused");
    }
    let answered = status != reqwest::StatusCode::SERVICE_UNAVAILABLE;
    if answered && delivery.entry.answered_failures + 1 >= MAX_ANSWERED_FAILURES {
        return Disposition::Reconcile("refused");
    }
    Disposition::Retry { answered }
}

pub(super) async fn run(
    cfg: TelemetryConfig,
    client: reqwest::Client,
    spool: Spool,
    metrics: Metrics,
    rx: mpsc::Receiver<UsageEvent>,
    mut stop: watch::Receiver<bool>,
) {
    let (outgoing, mut deliveries) = mpsc::channel::<Delivery>(1);
    let (receipts, incoming) = mpsc::channel(1);
    let runtime = tokio::runtime::Handle::current();
    let disk_stop = stop.clone();
    let disk_metrics = metrics.clone();
    let interval = cfg.interval;
    let disk = tokio::task::spawn_blocking(move || {
        runtime.block_on(spool.write_loop(
            rx,
            outgoing,
            incoming,
            disk_metrics,
            interval,
            disk_stop,
        ));
    });
    let network = async {
        let mut verified_dedup = false;
        while let Some(delivery) = deliveries.recv().await {
            let disposition =
                deliver(&client, &cfg, &metrics, &delivery, &mut verified_dedup).await;
            if receipts.send((delivery.entry, disposition)).await.is_err() {
                return;
            }
            if matches!(disposition, Disposition::Retry { .. }) {
                metrics.record_usage_spool_failure("delivery");
                // 网络不可达时只保留一个发送者，并限制首次重试的总请求频率。
                tokio::time::sleep(RETRY_INITIAL_BACKOFF).await;
            }
        }
    };
    tokio::select! {
        _ = network => {}
        _ = stop.wait_for(|stopping| *stopping) => {}
    }
    if let Err(error) = disk.await {
        metrics.record_usage_spool_failure("worker");
        metrics.usage_delivery().note_waiting();
        tracing::error!(%error, "usage spool writer stopped unexpectedly");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unowned_partial_is_preserved_and_a_live_spool_is_exclusive() {
        let root = tempfile::tempdir().unwrap();
        let queue = root.path().join("queue");
        fs::DirBuilder::new().mode(0o700).create(&queue).unwrap();
        let partial_path = queue.join(WRITING);
        fs::write(&partial_path, b"unowned").unwrap();
        let url = "https://localhost/dp/telemetry";
        assert!(Spool::open(&queue, url, "env", "dp").is_err());
        assert_eq!(fs::read(&partial_path).unwrap(), b"unowned");

        let queue = root.path().join("owned");
        let lock_holder = Spool::open(&queue, url, "env", "dp").unwrap();
        assert!(Spool::open(&queue, url, "env", "dp").is_err());
        drop(lock_holder);
        assert!(Spool::open(&queue, url, "different-env", "dp").is_err());
        assert!(Spool::open(&queue, url, "env", "dp").is_ok());
    }

    #[test]
    fn full_reconciliation_storage_preserves_existing_batches_and_counts_new_loss() {
        let root = tempfile::tempdir().unwrap();
        let queue = root.path().join("queue");
        let url = "https://localhost/dp/telemetry";
        let spool = Spool::open(&queue, url, "env", "dp").unwrap();
        let files = MAX_SPOOL_BYTES / MAX_BATCH_BYTES as u64;
        for sequence in 1..=files {
            let entry = Entry {
                sequence,
                id: Uuid::new_v4(),
                count: 1,
                bytes: MAX_BATCH_BYTES as u64,
                deadline_ms: 0,
                retry_at: Instant::now(),
                backoff: RETRY_INITIAL_BACKOFF,
                answered_failures: 0,
            };
            // 稀疏文件使测试实际占用很小，在重新打开时仍按可读字节计入容量。
            let file = private_file()
                .create_new(true)
                .open(queue.join(entry.name("reconcile")))
                .unwrap();
            file.set_len(MAX_BATCH_BYTES as u64).unwrap();
        }
        drop(spool);

        let mut spool = Spool::open(&queue, url, "env", "dp").unwrap();
        let metrics = Metrics::new(false);
        let mut incoming = Buffer::default();
        assert!(incoming.push(UsageEvent::default(), &metrics));
        spool.persist_buffer(&mut incoming, &metrics);
        assert!(incoming.is_empty());
        assert!(spool.pending.is_empty());
        assert_eq!(spool.reconciliation as u64, files);
        assert_eq!(spool.bytes, MAX_SPOOL_BYTES);
        assert_eq!(
            metrics.usage_event_drop_totals().get("spool_full"),
            Some(&1)
        );
        assert!(metrics.usage_delivery().is_waiting());
        assert_eq!(fs::read_dir(&queue).unwrap().count() as u64, files + 2);
    }
}
