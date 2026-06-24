use axum::{Router, body::Body as AxumBody, response::IntoResponse, routing::get};
use bytes::Bytes;
use futures::StreamExt;
use http_body::Frame;
use http_body_util::{StreamBody, combinators::BoxBody};
use iroh::{Endpoint, endpoint::presets::N0};
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::{
    CancellableBytesStream, IrohH3Client, PendingRequest, RequestCancelHandle, error::Error,
};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use wasm_bindgen_test::wasm_bindgen_test;

wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const ALPN: &[u8] = b"iroh+h3";

#[cfg(not(target_family = "wasm"))]
#[test]
fn cancellable_public_types_have_required_thread_bounds() {
    fn assert_send<T: Send>() {}
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_clone<T: Clone>() {}

    assert_send::<PendingRequest>();
    assert_send::<CancellableBytesStream>();
    assert_send_sync::<RequestCancelHandle>();
    assert_clone::<RequestCancelHandle>();
}

#[cfg(not(target_family = "wasm"))]
struct TestApp {
    endpoint: Endpoint,
    client: IrohH3Client,
    _router: iroh::protocol::Router,
}

#[cfg(not(target_family = "wasm"))]
async fn spawn_test_app(app: Router) -> TestApp {
    let server_endpoint = Endpoint::bind(N0).await.unwrap();
    let client_endpoint = Endpoint::bind(N0).await.unwrap();
    server_endpoint.online().await;
    client_endpoint.online().await;

    let router = iroh::protocol::Router::builder(server_endpoint.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    TestApp {
        endpoint: server_endpoint,
        client: IrohH3Client::new(client_endpoint, ALPN.into()),
        _router: router,
    }
}

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
async fn headers_pending_cancel_stops_waiting() {
    let endpoint_1 = Endpoint::bind(N0).await.unwrap();
    let endpoint_2 = Endpoint::bind(N0).await.unwrap();
    endpoint_1.online().await;
    endpoint_2.online().await;

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let entered_tx = Arc::new(Mutex::new(Some(entered_tx)));

    let app = Router::new().route(
        "/late",
        get({
            let entered_tx = entered_tx.clone();
            move || {
                let entered_tx = entered_tx.clone();
                async move {
                    if let Some(tx) = entered_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    n0_future::time::sleep(std::time::Duration::from_millis(200)).await;
                    "late"
                }
            }
        }),
    );
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/late", endpoint_1.id());
    let mut pending = Box::pin(client.get(&uri).send_cancellable().unwrap());
    let handle = pending.cancel_handle();
    let mut entered_rx = Box::pin(entered_rx);

    futures::future::poll_fn(|cx| {
        if let Poll::Ready(result) = entered_rx.as_mut().poll(cx) {
            result.unwrap();
            return Poll::Ready(());
        }
        if let Poll::Ready(result) = pending.as_mut().poll(cx) {
            match result {
                Ok(_) => panic!("request completed before server entered delayed handler"),
                Err(err) => {
                    panic!("request failed before server entered delayed handler: {err:?}")
                }
            }
        }
        Poll::Pending
    })
    .await;
    handle.cancel();

    let result = pending.await;
    assert!(matches!(result, Err(Error::Cancelled)));
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_pending_request_can_be_cancelled_from_external_handle() {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let entered_tx = Arc::new(Mutex::new(Some(entered_tx)));

    let app = Router::new().route(
        "/late",
        get({
            let entered_tx = entered_tx.clone();
            move || {
                let entered_tx = entered_tx.clone();
                async move {
                    if let Some(tx) = entered_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    n0_future::time::sleep(std::time::Duration::from_secs(60)).await;
                    "late"
                }
            }
        }),
    );
    let app = spawn_test_app(app).await;

    let uri = format!("iroh+h3://{}/late", app.endpoint.id());
    let pending = app.client.get(&uri).send_cancellable().unwrap();
    let handle = pending.cancel_handle();
    let task = tokio::spawn(pending);

    tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx)
        .await
        .expect("server handler was not entered before timeout")
        .expect("server handler sender was dropped");

    handle.cancel();

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("spawned pending request did not finish after cancellation")
        .expect("spawned pending request task panicked");

    assert!(matches!(result, Err(Error::Cancelled)));
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
async fn body_read_pending_cancel_or_drop_is_safe() {
    let endpoint_1 = Endpoint::bind(N0).await.unwrap();
    let endpoint_2 = Endpoint::bind(N0).await.unwrap();
    endpoint_1.online().await;
    endpoint_2.online().await;

    async fn delayed_body() -> impl IntoResponse {
        let stream = futures::stream::unfold(false, |sent| async move {
            if sent {
                None
            } else {
                n0_future::time::sleep(std::time::Duration::from_millis(200)).await;
                Some((
                    Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"late")),
                    true,
                ))
            }
        });
        AxumBody::from_stream(stream)
    }

    async fn ping() -> &'static str {
        "pong"
    }

    let app = Router::new()
        .route("/delayed-body", get(delayed_body))
        .route("/ping", get(ping));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let body_uri = format!("iroh+h3://{}/delayed-body", endpoint_1.id());
    let ping_uri = format!("iroh+h3://{}/ping", endpoint_1.id());
    let response = client
        .get(&body_uri)
        .send_cancellable()
        .unwrap()
        .await
        .unwrap();
    let stream = response.cancellable_bytes_stream().unwrap();
    let handle = stream.cancel_handle();

    handle.cancel();
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
async fn cancel_after_response_before_body_stream_then_convert_returns_cancelled() {
    let endpoint_1 = Endpoint::bind(N0).await.unwrap();
    let endpoint_2 = Endpoint::bind(N0).await.unwrap();
    endpoint_1.online().await;
    endpoint_2.online().await;

    async fn stream() -> impl IntoResponse {
        let chunk = Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"chunk"));
        AxumBody::from_stream(futures::stream::repeat(chunk).take(8))
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

    handle.cancel();
    let mut stream = response.cancellable_bytes_stream().unwrap();

    assert!(matches!(stream.next().await, Some(Err(Error::Cancelled))));
    assert!(stream.next().await.is_none());
}

#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn cancel_after_response_before_body_stream_then_drop_cleans_up() {
    let endpoint_1 = Endpoint::bind(N0).await.unwrap();
    let endpoint_2 = Endpoint::bind(N0).await.unwrap();
    endpoint_1.online().await;
    endpoint_2.online().await;

    async fn stream() -> impl IntoResponse {
        let chunk = Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"chunk"));
        AxumBody::from_stream(futures::stream::repeat(chunk).take(32))
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
    let pending = client.get(&stream_uri).send_cancellable().unwrap();
    let handle = pending.cancel_handle();
    let response = pending.await.unwrap();

    handle.cancel();
    drop(response);

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

#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn streaming_request_body_send_cancellable_returns_request_body_not_cancellable() {
    let endpoint = Endpoint::bind(N0).await.unwrap();
    endpoint.online().await;

    let client = IrohH3Client::new(endpoint.clone(), ALPN.into());
    let uri = format!("iroh+h3://{}/upload", endpoint.id());
    let stream = futures::stream::iter([Ok::<_, Error>(Frame::data(Bytes::from_static(b"x")))]);
    let body = BoxBody::new(StreamBody::new(stream)).into();

    let result = client.post(&uri).body(body).unwrap().send_cancellable();

    assert!(matches!(result, Err(Error::RequestBodyNotCancellable)));
}
