#[cfg(not(target_family = "wasm"))]
use iroh::endpoint::presets::Minimal;
#[cfg(target_family = "wasm")]
use iroh::endpoint::presets::N0;
use iroh::{Endpoint, EndpointId};
use iroh_h3_client::IrohH3Client;
use wasm_bindgen_test::wasm_bindgen_test;
wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

const ALPN: &[u8] = b"iroh+h3";

/// Error handling
#[cfg_attr(not(target_family = "wasm"), tokio::test)]
#[wasm_bindgen_test]
async fn error_handling_unresolvable_peer() {
    #[cfg(not(target_family = "wasm"))]
    let endpoint = Endpoint::builder(Minimal).bind().await.unwrap();

    #[cfg(target_family = "wasm")]
    let endpoint = Endpoint::bind(N0).await.unwrap();

    let client = IrohH3Client::new(endpoint, ALPN.into());

    let fake_id = EndpointId::from_bytes(b"fsdgh righrfdruigrfiuyrghsidugjm").unwrap();
    let uri = format!("iroh+h3://{}/ping", fake_id);

    let res = client.get(&uri).send().await;
    assert!(
        res.is_err(),
        "expected error when sending to an unresolvable peer, got Ok"
    );
}
