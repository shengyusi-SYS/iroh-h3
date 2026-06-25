use axum::{Router, body::Body as AxumBody, response::IntoResponse, routing::get};
use bytes::Bytes;
use futures::StreamExt;
use http_body::Frame;
use http_body_util::{StreamBody, combinators::BoxBody};
use iroh::{Endpoint, endpoint::presets::N0};
#[cfg(not(target_family = "wasm"))]
use iroh::{address_lookup::memory::MemoryLookup, endpoint::presets::Minimal};
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

struct TestApp {
    endpoint: Endpoint,
    client: IrohH3Client,
    _router: iroh::protocol::Router,
}

async fn spawn_test_app(app: Router) -> TestApp {
    #[cfg(not(target_family = "wasm"))]
    {
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

    #[cfg(target_family = "wasm")]
    {
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
}

#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn send_cancellable_returns_response_with_cancellable_body() {
    async fn hello() -> &'static str {
        "hello"
    }

    let app = Router::new().route("/hello", get(hello));
    let app = spawn_test_app(app).await;

    let uri = format!("iroh+h3://{}/hello", app.endpoint.id());
    let pending = app.client.get(&uri).send_cancellable().unwrap();
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
    async fn streaming() -> impl IntoResponse {
        let chunk = Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"chunk"));
        AxumBody::from_stream(futures::stream::repeat(chunk).take(64))
    }

    let app = Router::new().route("/stream", get(streaming));
    let app = spawn_test_app(app).await;

    let uri = format!("iroh+h3://{}/stream", app.endpoint.id());
    let response = app
        .client
        .get(&uri)
        .send_cancellable()
        .unwrap()
        .await
        .unwrap();
    let mut stream = response.cancellable_bytes_stream().unwrap();
    let handle = stream.cancel_handle();

    handle.cancel();

    let first = stream.next().await;
    let second = stream.next().await;

    assert!(matches!(first, Some(Err(Error::Cancelled))));
    assert!(second.is_none());
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_cancellable_body_stream_can_be_cancelled_from_external_handle() {
    async fn delayed_body() -> impl IntoResponse {
        let stream = futures::stream::unfold((), |_| async move {
            n0_future::time::sleep(std::time::Duration::from_secs(60)).await;
            Some((
                Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"late")),
                (),
            ))
        });
        AxumBody::from_stream(stream)
    }

    let app = Router::new().route("/delayed-body", get(delayed_body));
    let app = spawn_test_app(app).await;

    let uri = format!("iroh+h3://{}/delayed-body", app.endpoint.id());
    let response = app
        .client
        .get(&uri)
        .send_cancellable()
        .unwrap()
        .await
        .unwrap();
    let stream = response.cancellable_bytes_stream().unwrap();
    let handle = stream.cancel_handle();
    let (polling_tx, polling_rx) = tokio::sync::oneshot::channel();

    let task = tokio::spawn(async move {
        let mut stream = stream;
        let mut next = Box::pin(stream.next());
        let mut polling_tx = Some(polling_tx);

        let first_poll = futures::future::poll_fn(|cx| match next.as_mut().poll(cx) {
            Poll::Ready(item) => Poll::Ready(Some(item)),
            Poll::Pending => {
                if let Some(tx) = polling_tx.take() {
                    let _ = tx.send(());
                }
                Poll::Ready(None)
            }
        })
        .await;

        if let Some(item) = first_poll {
            return item;
        }

        next.await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), polling_rx)
        .await
        .expect("spawned body task did not start before timeout")
        .expect("spawned body task dropped startup sender");

    handle.cancel();

    let item = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("spawned body stream did not finish after cancellation")
        .expect("spawned body stream task panicked");

    assert!(matches!(item, Some(Err(Error::Cancelled))));
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_cancellable_body_cancel_returns_cancelled_and_reuses_connection() {
    async fn delayed_body() -> impl IntoResponse {
        let stream = futures::stream::unfold((), |_| async move {
            n0_future::time::sleep(std::time::Duration::from_secs(60)).await;
            Some((
                Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"late")),
                (),
            ))
        });
        AxumBody::from_stream(stream)
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
    let response = app
        .client
        .get(&body_uri)
        .send_cancellable()
        .unwrap()
        .await
        .unwrap();
    let stream = response.cancellable_bytes_stream().unwrap();
    let handle = stream.cancel_handle();
    let (polling_tx, polling_rx) = tokio::sync::oneshot::channel();

    let task = tokio::spawn(async move {
        let mut stream = stream;
        let mut next = Box::pin(stream.next());
        let mut polling_tx = Some(polling_tx);

        let first_poll = futures::future::poll_fn(|cx| match next.as_mut().poll(cx) {
            Poll::Ready(item) => Poll::Ready(Some(item)),
            Poll::Pending => {
                if let Some(tx) = polling_tx.take() {
                    let _ = tx.send(());
                }
                Poll::Ready(None)
            }
        })
        .await;

        if let Some(item) = first_poll {
            return item;
        }

        next.await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), polling_rx)
        .await
        .expect("body stream did not enter pending poll")
        .expect("pending poll sender dropped");

    handle.cancel();

    let item = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("body stream did not finish after cancellation")
        .expect("body task panicked");
    assert!(matches!(item, Some(Err(Error::Cancelled))));

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
async fn cancellable_body_cancel_after_one_chunk_returns_cancelled() {
    async fn one_chunk_then_pending() -> impl IntoResponse {
        let first = futures::stream::once(async {
            Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"first"))
        });
        let pending = futures::stream::pending::<Result<Bytes, std::convert::Infallible>>();
        AxumBody::from_stream(first.chain(pending))
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
    let response = app
        .client
        .get(&body_uri)
        .send_cancellable()
        .unwrap()
        .await
        .unwrap();

    let mut stream = response.cancellable_bytes_stream().unwrap();
    let handle = stream.cancel_handle();
    let first = stream
        .next()
        .await
        .expect("first item should exist")
        .unwrap();
    assert_eq!(first, Bytes::from_static(b"first"));

    let (polling_tx, polling_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut next = Box::pin(stream.next());
        let mut polling_tx = Some(polling_tx);
        futures::future::poll_fn(|cx| match next.as_mut().poll(cx) {
            Poll::Ready(item) => Poll::Ready(item),
            Poll::Pending => {
                if let Some(tx) = polling_tx.take() {
                    let _ = tx.send(());
                }
                Poll::Pending
            }
        })
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(5), polling_rx)
        .await
        .expect("body stream did not enter pending poll")
        .expect("pending poll sender dropped");

    handle.cancel();
    let item = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("body stream did not finish after cancellation")
        .expect("body task panicked");
    assert!(matches!(item, Some(Err(Error::Cancelled))));

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

#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn headers_pending_cancel_stops_waiting() {
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
    let app = spawn_test_app(app).await;

    let uri = format!("iroh+h3://{}/late", app.endpoint.id());
    let mut pending = Box::pin(app.client.get(&uri).send_cancellable().unwrap());
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
    let app = spawn_test_app(app).await;

    let stream_uri = format!("iroh+h3://{}/stream", app.endpoint.id());
    let ping_uri = format!("iroh+h3://{}/ping", app.endpoint.id());

    let response = app
        .client
        .get(&stream_uri)
        .send_cancellable()
        .unwrap()
        .await
        .unwrap();
    let mut stream = response.cancellable_bytes_stream().unwrap();
    stream.cancel_handle().cancel();
    assert!(matches!(stream.next().await, Some(Err(Error::Cancelled))));
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

#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn body_read_pending_cancel_or_drop_is_safe() {
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
    let app = spawn_test_app(app).await;

    let body_uri = format!("iroh+h3://{}/delayed-body", app.endpoint.id());
    let ping_uri = format!("iroh+h3://{}/ping", app.endpoint.id());
    let response = app
        .client
        .get(&body_uri)
        .send_cancellable()
        .unwrap()
        .await
        .unwrap();
    let stream = response.cancellable_bytes_stream().unwrap();
    let handle = stream.cancel_handle();

    handle.cancel();
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

#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn cancel_after_response_before_body_stream_then_convert_returns_cancelled() {
    async fn stream() -> impl IntoResponse {
        let chunk = Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"chunk"));
        AxumBody::from_stream(futures::stream::repeat(chunk).take(8))
    }

    let app = Router::new().route("/stream", get(stream));
    let app = spawn_test_app(app).await;

    let uri = format!("iroh+h3://{}/stream", app.endpoint.id());
    let pending = app.client.get(&uri).send_cancellable().unwrap();
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
    let app = spawn_test_app(app).await;

    let stream_uri = format!("iroh+h3://{}/stream", app.endpoint.id());
    let ping_uri = format!("iroh+h3://{}/ping", app.endpoint.id());
    let pending = app.client.get(&stream_uri).send_cancellable().unwrap();
    let handle = pending.cancel_handle();
    let response = pending.await.unwrap();

    handle.cancel();
    drop(response);

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

#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn cancellable_response_legacy_bytes_stream_ignores_handle_cancel() {
    async fn stream() -> impl IntoResponse {
        let chunks = futures::stream::iter([
            Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"a")),
            Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(b"b")),
        ]);
        AxumBody::from_stream(chunks)
    }

    let app = Router::new().route("/stream", get(stream));
    let app = spawn_test_app(app).await;

    let uri = format!("iroh+h3://{}/stream", app.endpoint.id());
    let pending = app.client.get(&uri).send_cancellable().unwrap();
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
    async fn hello() -> impl IntoResponse {
        "hello"
    }

    let app = Router::new().route("/hello", get(hello));
    let app = spawn_test_app(app).await;

    let uri = format!("iroh+h3://{}/hello", app.endpoint.id());
    let response = app.client.get(&uri).send().await.unwrap();
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
