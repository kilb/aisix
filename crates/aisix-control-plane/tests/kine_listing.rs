//! KineClient 的分页组件回归：真实 gRPC 传输，在确定的页边界触发服务错误。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use aisix_control_plane::etcd_front::proto::etcdserverpb::kv_server::{Kv, KvServer};
use aisix_control_plane::etcd_front::proto::etcdserverpb::*;
use aisix_control_plane::etcd_front::proto::mvccpb::KeyValue;
use aisix_control_plane::kine::{KeyValue as StoredKeyValue, KineClient, KineError, KineWriter};
use tonic::{Code, Request, Response, Status};

#[derive(Clone)]
struct Paged {
    generations: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    fail_generations: usize,
    code: Code,
    message: &'static str,
}

#[tonic::async_trait]
impl Kv for Paged {
    async fn range(
        &self,
        request: Request<RangeRequest>,
    ) -> Result<Response<RangeResponse>, Status> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let request = request.into_inner();
        assert_eq!(request.limit, 500);
        assert_eq!(request.range_end, b"/list0");
        let generation = if request.revision == 0 {
            assert_eq!(request.key, b"/list/");
            self.generations.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            assert_eq!(request.key, b"/list/0499\0");
            let generation = self.generations.load(Ordering::SeqCst);
            assert_eq!(request.revision, generation as i64);
            if generation <= self.fail_generations {
                return Err(Status::new(self.code, self.message));
            }
            generation
        };
        let first = request.revision == 0;
        let kvs = (if first { 0..500 } else { 500..501 })
            .map(|i| KeyValue {
                key: format!("/list/{i:04}").into_bytes(),
                value: format!("generation-{generation}").into_bytes(),
                mod_revision: generation as i64,
                ..Default::default()
            })
            .collect();
        Ok(Response::new(RangeResponse {
            header: Some(ResponseHeader {
                revision: generation as i64,
                ..Default::default()
            }),
            kvs,
            more: first,
            count: 501,
        }))
    }

    async fn put(&self, _: Request<PutRequest>) -> Result<Response<PutResponse>, Status> {
        Err(Status::unimplemented("read-only component"))
    }
    async fn delete_range(
        &self,
        _: Request<DeleteRangeRequest>,
    ) -> Result<Response<DeleteRangeResponse>, Status> {
        Err(Status::unimplemented("read-only component"))
    }
    async fn txn(&self, _: Request<TxnRequest>) -> Result<Response<TxnResponse>, Status> {
        Err(Status::unimplemented("read-only component"))
    }
    async fn compact(
        &self,
        _: Request<CompactionRequest>,
    ) -> Result<Response<CompactionResponse>, Status> {
        Err(Status::unimplemented("read-only component"))
    }
}

type Listing = Result<Vec<(Vec<u8>, StoredKeyValue)>, KineError>;

async fn list(fail_generations: usize, code: Code, message: &'static str) -> (Listing, usize) {
    let paged = Paged {
        generations: Arc::new(AtomicUsize::new(0)),
        calls: Arc::new(AtomicUsize::new(0)),
        fail_generations,
        code,
        message,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gRPC component");
    let endpoint = format!(
        "http://{}",
        listener.local_addr().expect("component address")
    );
    let incoming = futures::stream::unfold(listener, |listener| async {
        Some((listener.accept().await.map(|(socket, _)| socket), listener))
    });
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(KvServer::new(paged.clone()))
            .serve_with_incoming(incoming),
    );
    let client = KineClient::connect(&endpoint)
        .await
        .expect("connect real KineClient");
    let result = client.list("/list/").await;
    server.abort();
    (result, paged.calls.load(Ordering::SeqCst))
}

#[tokio::test]
async fn a_compacted_page_restarts_at_a_fresh_revision_without_partial_rows() {
    let (result, calls) = list(
        1,
        Code::OutOfRange,
        "etcdserver: mvcc: required revision has been compacted",
    )
    .await;
    let rows = result.expect("restart after compaction");
    assert_eq!(calls, 4);
    assert_eq!(rows.len(), 501);
    for (i, (key, value)) in rows.into_iter().enumerate() {
        assert_eq!(key, format!("/list/{i:04}").as_bytes());
        assert_eq!(value.value, b"generation-2");
        assert_eq!(value.mod_revision, 2);
    }
}

#[tokio::test]
async fn repeated_compaction_stops_after_three_restarts() {
    let (result, calls) = list(
        usize::MAX,
        Code::OutOfRange,
        "etcdserver: mvcc: required revision has been compacted",
    )
    .await;
    assert!(result.is_err());
    assert_eq!(calls, 8, "four listings, two pages each");
}

#[tokio::test]
async fn other_range_errors_are_not_retried() {
    for (code, message) in [
        (
            Code::OutOfRange,
            "etcdserver: mvcc: required revision is a future revision",
        ),
        (
            Code::PermissionDenied,
            "required revision has been compacted",
        ),
        (Code::Unavailable, "temporarily unavailable"),
    ] {
        let (result, calls) = list(usize::MAX, code, message).await;
        assert!(result.is_err());
        assert_eq!(calls, 2, "{code:?}: {message}");
    }
}
