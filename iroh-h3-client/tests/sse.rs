use std::convert::Infallible;

use axum::{
    Router,
    response::{IntoResponse, Sse, sse::Event},
    routing::get,
};
use futures::{StreamExt, stream::repeat};
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

/// Server-Sent Events
#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn sse_stream() {
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
        let event = Event::default().data("some data");
        let event_result = Ok::<_, Infallible>(event);
        let stream = repeat(event_result);
        Sse::new(stream.take(10))
    }

    let app = Router::new().route("/hello", get(hello));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/hello", endpoint_1.id());
    let response = client.get(&uri).send().await.unwrap();

    let mut sse_stream = response.sse_stream();
    let mut counter = 0;
    while let Some(event) = sse_stream.next().await.transpose().unwrap() {
        counter += 1;
        assert_eq!(event.data(), "some data");
    }
    assert_eq!(counter, 10);
}

#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn sse_stream_edge_cases() {
    use futures::stream::{self, StreamExt};

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

    async fn edge_case_handler() -> impl IntoResponse {
        let events = vec![
            Event::default().data("simple"),
            Event::default().data("line1\nline2"),
            Event::default().data(""), // empty data line
            Event::default().data("payload").id("42").event("custom"),
        ];

        let stream = stream::iter(events.into_iter().map(Ok::<_, Infallible>));
        Sse::new(stream)
    }

    let app = Router::new().route("/sse", get(edge_case_handler));
    let _router = iroh::protocol::Router::builder(endpoint_1.clone())
        .accept(ALPN, IrohAxum::new(app))
        .spawn();

    let client = IrohH3Client::new(endpoint_2, ALPN.into());
    let uri = format!("iroh+h3://{}/sse", endpoint_1.id());
    let response = client.get(&uri).send().await.unwrap();

    let mut sse_stream = response.sse_stream();
    let mut events = Vec::new();
    while let Some(event) = sse_stream.next().await.transpose().unwrap() {
        events.push(event);
    }

    assert_eq!(events.len(), 4);
    assert_eq!(events[0].data(), "simple");
    assert_eq!(events[1].data(), "line1\nline2");
    assert_eq!(events[2].data(), "");
    assert_eq!(events[3].id(), Some("42"));
    assert_eq!(events[3].event(), Some("custom"));
    assert_eq!(events[3].data(), "payload");
}
