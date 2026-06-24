use axum::{body::Body as AxumBody, response::IntoResponse, routing::get, Router};
use bytes::Bytes;
use futures::StreamExt;
use iroh::{endpoint::presets::N0, Endpoint};
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::{error::Error, IrohH3Client};
use wasm_bindgen_test::wasm_bindgen_test;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const ALPN: &[u8] = b"iroh+h3";

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
