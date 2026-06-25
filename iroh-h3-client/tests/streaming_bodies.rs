use std::convert::Infallible;
use std::future::Future;
use std::task::Poll;

use axum::{
    Router,
    body::Body,
    response::IntoResponse,
    routing::{get, post},
};
use bytes::Bytes;
use futures::StreamExt;
use http_body::Frame;
use http_body_util::{StreamBody, combinators::BoxBody};
use iroh::Endpoint;
#[cfg(target_family = "wasm")]
use iroh::endpoint::presets::N0;
#[cfg(not(target_family = "wasm"))]
use iroh::{address_lookup::memory::MemoryLookup, endpoint::presets::Minimal};
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;
use wasm_bindgen_test::wasm_bindgen_test;
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const ALPN: &[u8] = b"iroh+h3";

#[cfg(not(target_family = "wasm"))]
struct TestApp {
    endpoint: Endpoint,
    client: IrohH3Client,
    _router: iroh::protocol::Router,
}

#[cfg(not(target_family = "wasm"))]
async fn spawn_test_app(app: Router) -> TestApp {
    let address_lookup = MemoryLookup::new();
    let server_endpoint = Endpoint::builder(Minimal)
        .address_lookup(address_lookup.clone())
        .bind()
        .await
        .unwrap();
    let client_endpoint = Endpoint::builder(Minimal)
        .address_lookup(address_lookup.clone())
        .bind()
        .await
        .unwrap();
    address_lookup.add_endpoint_info(server_endpoint.addr());
    address_lookup.add_endpoint_info(client_endpoint.addr());

    let router = iroh::protocol::Router::builder(server_endpoint.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    TestApp {
        endpoint: server_endpoint,
        client: IrohH3Client::new(client_endpoint, ALPN.into()),
        _router: router,
    }
}

/// Streaming responses
#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn streaming_response() {
    let (endpoint_1, endpoint_2) = {
        #[cfg(not(target_family = "wasm"))]
        {
            let address_lookup = MemoryLookup::new();
            let endpoint_1 = Endpoint::builder(Minimal)
                .address_lookup(address_lookup.clone())
                .bind()
                .await
                .unwrap();
            let endpoint_2 = Endpoint::builder(Minimal)
                .address_lookup(address_lookup.clone())
                .bind()
                .await
                .unwrap();
            address_lookup.add_endpoint_info(endpoint_1.addr());
            address_lookup.add_endpoint_info(endpoint_2.addr());
            (endpoint_1, endpoint_2)
        }

        #[cfg(target_family = "wasm")]
        {
            let endpoint_1 = Endpoint::bind(N0).await.unwrap();
            let endpoint_2 = Endpoint::bind(N0).await.unwrap();
            endpoint_1.online().await;
            endpoint_2.online().await;
            (endpoint_1, endpoint_2)
        }
    };

    /// server: stream "Pong!" 10 times
    async fn streaming_ping() -> impl IntoResponse {
        let stream = futures::stream::repeat(Ok::<Bytes, Infallible>(Bytes::from_static(b"Pong!")));
        Body::from_stream(stream.take(10))
    }

    let app = Router::new().route("/streaming-ping", get(streaming_ping));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/streaming-ping", endpoint_1.id());
    let response = client.get(&uri).send().await.unwrap();

    let mut stream = response.bytes_stream();
    let mut count = 0usize;
    while let Some(chunk) = stream.next().await.transpose().unwrap() {
        assert_eq!(chunk, b"Pong!"[..]);
        count += 1;
    }
    assert_eq!(count, 10);
}

/// Streaming request body
#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn streaming_request_body() {
    let (endpoint_1, endpoint_2) = {
        #[cfg(not(target_family = "wasm"))]
        {
            let address_lookup = MemoryLookup::new();
            let endpoint_1 = Endpoint::builder(Minimal)
                .address_lookup(address_lookup.clone())
                .bind()
                .await
                .unwrap();
            let endpoint_2 = Endpoint::builder(Minimal)
                .address_lookup(address_lookup.clone())
                .bind()
                .await
                .unwrap();
            address_lookup.add_endpoint_info(endpoint_1.addr());
            address_lookup.add_endpoint_info(endpoint_2.addr());
            (endpoint_1, endpoint_2)
        }

        #[cfg(target_family = "wasm")]
        {
            let endpoint_1 = Endpoint::bind(N0).await.unwrap();
            let endpoint_2 = Endpoint::bind(N0).await.unwrap();
            endpoint_1.online().await;
            endpoint_2.online().await;
            (endpoint_1, endpoint_2)
        }
    };

    const PING: &str = "Ping!";
    const PING_COUNT: usize = 5;
    const PONG: &str = "Pong!";
    const PONG_COUNT: usize = 7;

    async fn streaming_ping(body: Body) -> impl IntoResponse {
        let mut body_stream = body.into_data_stream();
        let mut counter = 0usize;
        while let Some(chunk) = body_stream.next().await.transpose().unwrap() {
            assert_eq!(chunk, PING.as_bytes());
            counter += 1;
        }
        assert_eq!(counter, PING_COUNT);

        let pong_bytes = Bytes::from_static(PONG.as_bytes());
        let ok = Ok::<Bytes, Infallible>(pong_bytes);
        Body::from_stream(futures::stream::repeat(ok).take(PONG_COUNT))
    }

    let app = Router::new().route("/streaming-ping", post(streaming_ping));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/streaming-ping", endpoint_1.id());

    let ping_bytes = Bytes::from_static(PING.as_bytes());
    let frame = move || Frame::data(ping_bytes.clone());
    let stream = futures::stream::repeat_with(move || Ok::<_, Infallible>(frame()));
    let body = BoxBody::new(StreamBody::new(stream.take(PING_COUNT))).into();

    let response = client.post(uri).body(body).unwrap().send().await.unwrap();
    let mut resp_stream = response.bytes_stream();
    let mut count = 0usize;
    while let Some(chunk) = resp_stream.next().await.transpose().unwrap() {
        assert_eq!(chunk, PONG.as_bytes());
        count += 1;
    }
    assert_eq!(count, PONG_COUNT);
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_send_bytes_stream_pending_drop_keeps_connection_reusable() {
    async fn delayed_body() -> impl IntoResponse {
        let pending = futures::stream::pending::<Result<Bytes, Infallible>>();
        Body::from_stream(pending)
    }

    async fn ping() -> &'static str {
        "pong"
    }

    let app = Router::new()
        .route("/delayed-body", get(delayed_body))
        .route("/ping", get(ping));
    let app = spawn_test_app(app).await;

    let body_uri = format!("iroh+h3://{}/delayed-body", app.endpoint.id());
    let ping_uri = format!("iroh+h3://{}/ping", app.endpoint.id());
    let response = app.client.get(&body_uri).send().await.unwrap();
    let mut stream = response.bytes_stream();

    {
        let mut next = Box::pin(stream.next());
        let first_poll = futures::future::poll_fn(|cx| match next.as_mut().poll(cx) {
            Poll::Ready(item) => Poll::Ready(Poll::Ready(item)),
            Poll::Pending => Poll::Ready(Poll::Pending),
        })
        .await;
        assert!(
            matches!(first_poll, Poll::Pending),
            "delayed body stream completed before it could be dropped"
        );
    }
    drop(stream);

    let ping = app
        .client
        .get(&ping_uri)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(ping, Bytes::from_static(b"pong"));
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_send_bytes_stream_one_chunk_then_drop_keeps_connection_reusable() {
    async fn one_chunk_then_pending() -> impl IntoResponse {
        let first =
            futures::stream::once(async { Ok::<Bytes, Infallible>(Bytes::from_static(b"first")) });
        let pending = futures::stream::pending::<Result<Bytes, Infallible>>();
        Body::from_stream(first.chain(pending))
    }

    async fn ping() -> &'static str {
        "pong"
    }

    let app = Router::new()
        .route("/one-then-pending", get(one_chunk_then_pending))
        .route("/ping", get(ping));
    let app = spawn_test_app(app).await;

    let body_uri = format!("iroh+h3://{}/one-then-pending", app.endpoint.id());
    let ping_uri = format!("iroh+h3://{}/ping", app.endpoint.id());
    let response = app.client.get(&body_uri).send().await.unwrap();
    let mut stream = response.bytes_stream();
    let first = stream
        .next()
        .await
        .expect("first item should exist")
        .unwrap();
    assert_eq!(first, Bytes::from_static(b"first"));
    drop(stream);

    let ping = app
        .client
        .get(&ping_uri)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(ping, Bytes::from_static(b"pong"));
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_send_bytes_complete_read_still_succeeds() {
    async fn hello() -> &'static str {
        "hello"
    }

    let app = Router::new().route("/hello", get(hello));
    let app = spawn_test_app(app).await;

    let uri = format!("iroh+h3://{}/hello", app.endpoint.id());
    let body = app
        .client
        .get(&uri)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(body, Bytes::from_static(b"hello"));
}
