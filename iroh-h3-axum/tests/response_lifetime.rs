//! Native regression coverage for the server-side cancellation boundary.
#![cfg(not(target_family = "wasm"))]

use axum::{
    Router,
    body::{Body, Bytes},
    routing::get,
};
use futures_lite::stream;
use iroh::{
    Endpoint,
    address_lookup::memory::MemoryLookup,
    endpoint::presets::Minimal,
    protocol::{AcceptError, ProtocolHandler},
};
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::{IrohH3Client, error::Error};
use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

const ALPN: &[u8] = b"response-lifetime-test";
#[derive(Debug)]
struct CountConnections {
    inner: IrohAxum,
    count: Arc<AtomicUsize>,
}
impl ProtocolHandler for CountConnections {
    async fn accept(&self, connection: iroh::endpoint::Connection) -> Result<(), AcceptError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.inner.accept(connection).await
    }
}
struct Fixture {
    client: IrohH3Client,
    endpoint: Endpoint,
    router: iroh::protocol::Router,
    uri: String,
    connections: Arc<AtomicUsize>,
}
impl Fixture {
    async fn new(app: Router) -> Self {
        let lookup = MemoryLookup::new();
        let make = || {
            Endpoint::builder(Minimal)
                .clear_ip_transports()
                .clear_relay_transports()
                .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
                .unwrap()
                .address_lookup(lookup.clone())
        };
        let server = make().bind().await.unwrap();
        let endpoint = make().bind().await.unwrap();
        lookup.add_endpoint_info(server.addr());
        lookup.add_endpoint_info(endpoint.addr());
        let uri = format!("iroh+h3://{}", server.id());
        let connections = Arc::new(AtomicUsize::new(0));
        let router = iroh::protocol::Router::builder(server)
            .accept(
                ALPN,
                CountConnections {
                    inner: IrohAxum::new(app.route("/ping", get(|| async { "pong" }))),
                    count: connections.clone(),
                },
            )
            .spawn();
        let client = IrohH3Client::new(endpoint.clone(), ALPN.to_vec());
        Self {
            client,
            endpoint,
            router,
            uri,
            connections,
        }
    }
    async fn assert_same_connection_works(&self) {
        let body = tokio::time::timeout(Duration::from_secs(2), async {
            self.client
                .get(format!("{}/ping", self.uri))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        })
        .await
        .expect("another stream was blocked by cancellation");
        assert_eq!(body, "pong");
        assert_eq!(
            self.connections.load(Ordering::SeqCst),
            1,
            "silently reconnected"
        );
    }
    async fn close(self) {
        drop(self.client);
        self.endpoint.close().await;
        self.router.shutdown().await.unwrap();
    }
}
struct DropSignal(Arc<Notify>);
impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

#[tokio::test]
async fn pending_response_body_drops_on_cancel_without_closing_shared_connection() {
    let entered = Arc::new(Notify::new());
    let dropped = Arc::new(Notify::new());
    let app = Router::new().route(
        "/body",
        get({
            let entered = entered.clone();
            let dropped = dropped.clone();
            move || {
                let entered = entered.clone();
                let guard = DropSignal(dropped.clone());
                async move {
                    Body::from_stream(stream::unfold(guard, move |guard| {
                        let entered = entered.clone();
                        async move {
                            entered.notify_one();
                            std::future::pending::<Option<(Result<Bytes, Infallible>, DropSignal)>>(
                            )
                            .await
                            .map(|(chunk, _)| (chunk, guard))
                        }
                    }))
                }
            }
        }),
    );
    let fixture = Fixture::new(app).await;
    let response = fixture
        .client
        .get(format!("{}/body", fixture.uri))
        .send_cancellable()
        .unwrap()
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let body = response.cancellable_bytes_stream().unwrap();
    body.cancel_handle().cancel();
    drop(body);
    tokio::time::timeout(Duration::from_secs(2), dropped.notified())
        .await
        .expect("server retained a pending body after cancellation");
    fixture.assert_same_connection_works().await;
    fixture.close().await;
}

#[tokio::test]
async fn cancelling_before_headers_preserves_handler_completion() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let completed = Arc::new(Notify::new());
    let dropped = Arc::new(Notify::new());
    let app = Router::new().route(
        "/handler",
        get({
            let entered = entered.clone();
            let release = release.clone();
            let completed = completed.clone();
            let dropped = dropped.clone();
            move || {
                let entered = entered.clone();
                let release = release.clone();
                let completed = completed.clone();
                let guard = DropSignal(dropped.clone());
                async move {
                    let _guard = guard;
                    entered.notify_one();
                    release.notified().await;
                    completed.notify_one();
                    "completed"
                }
            }
        }),
    );
    let fixture = Fixture::new(app).await;
    let pending = fixture
        .client
        .get(format!("{}/handler", fixture.uri))
        .send_cancellable()
        .unwrap();
    let cancel = pending.cancel_handle();
    let task = tokio::spawn(pending);
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    cancel.cancel();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap(),
        Err(Error::Cancelled)
    ));
    fixture.assert_same_connection_works().await;
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), completed.notified())
        .await
        .expect("cancellation interrupted unfinished business work");
    tokio::time::timeout(Duration::from_secs(2), dropped.notified())
        .await
        .unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn response_body_error_is_not_reported_as_clean_eof() {
    let app = Router::new().route(
        "/error",
        get(|| async {
            Body::from_stream(stream::once(Err::<Bytes, _>(std::io::Error::other(
                "producer failed",
            ))))
        }),
    );
    let fixture = Fixture::new(app).await;
    let result = tokio::time::timeout(Duration::from_secs(2), async {
        fixture
            .client
            .get(format!("{}/error", fixture.uri))
            .send()
            .await?
            .bytes()
            .await
    })
    .await
    .expect("body error did not terminate the response");
    assert!(
        result.is_err(),
        "producer error was silently converted into a successful empty body"
    );
    fixture.assert_same_connection_works().await;
    fixture.close().await;
}
