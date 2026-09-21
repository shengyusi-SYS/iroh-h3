#![cfg(not(target_family = "wasm"))]

use axum::{Router, routing::get};
use iroh::{
    Endpoint,
    address_lookup::memory::MemoryLookup,
    endpoint::presets::Minimal,
    protocol::{AcceptError, ProtocolHandler},
};
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;
use std::time::Duration;
use tokio::sync::mpsc;

const ALPN: &[u8] = b"iroh+h3";

#[derive(Debug)]
struct ObservedServer {
    inner: IrohAxum,
    accepted: mpsc::UnboundedSender<iroh::endpoint::Connection>,
}
impl ProtocolHandler for ObservedServer {
    async fn accept(&self, connection: iroh::endpoint::Connection) -> Result<(), AcceptError> {
        self.accepted.send(connection.clone()).unwrap();
        self.inner.accept(connection).await
    }
}

#[tokio::test]
async fn temporary_clients_close_after_last_response_drops() {
    let lookup = MemoryLookup::new();
    let server = Endpoint::builder(Minimal)
        .address_lookup(lookup.clone())
        .bind()
        .await
        .unwrap();
    let endpoint = Endpoint::builder(Minimal)
        .address_lookup(lookup.clone())
        .bind()
        .await
        .unwrap();
    lookup.add_endpoint_info(server.addr());
    lookup.add_endpoint_info(endpoint.addr());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let router = iroh::protocol::Router::builder(server.clone())
        .accept(
            ALPN,
            ObservedServer {
                inner: IrohAxum::new(
                    Router::new().route("/", get(|| async { "body survives client" })),
                ),
                accepted: tx,
            },
        )
        .spawn();
    let uri = format!("iroh+h3://{}/", server.id());
    for iteration in 0..3 {
        let client = IrohH3Client::new(endpoint.clone(), ALPN.into());
        let response = client.get(&uri).send().await.unwrap();
        let connection = rx.recv().await.unwrap();
        drop(client);
        if iteration == 1 {
            drop(response); // An abandoned response must release its sender too.
        } else {
            assert_eq!(response.text().await.unwrap(), "body survives client");
        }
        tokio::time::timeout(Duration::from_secs(2), connection.closed())
            .await
            .expect("temporary client retained its connection after the response was consumed");
    }
    router.shutdown().await.unwrap();
    endpoint.close().await;
}

#[tokio::test]
async fn response_stream_outlives_client_then_closes_connection() {
    check_late_response(false).await;
}

#[tokio::test]
async fn response_stream_outlives_client_that_owns_endpoint() {
    check_late_response(true).await;
}

async fn check_late_response(client_owns_endpoint: bool) {
    let lookup = MemoryLookup::new();
    let server = Endpoint::builder(Minimal)
        .address_lookup(lookup.clone())
        .bind()
        .await
        .unwrap();
    let endpoint = Endpoint::builder(Minimal)
        .address_lookup(lookup.clone())
        .bind()
        .await
        .unwrap();
    lookup.add_endpoint_info(server.addr());
    lookup.add_endpoint_info(endpoint.addr());
    let (chunks, body_rx) = mpsc::unbounded_channel::<Result<bytes::Bytes, std::io::Error>>();
    let body_rx = std::sync::Arc::new(tokio::sync::Mutex::new(Some(body_rx)));
    let app = Router::new().route(
        "/",
        get(move || {
            let body_rx = body_rx.clone();
            async move {
                let rx = body_rx.lock().await.take().unwrap();
                axum::body::Body::from_stream(futures::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|chunk| (chunk, rx))
                }))
            }
        }),
    );
    let (tx, mut accepted) = mpsc::unbounded_channel();
    let router = iroh::protocol::Router::builder(server.clone())
        .accept(
            ALPN,
            ObservedServer {
                inner: IrohAxum::new(app),
                accepted: tx,
            },
        )
        .spawn();
    let retained_endpoint = (!client_owns_endpoint).then(|| endpoint.clone());
    let client = IrohH3Client::new(endpoint, ALPN.into());
    let response = client
        .get(format!("iroh+h3://{}/", server.id()))
        .send()
        .await
        .unwrap();
    let connection = accepted.recv().await.unwrap();
    drop(client);
    // The data is produced only after dropping the last client, not buffered before it.
    chunks
        .send(Ok(bytes::Bytes::from_static(b"late data")))
        .unwrap();
    drop(chunks);
    assert_eq!(response.text().await.unwrap(), "late data");
    if let Some(endpoint) = retained_endpoint {
        tokio::time::timeout(Duration::from_secs(2), connection.closed())
            .await
            .expect("connection survived its last response stream");
        endpoint.close().await;
    }
    // With no external Endpoint owner, Iroh's final Drop aborts the socket.
    // That releases local resources but does not promise delivery of a remote
    // close packet. Never close an externally shared endpoint on a client's behalf.
    router.shutdown().await.unwrap();
}
