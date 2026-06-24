use axum::{Router, body::Body as AxumBody, response::IntoResponse, routing::get};
use bytes::Bytes;
use futures::StreamExt;
use iroh::{Endpoint, endpoint::presets::N0};
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::{IrohH3Client, error::Error};
use wasm_bindgen_test::wasm_bindgen_test;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const ALPN: &[u8] = b"iroh+h3";

#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn send_cancellable_returns_response_with_cancellable_body() {
    let endpoint_1 = Endpoint::bind(N0).await.unwrap();
    let endpoint_2 = Endpoint::bind(N0).await.unwrap();
    endpoint_1.online().await;
    endpoint_2.online().await;

    async fn hello() -> &'static str {
        "hello"
    }

    let app = Router::new().route("/hello", get(hello));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/hello", endpoint_1.id());
    let pending = client.get(&uri).send_cancellable().unwrap();
    let handle = pending.cancel_handle();
    assert!(!handle.is_cancelled());

    let response = pending.await.unwrap();
    let mut stream = response.cancellable_bytes_stream().unwrap();
    assert_eq!(
        stream.next().await.transpose().unwrap().unwrap(),
        Bytes::from_static(b"hello")
    );
    assert!(stream.next().await.is_none());
}

#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn body_cancel_returns_cancelled_once() {
    let endpoint_1 = Endpoint::bind(N0).await.unwrap();
    let endpoint_2 = Endpoint::bind(N0).await.unwrap();
    endpoint_1.online().await;
    endpoint_2.online().await;

    async fn streaming() -> impl IntoResponse {
        let chunk = Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"chunk"));
        AxumBody::from_stream(futures::stream::repeat(chunk).take(64))
    }

    let app = Router::new().route("/stream", get(streaming));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/stream", endpoint_1.id());
    let response = client.get(&uri).send_cancellable().unwrap().await.unwrap();
    let mut stream = response.cancellable_bytes_stream().unwrap();
    let handle = stream.cancel_handle();

    handle.cancel();

    let first = stream.next().await;
    let second = stream.next().await;

    assert!(matches!(first, Some(Err(Error::Cancelled))));
    assert!(second.is_none());
}

#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn cancel_keeps_connection_reusable() {
    let endpoint_1 = Endpoint::bind(N0).await.unwrap();
    let endpoint_2 = Endpoint::bind(N0).await.unwrap();
    endpoint_1.online().await;
    endpoint_2.online().await;

    async fn stream() -> impl IntoResponse {
        let chunk = Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"chunk"));
        AxumBody::from_stream(futures::stream::repeat(chunk).take(128))
    }

    async fn ping() -> &'static str {
        "pong"
    }

    let app = Router::new()
        .route("/stream", get(stream))
        .route("/ping", get(ping));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let stream_uri = format!("iroh+h3://{}/stream", endpoint_1.id());
    let ping_uri = format!("iroh+h3://{}/ping", endpoint_1.id());

    let response = client
        .get(&stream_uri)
        .send_cancellable()
        .unwrap()
        .await
        .unwrap();
    let mut stream = response.cancellable_bytes_stream().unwrap();
    stream.cancel_handle().cancel();
    assert!(matches!(stream.next().await, Some(Err(Error::Cancelled))));
    drop(stream);

    let ping = client
        .get(&ping_uri)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(ping, Bytes::from_static(b"pong"));
}

#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn cancellable_response_legacy_bytes_stream_ignores_handle_cancel() {
    let endpoint_1 = Endpoint::bind(N0).await.unwrap();
    let endpoint_2 = Endpoint::bind(N0).await.unwrap();
    endpoint_1.online().await;
    endpoint_2.online().await;

    async fn stream() -> impl IntoResponse {
        let chunks = futures::stream::iter([
            Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"a")),
            Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"b")),
        ]);
        AxumBody::from_stream(chunks)
    }

    let app = Router::new().route("/stream", get(stream));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/stream", endpoint_1.id());
    let pending = client.get(&uri).send_cancellable().unwrap();
    let handle = pending.cancel_handle();
    let response = pending.await.unwrap();
    let mut stream = response.bytes_stream();

    handle.cancel();

    let first = stream.next().await.transpose().unwrap().unwrap();
    let second = stream.next().await.transpose().unwrap().unwrap();
    assert_eq!(first, Bytes::from_static(b"a"));
    assert_eq!(second, Bytes::from_static(b"b"));
    assert!(stream.next().await.is_none());
}

#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn non_cancellable_response_returns_body_not_cancellable() {
    let endpoint_1 = Endpoint::bind(N0).await.unwrap();
    let endpoint_2 = Endpoint::bind(N0).await.unwrap();
    endpoint_1.online().await;
    endpoint_2.online().await;

    async fn hello() -> impl IntoResponse {
        "hello"
    }

    let app = Router::new().route("/hello", get(hello));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/hello", endpoint_1.id());
    let response = client.get(&uri).send().await.unwrap();
    let result = response.cancellable_bytes_stream();

    assert!(matches!(result, Err(Error::BodyNotCancellable)));
}
