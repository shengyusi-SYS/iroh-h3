use bytes::Bytes;
use iroh::Endpoint;
#[cfg(target_family = "wasm")]
use iroh::endpoint::presets::N0;
#[cfg(not(target_family = "wasm"))]
use iroh::{address_lookup::memory::MemoryLookup, endpoint::presets::Minimal};
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::{
    Body as ClientBody, IrohH3Client,
    error::Error,
    middleware::{Middleware, Service},
};

use axum::{
    Router,
    http::{HeaderMap, HeaderValue, Request, Response},
    response::IntoResponse,
    routing::get,
};
use wasm_bindgen_test::wasm_bindgen_test;
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const ALPN: &[u8] = b"iroh+h3";

struct HeaderInjectMiddleware;

impl Middleware for HeaderInjectMiddleware {
    async fn handle(
        &self,
        mut request: Request<ClientBody>,
        next: &impl Service,
    ) -> Result<Response<ClientBody>, Error> {
        request
            .headers_mut()
            .insert("x-test-middleware", HeaderValue::from_static("injected"));
        next.handle(request).await
    }
}

/// Basic request & headers
#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn basic_get_and_headers() {
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

    /// simple handler returns a static body and sets a custom header
    async fn hello() -> impl IntoResponse {
        (
            axum::response::AppendHeaders([("x-test", "value")]),
            "Hello, World!",
        )
    }

    let app = Router::new().route("/hello", get(hello));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/hello", endpoint_1.id());
    let response = client.get(&uri).send().await.unwrap();

    let header = response.headers.get("x-test").unwrap();
    assert_eq!(header, "value");

    let body = response.bytes().await.unwrap();
    assert_eq!(body, Bytes::from_static(b"Hello, World!"));
}

#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn send_uses_middleware_but_send_cancellable_keeps_direct_path() {
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

    async fn echo_middleware_header(headers: HeaderMap) -> impl IntoResponse {
        headers
            .get("x-test-middleware")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    let app = Router::new().route("/middleware", get(echo_middleware_header));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::with_middleware(endpoint_2, ALPN.into(), HeaderInjectMiddleware);
    let uri = format!("iroh+h3://{}/middleware", endpoint_1.id());

    let middleware_body = client
        .get(&uri)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(middleware_body, Bytes::from_static(b"injected"));

    let direct_body = client
        .get(&uri)
        .send_cancellable()
        .unwrap()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(direct_body, Bytes::new());
}
