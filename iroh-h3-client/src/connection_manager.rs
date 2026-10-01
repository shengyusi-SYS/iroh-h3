use std::pin::Pin;
use std::sync::{Arc, Weak};

use bytes::Buf;
use bytes::Bytes;
use dashmap::{DashMap, Entry};
use futures::FutureExt;
use futures::future::Shared;
use h3::client::Connection;
use h3::client::RequestStream;
use http::Response;
use http::Uri;
use http::Version;
use http_body_util::BodyExt;
use iroh::{Endpoint, EndpointId};
use iroh_h3::BidiStream;
use iroh_h3::{Connection as IrohH3Connection, OpenStreams};
use n0_future::task; // unifies wasm/tokio task spawning.
use tokio::sync::broadcast;
use tracing::instrument;
use tracing::trace;
use tracing::warn;

use crate::body::Body;
use crate::cancel::PendingRequest;
use crate::error::Error;
use crate::error::RequestValidationError;
use crate::middleware::Service;
use crate::response::IrohH3ResponseBody;

type Sender = h3::client::SendRequest<OpenStreams, Bytes>;
type SenderFuture = Pin<Box<dyn futures::Future<Output = Result<Sender, Arc<Error>>> + Send>>;
type CachedSender = Shared<SenderFuture>;

#[derive(Clone)]
pub struct ConnectionManager {
    endpoint: Endpoint,
    alpn: Vec<u8>,
    sender_cache: Arc<DashMap<EndpointId, CachedSender>>,
    disconnect_tx: Option<broadcast::Sender<EndpointId>>,
}

impl std::fmt::Debug for ConnectionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionManager")
            .field("alpn", &self.alpn)
            .field("sender_cache_len", &self.sender_cache.len())
            .finish()
    }
}

impl ConnectionManager {
    pub fn new(endpoint: Endpoint, alpn: Vec<u8>) -> Self {
        Self {
            endpoint,
            alpn,
            sender_cache: Default::default(),
            disconnect_tx: None,
        }
    }

    /// Set a broadcast channel sender for connection disconnect notifications.
    ///
    /// When a QUIC connection closes (graceful or timeout), the peer's `EndpointId`
    /// will be sent on this channel.
    pub fn with_disconnect_notify(mut self, tx: broadcast::Sender<EndpointId>) -> Self {
        self.disconnect_tx = Some(tx);
        self
    }

    #[instrument(skip(self, peer_id))]
    pub async fn get_sender(&self, peer_id: EndpointId) -> Result<Sender, Error> {
        let mut may_recover_cached_failure = true;
        loop {
            // Keep the read-only fast path for established connections. No map
            // guard is retained while polling the shared setup future.
            let cached = self.sender_cache.get(&peer_id).as_deref().cloned();
            let (shared, created) = match cached {
                Some(shared) => (shared, false),
                None => match self.sender_cache.entry(peer_id) {
                    Entry::Occupied(entry) => (entry.get().clone(), false),
                    Entry::Vacant(entry) => {
                        (entry.insert(self.create_connection(peer_id)).clone(), true)
                    }
                },
            };
            // Keep an unpolled identity: awaiting Shared consumes its pointer.
            match shared.clone().await {
                Ok(sender) => return Ok(sender),
                Err(error) => {
                    self.sender_cache
                        .remove_if(&peer_id, |_, cached| cached.ptr_eq(&shared));
                    if created || !may_recover_cached_failure {
                        return Err(error.into());
                    }
                    // A waiter may acquire one replacement setup before the
                    // request is sent. This preserves its body and does not
                    // replay HTTP. Repeated failures must leave the cache and
                    // return, rather than spinning until an outer timeout.
                    may_recover_cached_failure = false;
                }
            }
        }
    }

    #[instrument(skip(self, peer_id))]
    fn create_connection(&self, peer_id: EndpointId) -> Shared<SenderFuture> {
        // Neither the cached setup future nor its driver may own the cache:
        // otherwise dropping the last client cannot release the cached sender.
        let endpoint = self.endpoint.clone();
        let alpn = self.alpn.clone();
        let sender_cache = Arc::downgrade(&self.sender_cache);
        let disconnect_tx = self.disconnect_tx.clone();
        let fut: SenderFuture = Box::pin(async move {
            let conn = endpoint
                .connect(peer_id, &alpn)
                .await
                .map_err(|err| Error::Transport(err.into()))
                .map_err(Arc::new)?;
            let conn = IrohH3Connection::new(conn);
            let (conn, sender) = h3::client::new(conn)
                .await
                .map_err(|err| Error::Transport(err.into()))
                .map_err(Arc::new)?;

            // Cleanup task when connection closes
            task::spawn(Self::run_connection(
                conn,
                peer_id,
                sender_cache,
                disconnect_tx,
                endpoint,
            ));

            Ok(sender)
        });

        fut.shared()
    }

    #[instrument(skip_all)]
    async fn run_connection(
        mut conn: Connection<iroh_h3::Connection, Bytes>,
        peer_id: EndpointId,
        sender_cache: Weak<DashMap<EndpointId, CachedSender>>,
        disconnect_tx: Option<broadcast::Sender<EndpointId>>,
        endpoint: Endpoint,
    ) {
        let error = conn.wait_idle().await;
        trace!(
            "Connection with {} closed. Cause: {error}",
            peer_id.fmt_short()
        );
        if let Some(cache) = sender_cache.upgrade() {
            cache.remove(&peer_id);
        }
        if let Some(tx) = disconnect_tx {
            let _ = tx.send(peer_id);
        }
        // QUIC connections do not keep Iroh's endpoint driver alive. Responses
        // may outlive the client, so retain the endpoint until H3 has closed.
        drop(endpoint);
    }

    /// Sends an HTTP body over the given request stream.
    ///
    /// Consumes all frames emitted by the provided [`Body`] and transmits them
    /// as HTTP/3 DATA or TRAILERS frames.
    #[instrument(skip(stream, body))]
    async fn send_body(
        stream: &mut RequestStream<BidiStream<Bytes>, Bytes>,
        body: Body,
    ) -> Result<(), Error> {
        let mut body_stream = body.into_stream();
        loop {
            match body_stream.frame().await.transpose()? {
                Some(frame) if frame.is_data() => {
                    let mut data = frame
                        .into_data()
                        .expect("Non-data frame in a branch guarded by is_data");
                    let buf = data.copy_to_bytes(data.remaining());
                    stream
                        .send_data(buf)
                        .await
                        .map_err(|err| Error::Transport(err.into()))?;
                }
                Some(frame) if frame.is_trailers() => {
                    let trailers = frame
                        .into_trailers()
                        .expect("Non-trailers frame in a branch guarded by is_trailers");
                    stream
                        .send_trailers(trailers)
                        .await
                        .map_err(|err| Error::Transport(err.into()))?;
                }
                Some(_) => warn!("Unexpected frame type"),
                None => break,
            }
        }
        Ok(())
    }

    pub(crate) fn start_cancellable_request(
        &self,
        request: http::Request<Body>,
    ) -> Result<PendingRequest, Error> {
        let peer_id = peer_id(request.uri())?;
        let (mut parts, body) = request.into_parts();
        let body = body.into_fixed_bytes_for_cancellable_request()?;
        parts.version = Version::HTTP_3;
        let request = http::Request::from_parts(parts, ());
        Ok(PendingRequest::new(self.clone(), peer_id, request, body))
    }
}

impl Service for ConnectionManager {
    #[instrument(skip(self, request))]
    async fn handle(&self, mut request: http::Request<Body>) -> Result<Response<Body>, Error> {
        let peer_id = peer_id(request.uri())?;
        let mut sender = self.get_sender(peer_id).await?;

        *request.version_mut() = Version::HTTP_3;

        let (parts, body) = request.into_parts();
        let req = http::Request::from_parts(parts, ());

        let mut stream = sender
            .send_request(req)
            .await
            .map_err(|err| Error::Transport(err.into()))?;
        Self::send_body(&mut stream, body).await?;
        stream
            .finish()
            .await
            .map_err(|err| Error::Transport(err.into()))?;

        let response = stream
            .recv_response()
            .await
            .map_err(|err| Error::Transport(err.into()))?;
        let inner = response.into_parts().0;

        let response_body = IrohH3ResponseBody::new(stream, sender);
        let boxed_response_body = response_body.boxed();

        Ok(Response::from_parts(inner, boxed_response_body.into()))
    }
}

/// Extracts the [`EndpointId`] from the authority component of a URI.
///
/// # Errors
///
/// Returns:
/// - [`Error::MissingAuthority`] if the URI lacks an authority.
/// - [`Error::BadPeerId`] if the authority is not a valid [`EndpointId`].
#[instrument]
pub(crate) fn peer_id(uri: &Uri) -> Result<EndpointId, Error> {
    let authority = uri
        .authority()
        .ok_or_else(|| RequestValidationError::MissingAuthority)?
        .as_str();
    authority
        .parse()
        .map_err(|err| RequestValidationError::BadPeerId(err).into())
}

#[cfg(all(test, not(target_family = "wasm")))]
mod lifecycle_tests {
    use super::*;
    use iroh::{address_lookup::memory::MemoryLookup, endpoint::presets::Minimal};

    fn pending_setup() -> (tokio::sync::oneshot::Sender<()>, CachedSender) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let future: SenderFuture = Box::pin(async move {
            rx.await.expect("release setup failure");
            Err(Arc::new(Error::Other("controlled setup failure".into())))
        });
        (tx, future.shared())
    }

    #[tokio::test]
    async fn cancelled_waiter_allows_inflight_post_to_reconnect_with_body() {
        let lookup = MemoryLookup::new();
        let server = Endpoint::builder(Minimal).bind().await.unwrap();
        let router = iroh::protocol::Router::builder(server.clone())
            .accept(
                b"iroh+h3".to_vec(),
                iroh_h3_axum::IrohAxum::new(axum::Router::new().route(
                    "/echo",
                    axum::routing::post(|body: Bytes| async move { body }),
                )),
            )
            .spawn();
        lookup.add_endpoint_info(server.addr());
        let endpoint = Endpoint::builder(Minimal)
            .address_lookup(lookup)
            .bind()
            .await
            .unwrap();
        let manager = ConnectionManager::new(endpoint.clone(), b"iroh+h3".to_vec());
        let peer = server.id();
        let (release, setup) = pending_setup();
        manager.sender_cache.insert(peer, setup);
        {
            let request = manager.get_sender(peer);
            futures::pin_mut!(request);
            assert!(futures::poll!(request.as_mut()).is_pending());
        }
        // This POST joins the pending setup before it fails. Recovery must
        // happen inside this same request, without middleware replaying its body.
        let request = http::Request::builder()
            .method(http::Method::POST)
            .uri(format!("iroh+h3://{peer}/echo"))
            .body(Body::bytes(Bytes::from_static(b"complete-original-body")))
            .unwrap();
        let response = manager.handle(request);
        futures::pin_mut!(response);
        assert!(futures::poll!(response.as_mut()).is_pending());
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let response = response.await.unwrap();
            assert_eq!(response.status(), http::StatusCode::OK);
            let body = response.into_body().into_bytes().await.unwrap();
            assert_eq!(&body[..], b"complete-original-body");
        })
        .await
        .expect("inflight POST must reconnect after failed cached setup");
        endpoint.close().await;
        router.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn old_setup_failure_joins_replacement_but_stops_after_second_failure() {
        let endpoint = Endpoint::builder(Minimal).bind().await.unwrap();
        let manager = ConnectionManager::new(endpoint.clone(), b"iroh+h3".to_vec());
        let peer = endpoint.id();
        let (release, setup) = pending_setup();
        manager.sender_cache.insert(peer, setup);
        let request = manager.get_sender(peer);
        futures::pin_mut!(request);
        assert!(futures::poll!(request.as_mut()).is_pending());
        let (replacement_release, replacement) = pending_setup();
        manager.sender_cache.insert(peer, replacement.clone());
        release.send(()).unwrap();
        assert!(futures::poll!(request.as_mut()).is_pending());
        assert!(
            manager
                .sender_cache
                .get(&peer)
                .unwrap()
                .ptr_eq(&replacement)
        );
        replacement_release.send(()).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), request).await;
        assert!(matches!(result, Ok(Err(Error::Shared(ref error)))
            if error.to_string() == "controlled setup failure"));
        assert!(!manager.sender_cache.contains_key(&peer));
        endpoint.close().await;
    }

    #[tokio::test]
    async fn cancelled_connection_setup_releases_cache() {
        let lookup = MemoryLookup::new();
        // No accept loop: the outgoing connection remains in setup.
        let server = Endpoint::builder(Minimal)
            .alpns(vec![b"iroh+h3".to_vec()])
            .bind()
            .await
            .unwrap();
        lookup.add_endpoint_info(server.addr());
        let endpoint = Endpoint::builder(Minimal)
            .address_lookup(lookup)
            .bind()
            .await
            .unwrap();
        let manager = ConnectionManager::new(endpoint.clone(), b"iroh+h3".to_vec());
        let cache = Arc::downgrade(&manager.sender_cache);
        {
            let setup = manager.get_sender(server.id());
            futures::pin_mut!(setup);
            assert!(futures::poll!(setup).is_pending());
        }
        drop(manager);
        assert!(
            cache.upgrade().is_none(),
            "cancelled setup retained its own cache"
        );
        server.close().await;
        endpoint.close().await;
    }
}
