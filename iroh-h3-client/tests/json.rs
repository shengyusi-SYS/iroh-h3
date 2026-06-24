#![cfg(feature = "json")]

use axum::{Json, Router, response::IntoResponse, routing::post};
#[cfg(not(target_family = "wasm"))]
use iroh::{address_lookup::memory::MemoryLookup, endpoint::presets::Minimal};
#[cfg(target_family = "wasm")]
use iroh::endpoint::presets::N0;
use iroh::Endpoint;
use iroh_h3_axum::IrohAxum;
use iroh_h3_client::IrohH3Client;

use serde::{Deserialize, Serialize};
use wasm_bindgen_test::wasm_bindgen_test;
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const ALPN: &[u8] = b"iroh+h3";

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Message {
    message: String,
}

const PING: &str = "Ping!";
const PONG: &str = "Pong!";

#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn json_request_response() {
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

    async fn ping(Json(msg): Json<Message>) -> impl IntoResponse {
        assert_eq!(msg.message, PING);
        Json(Message {
            message: PONG.into(),
        })
    }

    let app = Router::new().route("/ping", post(ping));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/ping", endpoint_1.id());

    let req = client
        .post(&uri)
        .json(&Message {
            message: PING.into(),
        })
        .unwrap();
    let res = req.send().await.unwrap();

    assert_eq!(res.headers.get("Content-Type").unwrap(), "application/json");

    let reply: Message = res.json().await.unwrap();
    assert_eq!(
        reply,
        Message {
            message: PONG.into()
        }
    );
}
