# Local H3 source

Baseline: crates.io h3 0.0.8, MIT (see LICENSE), original registry archive source. Published archive checksum: `10872b55cfb02a821b69dc7cf8dc6a71d6af25eb9a79662bec4a9d016056b3be`.
Upstream: https://github.com/hyperium/h3/tree/h3-v0.0.8

This source is shared by all three iroh-h3 workspace members using relative path dependencies. Do not mix it with registry h3 types in consumers. It is excluded as a workspace member; its upstream unit-test suite is not this fork's test target. Behavior is checked through the actual transport in the application network-lab and fork lifecycle tests.

Local deltas:
- Optional `quic::SendStream::poll_stopped`, default Pending for unchanged adapters. `BufRecvStream` / `FrameStream` forward it; server `RequestStream` exposes it. Contract: peer STOP_SENDING code, acknowledged completion, or transport failure, independently of application body polling. No extra task, timer, or global registry. Addresses upstream issue https://github.com/hyperium/h3/issues/350 (still open at integration).
- `stream::write` leading readiness poll from https://github.com/hyperium/h3/pull/360 (merged 14a14224242862de31e780a7907fe8839b893fd6). Its adapter finish-flush counterpart lives in irohh3's SendStream, not this source tree.

No #365 resolver accounting changes are included. Original package sources, README and LICENSE are otherwise retained. Formatting changes are restricted to edited declarations. Remove local deltas when an upstream release provides equivalent cancellation and buffered-write contracts, after running the same regressions.

Regression entry point (native, local endpoints):

```sh
cargo test --workspace --all-features --locked --offline
```

`iroh-h3-axum/tests/response_lifetime.rs` checks that a cancelled pending body is dropped, an already-running handler still completes, and a failed body is not a successful empty response. Each case also checks a subsequent request on the same accepted connection. Existing client tests cover streaming upload/download, late bodies surviving temporary clients, explicit cancellation, JSON/SSE and connection reuse. Buffered frame/FIN regressions and repeated cancellation allocation checks remain in the application's isolated `transport-stall-diagnostics/network-lab`; no diagnostic counters or test-only timers are added to this fork's production code.
