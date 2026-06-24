use axum::{Router, body::Body, response::IntoResponse, routing::post};
use bytes::Bytes;
use http_body_util::BodyExt as _;
#[cfg(not(target_family = "wasm"))]
use iroh::{address_lookup::memory::MemoryLookup, endpoint::presets::Minimal};
#[cfg(target_family = "wasm")]
use iroh::endpoint::presets::N0;
use iroh::Endpoint;
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;
use wasm_bindgen_test::wasm_bindgen_test;
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const ALPN: &[u8] = b"iroh+h3";

/// Full-body convenience
#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn full_body_helpers() {
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

    async fn echo_full(body: Body) -> impl IntoResponse {
        let b = body.collect().await.unwrap();
        b.to_bytes()
    }

    let app = Router::new().route("/echo", post(echo_full));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/echo", endpoint_1.id());

    let payload = Bytes::from_static(b"hello-bytes");
    let request = client.post(&uri).bytes(payload.clone()).unwrap();
    let response = request.send().await.unwrap();

    let got = response.bytes().await.unwrap();
    assert_eq!(got, payload);
}
