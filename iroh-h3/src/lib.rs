//! # iroh-h3: HTTP/3 support over iroh P2P connections
//!
//! `iroh-h3` provides low-level integration for running HTTP/3 over
//! [`iroh`](https://docs.rs/iroh) peer-to-peer QUIC connections.
//! It implements the traits required by the `h3` crate on top of `iroh`
//! connections and streams. This crate is intended for internal use in building
//! HTTP/3 over P2P layers.
//!
//! # License
//!
//! This crate is MIT licensed. Portions of the code are derived from
//! [`hyperium/h3`](https://github.com/hyperium/h3) and are reproduced under
//! the original MIT license terms.

#![deny(missing_docs)]

use std::{
    convert::TryInto,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{self, Poll},
};

use bytes::{Buf, Bytes};
use futures::{
    Stream, StreamExt, ready,
    stream::{self},
};
use h3::{
    error::Code,
    quic::{self, ConnectionErrorIncoming, StreamErrorIncoming, StreamId, WriteBuf},
};
pub use iroh::endpoint::{AcceptBi, AcceptUni, Endpoint, OpenBi, OpenUni, VarInt};
use iroh::endpoint::{ConnectionError, ReadError, WriteError};

/// BoxStream type alias with `Sync` and `Send` requirements.
type BoxStreamSync<'a, T> = Pin<Box<dyn Stream<Item = T> + Sync + Send + 'a>>;

/// A wrapper around an [`iroh::endpoint::Connection`] that implements the
/// [`h3::quic::Connection`] trait for use in HTTP/3 over QUIC.
///
/// This struct manages incoming and outgoing unidirectional and bidirectional
/// streams and handles conversions between `iroh` and `h3` errors.
pub struct Connection {
    conn: iroh::endpoint::Connection,
    incoming_bi: BoxStreamSync<'static, <AcceptBi<'static> as Future>::Output>,
    opening_bi: Option<BoxStreamSync<'static, <OpenBi<'static> as Future>::Output>>,
    incoming_uni: BoxStreamSync<'static, <AcceptUni<'static> as Future>::Output>,
    opening_uni: Option<BoxStreamSync<'static, <OpenUni<'static> as Future>::Output>>,
}

impl Connection {
    /// Creates a new [`Connection`] from an existing [`iroh::endpoint::Connection`].
    ///
    /// This sets up async streams for accepting incoming unidirectional and
    /// bidirectional QUIC streams.
    ///
    /// # Arguments
    ///
    /// * `conn` - The underlying `iroh` connection to wrap.
    pub fn new(conn: iroh::endpoint::Connection) -> Self {
        Self {
            conn: conn.clone(),
            incoming_bi: Box::pin(stream::unfold(conn.clone(), |conn| async {
                Some((conn.accept_bi().await, conn))
            })),
            opening_bi: None,
            incoming_uni: Box::pin(stream::unfold(conn.clone(), |conn| async {
                Some((conn.accept_uni().await, conn))
            })),
            opening_uni: None,
        }
    }
}

impl<B> quic::Connection<B> for Connection
where
    B: Buf,
{
    type RecvStream = RecvStream;
    type OpenStreams = OpenStreams;

    /// Polls for an incoming bidirectional stream (accepts a stream).
    ///
    /// Returns a pair of [`SendStream`] and [`RecvStream`] wrapped in
    /// [`BidiStream`].
    fn poll_accept_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::BidiStream, ConnectionErrorIncoming>> {
        let (send, recv) = ready!(self.incoming_bi.poll_next_unpin(cx))
            .expect("self.incoming_bi BoxStream never returns None")
            .map_err(convert_connection_error)?;
        Poll::Ready(Ok(Self::BidiStream {
            send: Self::SendStream::new(send),
            recv: Self::RecvStream::new(recv),
        }))
    }

    /// Polls for an incoming unidirectional receive stream.
    ///
    /// Returns a [`RecvStream`] once available.
    fn poll_accept_recv(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::RecvStream, ConnectionErrorIncoming>> {
        let recv = ready!(self.incoming_uni.poll_next_unpin(cx))
            .expect("self.incoming_uni BoxStream never returns None")
            .map_err(convert_connection_error)?;
        Poll::Ready(Ok(Self::RecvStream::new(recv)))
    }

    /// Returns a new [`OpenStreams`] handle for opening outgoing streams.
    fn opener(&self) -> Self::OpenStreams {
        OpenStreams {
            conn: self.conn.clone(),
            opening_bi: None,
            opening_uni: None,
        }
    }
}

/// Converts an [`iroh::endpoint::ConnectionError`] to an [`h3::quic::ConnectionErrorIncoming`].
fn convert_connection_error(e: ConnectionError) -> h3::quic::ConnectionErrorIncoming {
    match e {
        ConnectionError::ApplicationClosed(application_close) => {
            ConnectionErrorIncoming::ApplicationClose {
                error_code: application_close.error_code.into(),
            }
        }
        ConnectionError::TimedOut => ConnectionErrorIncoming::Timeout,
        error @ ConnectionError::VersionMismatch
        | error @ ConnectionError::Reset
        | error @ ConnectionError::LocallyClosed
        | error @ ConnectionError::CidsExhausted
        | error @ ConnectionError::TransportError(_)
        | error @ ConnectionError::ConnectionClosed(_) => {
            ConnectionErrorIncoming::Undefined(Arc::new(error))
        }
    }
}

impl<B> quic::OpenStreams<B> for Connection
where
    B: Buf,
{
    type SendStream = SendStream<B>;
    type BidiStream = BidiStream<B>;

    /// Attempts to open a new bidirectional stream for sending and receiving.
    ///
    /// Returns a [`BidiStream`] once ready, or a [`StreamErrorIncoming`] on failure.
    fn poll_open_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        let bi = self.opening_bi.get_or_insert_with(|| {
            Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((conn.open_bi().await, conn))
            }))
        });
        let (send, recv) = ready!(bi.poll_next_unpin(cx))
            .expect("BoxStream does not return None")
            .map_err(|e| StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_connection_error(e),
            })?;
        Poll::Ready(Ok(Self::BidiStream {
            send: Self::SendStream::new(send),
            recv: RecvStream::new(recv),
        }))
    }

    /// Attempts to open a new unidirectional send stream.
    ///
    /// Returns a [`SendStream`] once ready.
    fn poll_open_send(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::SendStream, StreamErrorIncoming>> {
        let uni = self.opening_uni.get_or_insert_with(|| {
            Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((conn.open_uni().await, conn))
            }))
        });

        let send = ready!(uni.poll_next_unpin(cx))
            .expect("BoxStream does not return None")
            .map_err(|e| StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_connection_error(e),
            })?;
        Poll::Ready(Ok(Self::SendStream::new(send)))
    }

    /// Closes the QUIC connection with the provided application error code and reason.
    fn close(&mut self, code: Code, reason: &[u8]) {
        self.conn.close(
            VarInt::from_u64(code.value()).expect("error code VarInt"),
            reason,
        );
    }
}

/// A handle for opening outgoing QUIC streams.
///
/// Implements [`h3::quic::OpenStreams`] for use with HTTP/3.
pub struct OpenStreams {
    conn: iroh::endpoint::Connection,
    opening_bi: Option<BoxStreamSync<'static, <OpenBi<'static> as Future>::Output>>,
    opening_uni: Option<BoxStreamSync<'static, <OpenUni<'static> as Future>::Output>>,
}

impl<B> quic::OpenStreams<B> for OpenStreams
where
    B: Buf,
{
    type SendStream = SendStream<B>;
    type BidiStream = BidiStream<B>;

    /// Polls for opening a new bidirectional stream.
    ///
    /// Returns a [`BidiStream`] on success.
    fn poll_open_bidi(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        let bi = self.opening_bi.get_or_insert_with(|| {
            Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((conn.open_bi().await, conn))
            }))
        });

        let (send, recv) = ready!(bi.poll_next_unpin(cx))
            .expect("BoxStream does not return None")
            .map_err(|e| StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_connection_error(e),
            })?;
        Poll::Ready(Ok(Self::BidiStream {
            send: Self::SendStream::new(send),
            recv: RecvStream::new(recv),
        }))
    }

    /// Polls for opening a new unidirectional send stream.
    ///
    /// Returns a [`SendStream`] on success.
    fn poll_open_send(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Self::SendStream, StreamErrorIncoming>> {
        let uni = self.opening_uni.get_or_insert_with(|| {
            Box::pin(stream::unfold(self.conn.clone(), |conn| async {
                Some((conn.open_uni().await, conn))
            }))
        });

        let send = ready!(uni.poll_next_unpin(cx))
            .expect("BoxStream does not return None")
            .map_err(|e| StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_connection_error(e),
            })?;
        Poll::Ready(Ok(Self::SendStream::new(send)))
    }

    /// Closes the underlying connection with the given error code and reason.
    fn close(&mut self, code: Code, reason: &[u8]) {
        self.conn.close(
            VarInt::from_u64(code.value()).expect("error code VarInt"),
            reason,
        );
    }
}

/// Implements [`Clone`] for [`OpenStreams`].
impl Clone for OpenStreams {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
            opening_bi: None,
            opening_uni: None,
        }
    }
}

/// A bidirectional QUIC stream that contains both send and receive halves.
///
/// This struct implements both [`h3::quic::BidiStream`], [`h3::quic::RecvStream`],
/// and [`h3::quic::SendStream`] traits, allowing it to be split or used directly.
pub struct BidiStream<B>
where
    B: Buf,
{
    send: SendStream<B>,
    recv: RecvStream,
}

impl<B> quic::BidiStream<B> for BidiStream<B>
where
    B: Buf,
{
    type SendStream = SendStream<B>;
    type RecvStream = RecvStream;

    /// Splits the bidirectional stream into its send and receive halves.
    ///
    /// # Returns
    /// A tuple of `(SendStream, RecvStream)`.
    fn split(self) -> (Self::SendStream, Self::RecvStream) {
        (self.send, self.recv)
    }
}

impl<B: Buf> quic::RecvStream for BidiStream<B> {
    type Buf = Bytes;

    /// Polls for incoming data on the receive side of the stream.
    ///
    /// Returns `Poll::Ready(Ok(Some(Bytes)))` when data is available,
    /// `Poll::Ready(Ok(None))` when the stream is finished,
    /// or `Poll::Ready(Err(StreamErrorIncoming))` on error.
    fn poll_data(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<Self::Buf>, StreamErrorIncoming>> {
        self.recv.poll_data(cx)
    }

    /// Informs the peer that the receiver is no longer interested in this stream.
    fn stop_sending(&mut self, error_code: u64) {
        self.recv.stop_sending(error_code)
    }

    /// Returns the QUIC stream ID for this receiving stream.
    fn recv_id(&self) -> StreamId {
        self.recv.recv_id()
    }
}

impl<B> quic::SendStream<B> for BidiStream<B>
where
    B: Buf,
{
    /// Polls for readiness to send data on the stream.
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        self.send.poll_ready(cx)
    }

    /// Polls for completion of the stream’s send side (finishing transmission).
    fn poll_finish(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        self.send.poll_finish(cx)
    }

    /// Resets the send side of the stream with an error code.
    fn reset(&mut self, reset_code: u64) {
        self.send.reset(reset_code)
    }

    /// Queues a buffer of data to be sent on the stream.
    fn send_data<D: Into<WriteBuf<B>>>(&mut self, data: D) -> Result<(), StreamErrorIncoming> {
        self.send.send_data(data)
    }

    /// Returns the QUIC stream ID for this sending stream.
    fn send_id(&self) -> StreamId {
        self.send.send_id()
    }
}

impl<B> quic::SendStreamUnframed<B> for BidiStream<B>
where
    B: Buf,
{
    /// Polls to send raw unframed data from the provided buffer.
    ///
    /// This variant writes directly from the buffer without framing.
    fn poll_send<D: Buf>(
        &mut self,
        cx: &mut task::Context<'_>,
        buf: &mut D,
    ) -> Poll<Result<usize, StreamErrorIncoming>> {
        self.send.poll_send(cx, buf)
    }
}

/// A receiving QUIC stream that reads ordered chunks of data.
///
/// Internally wraps an [`iroh::endpoint::RecvStream`] and tracks whether it is
/// idle, actively reading, or terminal.
pub struct RecvStream {
    recv_id: StreamId,
    state: RecvStreamState,
}

enum RecvStreamState {
    Idle(iroh::endpoint::RecvStream),
    Reading(ReadChunkFuture),
    Stopped,
    Drained,
    Failed,
}

type ReadChunkFuture = Pin<
    Box<
        dyn Future<
                Output = (
                    iroh::endpoint::RecvStream,
                    Result<Option<Bytes>, iroh::endpoint::ReadError>,
                ),
            > + Send
            + 'static,
    >,
>;

impl RecvStream {
    /// Creates a new [`RecvStream`] from an [`iroh::endpoint::RecvStream`].
    fn new(stream: iroh::endpoint::RecvStream) -> Self {
        let num: u64 = stream.id().into();
        Self {
            recv_id: num.try_into().expect("invalid stream id"),
            state: RecvStreamState::Idle(stream),
        }
    }
}

impl quic::RecvStream for RecvStream {
    type Buf = Bytes;

    /// Polls for the next chunk of received data.
    ///
    /// Returns:
    /// * `Poll::Ready(Ok(Some(Bytes)))` — when data is available.
    /// * `Poll::Ready(Ok(None))` — when the stream has finished.
    /// * `Poll::Ready(Err(StreamErrorIncoming))` — when an error occurs.
    fn poll_data(
        &mut self,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<Option<Self::Buf>, StreamErrorIncoming>> {
        loop {
            match &mut self.state {
                RecvStreamState::Idle(_) => {
                    let state = std::mem::replace(&mut self.state, RecvStreamState::Stopped);
                    let RecvStreamState::Idle(mut stream) = state else {
                        unreachable!("state checked before replacement")
                    };
                    self.state = RecvStreamState::Reading(Box::pin(async move {
                        let chunk = stream.read_chunk(usize::MAX).await;
                        (stream, chunk)
                    }));
                }
                RecvStreamState::Reading(read_chunk_fut) => {
                    let (stream, chunk) = ready!(read_chunk_fut.as_mut().poll(cx));
                    match chunk {
                        Ok(Some(chunk)) => {
                            self.state = RecvStreamState::Idle(stream);
                            return Poll::Ready(Ok(Some(chunk)));
                        }
                        Ok(None) => {
                            self.state = RecvStreamState::Drained;
                            return Poll::Ready(Ok(None));
                        }
                        Err(error) => {
                            self.state = RecvStreamState::Failed;
                            return Poll::Ready(Err(convert_read_error_to_stream_error(error)));
                        }
                    }
                }
                RecvStreamState::Stopped | RecvStreamState::Drained | RecvStreamState::Failed => {
                    return Poll::Ready(Ok(None));
                }
            }
        }
    }

    /// Cancels further reception on this stream with the given error code.
    fn stop_sending(&mut self, error_code: u64) {
        let error_code = VarInt::from_u64(error_code).expect("invalid error_code");
        let state = std::mem::replace(&mut self.state, RecvStreamState::Stopped);
        self.state = match state {
            RecvStreamState::Idle(mut stream) => {
                stream.stop(error_code).ok();
                RecvStreamState::Stopped
            }
            RecvStreamState::Reading(_) => RecvStreamState::Stopped,
            RecvStreamState::Stopped => RecvStreamState::Stopped,
            RecvStreamState::Drained => RecvStreamState::Drained,
            RecvStreamState::Failed => RecvStreamState::Failed,
        };
    }

    /// Returns the QUIC stream ID associated with this receive stream.
    fn recv_id(&self) -> StreamId {
        self.recv_id
    }
}

/// Converts an [`iroh::endpoint::ReadError`] into an [`h3::quic::StreamErrorIncoming`].
fn convert_read_error_to_stream_error(error: ReadError) -> StreamErrorIncoming {
    match error {
        ReadError::Reset(var_int) => StreamErrorIncoming::StreamTerminated {
            error_code: var_int.into_inner(),
        },
        ReadError::ConnectionLost(connection_error) => {
            StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_connection_error(connection_error),
            }
        }
        error @ ReadError::ClosedStream => StreamErrorIncoming::Unknown(Box::new(error)),
        error @ ReadError::ZeroRttRejected => StreamErrorIncoming::Unknown(Box::new(error)),
    }
}

/// Converts an [`iroh::endpoint::WriteError`] into an [`h3::quic::StreamErrorIncoming`].
fn convert_write_error_to_stream_error(error: WriteError) -> StreamErrorIncoming {
    match error {
        WriteError::Stopped(var_int) => StreamErrorIncoming::StreamTerminated {
            error_code: var_int.into_inner(),
        },
        WriteError::ConnectionLost(connection_error) => {
            StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: convert_connection_error(connection_error),
            }
        }
        error @ WriteError::ClosedStream | error @ WriteError::ZeroRttRejected => {
            StreamErrorIncoming::Unknown(Box::new(error))
        }
    }
}

/// A sending QUIC stream that transmits buffered data.
///
/// This struct wraps an [`iroh::endpoint::SendStream`] and implements the
/// [`h3::quic::SendStream`] and [`h3::quic::SendStreamUnframed`] traits.
pub struct SendStream<B: Buf> {
    stream: iroh::endpoint::SendStream,
    writing: Option<WriteBuf<B>>,
}

impl<B> SendStream<B>
where
    B: Buf,
{
    /// Creates a new [`SendStream`] from an [`iroh::endpoint::SendStream`].
    fn new(stream: iroh::endpoint::SendStream) -> SendStream<B> {
        Self {
            stream,
            writing: None,
        }
    }
}

impl<B> quic::SendStream<B> for SendStream<B>
where
    B: Buf,
{
    /// Polls to check if the stream is ready to send more data.
    ///
    /// If data is pending in `self.writing`, it is written until complete.
    fn poll_ready(&mut self, cx: &mut task::Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        if let Some(ref mut data) = self.writing {
            while data.has_remaining() {
                let stream = Pin::new(&mut self.stream);
                let written = ready!(stream.poll_write(cx, data.chunk()))
                    .map_err(convert_write_error_to_stream_error)?;
                data.advance(written);
            }
        }
        self.writing = None;
        Poll::Ready(Ok(()))
    }

    /// Finishes sending data on this stream and closes it gracefully.
    fn poll_finish(
        &mut self,
        _cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), StreamErrorIncoming>> {
        Poll::Ready(
            self.stream
                .finish()
                .map_err(|e| StreamErrorIncoming::Unknown(Box::new(e))),
        )
    }

    /// Resets the stream with the provided error code, immediately terminating it.
    fn reset(&mut self, reset_code: u64) {
        let _ = self
            .stream
            .reset(VarInt::from_u64(reset_code).unwrap_or(VarInt::MAX));
    }

    /// Queues data to be sent in the next `poll_ready` call.
    ///
    /// Returns an error if called while another write is still in progress.
    fn send_data<D: Into<WriteBuf<B>>>(&mut self, data: D) -> Result<(), StreamErrorIncoming> {
        if self.writing.is_some() {
            return Err(StreamErrorIncoming::ConnectionErrorIncoming {
                connection_error: ConnectionErrorIncoming::InternalError(
                    "internal error in the http stack".to_string(),
                ),
            });
        }
        self.writing = Some(data.into());
        Ok(())
    }

    /// Returns the QUIC stream ID for this sending stream.
    fn send_id(&self) -> StreamId {
        let num: u64 = self.stream.id().into();
        num.try_into().expect("invalid stream id")
    }
}

impl<B> quic::SendStreamUnframed<B> for SendStream<B>
where
    B: Buf,
{
    /// Polls to send unframed raw data directly from the provided buffer.
    ///
    /// Returns the number of bytes written on success.
    fn poll_send<D: Buf>(
        &mut self,
        cx: &mut task::Context<'_>,
        buf: &mut D,
    ) -> Poll<Result<usize, StreamErrorIncoming>> {
        if self.writing.is_some() {
            panic!("poll_send called while send stream is not ready");
        }

        let s = Pin::new(&mut self.stream);

        let res = ready!(s.poll_write(cx, buf.chunk()));
        match res {
            Ok(written) => {
                buf.advance(written);
                Poll::Ready(Ok(written))
            }
            Err(err) => Poll::Ready(Err(convert_write_error_to_stream_error(err))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        task::{Context, Poll},
    };

    use futures::{future::poll_fn, task::noop_waker_ref};
    use h3::quic::RecvStream as H3RecvStream;
    use iroh::{address_lookup::memory::MemoryLookup, endpoint::presets::Minimal};

    use super::*;

    const TEST_ALPN: &[u8] = b"iroh-h3-adapter-test";
    const MARKER: &[u8] = b"unlock";

    struct TestRecv {
        recv: RecvStream,
        client_send: iroh::endpoint::SendStream,
        _client_recv: iroh::endpoint::RecvStream,
        _server_send: iroh::endpoint::SendStream,
        _client_conn: iroh::endpoint::Connection,
        _server_conn: iroh::endpoint::Connection,
        _client_endpoint: Endpoint,
        _server_endpoint: Endpoint,
    }

    async fn make_test_recv() -> TestRecv {
        let address_lookup = MemoryLookup::new();
        let server_endpoint = Endpoint::builder(Minimal)
            .alpns(vec![TEST_ALPN.to_vec()])
            .address_lookup(address_lookup.clone())
            .bind()
            .await
            .expect("bind server endpoint");
        let client_endpoint = Endpoint::builder(Minimal)
            .address_lookup(address_lookup.clone())
            .bind()
            .await
            .expect("bind client endpoint");
        address_lookup.add_endpoint_info(server_endpoint.addr());
        address_lookup.add_endpoint_info(client_endpoint.addr());

        let accepting_endpoint = server_endpoint.clone();
        let server_accept = tokio::spawn(async move {
            accepting_endpoint
                .accept()
                .await
                .expect("server accepts incoming connection")
                .accept()
                .expect("server accepts connecting handshake")
                .await
                .expect("server accepts established connection")
        });

        let client_conn = client_endpoint
            .connect(server_endpoint.id(), TEST_ALPN)
            .await
            .expect("client connects to server");
        let server_conn = server_accept.await.expect("server accept task joins");

        let (mut client_send, client_recv) = client_conn
            .open_bi()
            .await
            .expect("client opens bidirectional stream");
        client_send
            .write_all(MARKER)
            .await
            .expect("client writes marker");

        let (server_send, server_recv) = server_conn
            .accept_bi()
            .await
            .expect("server accepts bidirectional stream");

        TestRecv {
            recv: RecvStream::new(server_recv),
            client_send,
            _client_recv: client_recv,
            _server_send: server_send,
            _client_conn: client_conn,
            _server_conn: server_conn,
            _client_endpoint: client_endpoint,
            _server_endpoint: server_endpoint,
        }
    }

    async fn drain_marker(recv: &mut RecvStream) {
        let marker = poll_fn(|cx| H3RecvStream::poll_data(recv, cx))
            .await
            .expect("marker read succeeds")
            .expect("marker chunk is present");
        assert_eq!(marker, Bytes::from_static(MARKER));
    }

    fn poll_once(recv: &mut RecvStream) -> Poll<Result<Option<Bytes>, StreamErrorIncoming>> {
        let mut cx = Context::from_waker(noop_waker_ref());
        H3RecvStream::poll_data(recv, &mut cx)
    }

    async fn enter_reading(recv: &mut RecvStream) {
        drain_marker(recv).await;
        assert!(
            poll_once(recv).is_pending(),
            "read with no available data should be pending"
        );
    }

    fn assert_no_panic(f: impl FnOnce()) {
        assert!(catch_unwind(AssertUnwindSafe(f)).is_ok());
    }

    #[tokio::test]
    async fn reading_recv_id_and_stop_sending_do_not_panic() {
        let mut test = make_test_recv().await;
        let recv_id = H3RecvStream::recv_id(&test.recv);
        enter_reading(&mut test.recv).await;

        assert_no_panic(|| {
            assert_eq!(H3RecvStream::recv_id(&test.recv), recv_id);
        });
        assert_no_panic(|| H3RecvStream::stop_sending(&mut test.recv, 42));
    }

    #[tokio::test]
    async fn reading_stop_then_next_poll_returns_none_immediately() {
        let mut test = make_test_recv().await;
        enter_reading(&mut test.recv).await;

        H3RecvStream::stop_sending(&mut test.recv, 42);

        assert!(matches!(poll_once(&mut test.recv), Poll::Ready(Ok(None))));
    }

    #[tokio::test]
    async fn idle_stop_then_poll_returns_none() {
        let mut test = make_test_recv().await;
        let stopped = test.client_send.stopped();

        H3RecvStream::stop_sending(&mut test.recv, 42);

        let stop_code = stopped
            .await
            .expect("client observes peer stop")
            .expect("peer stop has an error code");
        assert_eq!(stop_code.into_inner(), 42);
        assert!(matches!(poll_once(&mut test.recv), Poll::Ready(Ok(None))));
    }

    #[tokio::test]
    async fn terminal_states_repeated_stop_sending_is_idempotent_for_valid_code() {
        let mut stopped = make_test_recv().await;
        H3RecvStream::stop_sending(&mut stopped.recv, 42);
        assert_no_panic(|| H3RecvStream::stop_sending(&mut stopped.recv, 42));
        assert!(matches!(
            poll_once(&mut stopped.recv),
            Poll::Ready(Ok(None))
        ));

        let mut drained = make_test_recv().await;
        drain_marker(&mut drained.recv).await;
        drained
            .client_send
            .finish()
            .expect("client finishes send stream");
        let eof = poll_fn(|cx| H3RecvStream::poll_data(&mut drained.recv, cx))
            .await
            .expect("eof poll succeeds");
        assert!(eof.is_none());
        assert_no_panic(|| H3RecvStream::stop_sending(&mut drained.recv, 42));
        assert_no_panic(|| H3RecvStream::stop_sending(&mut drained.recv, 42));
        assert!(matches!(
            poll_once(&mut drained.recv),
            Poll::Ready(Ok(None))
        ));

        let mut failed = make_test_recv().await;
        drain_marker(&mut failed.recv).await;
        failed
            .client_send
            .reset(VarInt::from_u64(42).expect("valid reset code"))
            .expect("client resets send stream");
        let first_error = poll_fn(|cx| H3RecvStream::poll_data(&mut failed.recv, cx))
            .await
            .expect_err("first reset poll returns an error");
        assert!(matches!(
            first_error,
            StreamErrorIncoming::StreamTerminated { error_code: 42 }
        ));
        assert_no_panic(|| H3RecvStream::stop_sending(&mut failed.recv, 42));
        assert_no_panic(|| H3RecvStream::stop_sending(&mut failed.recv, 42));
        assert!(matches!(poll_once(&mut failed.recv), Poll::Ready(Ok(None))));
    }

    #[tokio::test]
    async fn eof_then_repeated_poll_returns_none() {
        let mut test = make_test_recv().await;
        drain_marker(&mut test.recv).await;
        test.client_send
            .finish()
            .expect("client finishes send stream");

        let eof = poll_fn(|cx| H3RecvStream::poll_data(&mut test.recv, cx))
            .await
            .expect("eof poll succeeds");
        assert!(eof.is_none());

        assert!(matches!(poll_once(&mut test.recv), Poll::Ready(Ok(None))));
        assert!(matches!(poll_once(&mut test.recv), Poll::Ready(Ok(None))));
    }

    #[tokio::test]
    async fn peer_reset_first_error_then_repeated_poll_returns_none() {
        let mut test = make_test_recv().await;
        drain_marker(&mut test.recv).await;
        test.client_send
            .reset(VarInt::from_u64(42).expect("valid reset code"))
            .expect("client resets send stream");

        let first_error = poll_fn(|cx| H3RecvStream::poll_data(&mut test.recv, cx))
            .await
            .expect_err("first reset poll returns an error");
        assert!(matches!(
            first_error,
            StreamErrorIncoming::StreamTerminated { error_code: 42 }
        ));

        assert!(matches!(poll_once(&mut test.recv), Poll::Ready(Ok(None))));
        assert!(matches!(poll_once(&mut test.recv), Poll::Ready(Ok(None))));
    }
}
