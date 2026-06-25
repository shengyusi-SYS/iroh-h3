use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};

use bytes::{Buf, Bytes};
use futures::{Stream, task::AtomicWaker};
use h3::error::Code;
use http_body::Frame;
use iroh::EndpointId;
use iroh_h3::OpenStreams;
use tracing::{debug, instrument, trace};

use crate::body::Body;
use crate::connection_manager::ConnectionManager;
use crate::error::Error;
use crate::response::Response;

pub(crate) type H3RequestStream = h3::client::RequestStream<iroh_h3::BidiStream<Bytes>, Bytes>;
pub(crate) type H3Sender = h3::client::SendRequest<OpenStreams, Bytes>;

/// A cancellable request that has been created but not yet resolved.
pub struct PendingRequest {
    state: Arc<CancellableRequestState>,
    stage: PendingRequestStage,
    response: PhantomData<Result<Response, Error>>,
}

type GetSenderFuture =
    Pin<Box<dyn Future<Output = (http::Request<()>, Bytes, Result<H3Sender, Error>)> + Send>>;
type SendRequestFuture =
    Pin<Box<dyn Future<Output = (H3Sender, Bytes, Result<H3RequestStream, Error>)> + Send>>;
type StreamIoFuture =
    Pin<Box<dyn Future<Output = (H3Sender, H3RequestStream, Result<(), Error>)> + Send>>;
type RecvResponseFuture = Pin<
    Box<dyn Future<Output = (H3Sender, H3RequestStream, Result<http::Response<()>, Error>)> + Send>,
>;

enum PendingRequestStage {
    BeforeStreamOpen {
        connection_manager: ConnectionManager,
        peer_id: EndpointId,
        request: Option<http::Request<()>>,
        body: Bytes,
    },
    GettingSender(GetSenderFuture),
    SendingRequestHeaders(SendRequestFuture),
    SendingRequestBody(StreamIoFuture),
    FinishingRequest(StreamIoFuture),
    WaitingResponseHeaders(RecvResponseFuture),
    Complete,
}

/// A response byte stream associated with a cancellation handle.
#[allow(dead_code)]
pub struct CancellableBytesStream {
    body: Option<CancellableH3Body>,
}

/// Handle used to cancel an in-flight request.
#[derive(Clone)]
pub struct RequestCancelHandle {
    state: Arc<CancellableRequestState>,
}

pub(crate) struct CancellableRequestState {
    inner: Mutex<CancellableRequestInner>,
    waker: AtomicWaker,
}

#[allow(dead_code)]
pub(crate) struct CancellableRequestInner {
    cancelled: bool,
    phase: RequestPhase,
    send_finished: bool,
    recv_terminal: bool,
    receive_stop_sent: bool,
    send_stop_sent: bool,
    cancel_error_emitted: bool,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestPhase {
    BeforeStreamOpen,
    OpeningRequestStream,
    SendingRequestHeaders,
    SendingRequestBody,
    FinishingRequest,
    WaitingResponseHeaders,
    ReadingResponseBody,
    Complete,
}

pub(crate) const REQUEST_CANCEL_CODE: Code = Code::H3_REQUEST_CANCELLED;

pub(crate) trait StopOwner {
    fn stop_receive(&mut self);
    fn stop_send(&mut self);
}

impl PendingRequest {
    pub(crate) fn new(
        connection_manager: ConnectionManager,
        peer_id: EndpointId,
        request: http::Request<()>,
        body: Bytes,
    ) -> Self {
        Self {
            state: CancellableRequestState::new(),
            stage: PendingRequestStage::BeforeStreamOpen {
                connection_manager,
                peer_id,
                request: Some(request),
                body,
            },
            response: PhantomData,
        }
    }

    /// Returns a handle that can cancel this pending request.
    pub fn cancel_handle(&self) -> RequestCancelHandle {
        RequestCancelHandle::new(self.state.clone())
    }
}

impl Unpin for PendingRequest {}

impl Future for PendingRequest {
    type Output = Result<Response, Error>;

    #[instrument(skip(self, cx))]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.state.waker.register(cx.waker());

        loop {
            if this.state.is_cancelled() {
                this.stage = PendingRequestStage::Complete;
                return Poll::Ready(Err(Error::Cancelled));
            }

            match &mut this.stage {
                PendingRequestStage::BeforeStreamOpen {
                    connection_manager,
                    peer_id,
                    request,
                    body,
                } => {
                    this.state.mark_phase(RequestPhase::OpeningRequestStream);
                    let future = get_sender_owned(
                        connection_manager.clone(),
                        *peer_id,
                        request.take().expect("request missing before stream open"),
                        body.clone(),
                    );
                    this.stage = PendingRequestStage::GettingSender(Box::pin(future));
                }
                PendingRequestStage::GettingSender(future) => {
                    let (request, body, result) = ready!(future.as_mut().poll(cx));
                    let sender = match result {
                        Ok(sender) => sender,
                        Err(err) => {
                            this.stage = PendingRequestStage::Complete;
                            return Poll::Ready(Err(err));
                        }
                    };

                    this.state.mark_phase(RequestPhase::SendingRequestHeaders);
                    this.stage = PendingRequestStage::SendingRequestHeaders(Box::pin(
                        send_request_owned(sender, request, body),
                    ));
                }
                PendingRequestStage::SendingRequestHeaders(future) => {
                    let (sender, body, result) = ready!(future.as_mut().poll(cx));
                    let stream = match result {
                        Ok(stream) => stream,
                        Err(err) => {
                            this.stage = PendingRequestStage::Complete;
                            return Poll::Ready(Err(err));
                        }
                    };

                    this.state.mark_phase(RequestPhase::SendingRequestBody);
                    this.stage = PendingRequestStage::SendingRequestBody(Box::pin(
                        send_body_owned(sender, stream, body, this.state.clone()),
                    ));
                }
                PendingRequestStage::SendingRequestBody(future) => {
                    let (sender, mut stream, result) = ready!(future.as_mut().poll(cx));
                    if let Err(err) = result {
                        let mut owner = H3StopOwner::new(&mut stream);
                        this.state.apply_drop_cleanup_to_owner(&mut owner);
                        this.stage = PendingRequestStage::Complete;
                        return Poll::Ready(Err(err));
                    }

                    this.state.mark_phase(RequestPhase::FinishingRequest);
                    this.stage = PendingRequestStage::FinishingRequest(Box::pin(
                        finish_request_owned(sender, stream, this.state.clone()),
                    ));
                }
                PendingRequestStage::FinishingRequest(future) => {
                    let (sender, mut stream, result) = ready!(future.as_mut().poll(cx));
                    if let Err(err) = result {
                        let mut owner = H3StopOwner::new(&mut stream);
                        this.state.apply_drop_cleanup_to_owner(&mut owner);
                        this.stage = PendingRequestStage::Complete;
                        return Poll::Ready(Err(err));
                    }

                    this.state.mark_send_finished();
                    this.state.mark_phase(RequestPhase::WaitingResponseHeaders);
                    this.stage = PendingRequestStage::WaitingResponseHeaders(Box::pin(
                        recv_response_owned(sender, stream, this.state.clone()),
                    ));
                }
                PendingRequestStage::WaitingResponseHeaders(future) => {
                    let (sender, mut stream, result) = ready!(future.as_mut().poll(cx));
                    let response = match result {
                        Ok(response) => response,
                        Err(err) => {
                            let mut owner = H3StopOwner::new(&mut stream);
                            this.state.apply_drop_cleanup_to_owner(&mut owner);
                            this.stage = PendingRequestStage::Complete;
                            return Poll::Ready(Err(err));
                        }
                    };

                    let (inner, ()) = response.into_parts();
                    let body = Body::cancellable_h3(CancellableH3Body::new(
                        stream,
                        sender,
                        this.state.clone(),
                    ));
                    this.stage = PendingRequestStage::Complete;
                    return Poll::Ready(Ok(Response { inner, body }));
                }
                PendingRequestStage::Complete => {
                    return Poll::Ready(Err(Error::Cancelled));
                }
            }
        }
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.stage = PendingRequestStage::Complete;
    }
}

async fn get_sender_owned(
    connection_manager: ConnectionManager,
    peer_id: EndpointId,
    request: http::Request<()>,
    body: Bytes,
) -> (http::Request<()>, Bytes, Result<H3Sender, Error>) {
    let result = connection_manager.get_sender(peer_id).await;
    (request, body, result)
}

async fn send_request_owned(
    mut sender: H3Sender,
    request: http::Request<()>,
    body: Bytes,
) -> (H3Sender, Bytes, Result<H3RequestStream, Error>) {
    let result = sender
        .send_request(request)
        .await
        .map_err(|err| Error::Transport(err.into()));
    (sender, body, result)
}

async fn send_body_owned(
    sender: H3Sender,
    stream: H3RequestStream,
    body: Bytes,
    state: Arc<CancellableRequestState>,
) -> (H3Sender, H3RequestStream, Result<(), Error>) {
    let mut owner = OwnedStreamGuard::new(stream, state);
    let result = if body.is_empty() {
        Ok(())
    } else {
        owner
            .stream_mut()
            .send_data(body)
            .await
            .map_err(|err| Error::Transport(err.into()))
    };
    let stream = owner.take_stream();
    (sender, stream, result)
}

async fn finish_request_owned(
    sender: H3Sender,
    stream: H3RequestStream,
    state: Arc<CancellableRequestState>,
) -> (H3Sender, H3RequestStream, Result<(), Error>) {
    let mut owner = OwnedStreamGuard::new(stream, state);
    let result = owner
        .stream_mut()
        .finish()
        .await
        .map_err(|err| Error::Transport(err.into()));
    let stream = owner.take_stream();
    (sender, stream, result)
}

async fn recv_response_owned(
    sender: H3Sender,
    stream: H3RequestStream,
    state: Arc<CancellableRequestState>,
) -> (H3Sender, H3RequestStream, Result<http::Response<()>, Error>) {
    let mut owner = OwnedStreamGuard::new(stream, state);
    let result = owner
        .stream_mut()
        .recv_response()
        .await
        .map_err(|err| Error::Transport(err.into()));
    let stream = owner.take_stream();
    (sender, stream, result)
}

struct OwnedStreamGuard {
    stream: Option<H3RequestStream>,
    state: Arc<CancellableRequestState>,
}

impl OwnedStreamGuard {
    fn new(stream: H3RequestStream, state: Arc<CancellableRequestState>) -> Self {
        Self {
            stream: Some(stream),
            state,
        }
    }

    fn stream_mut(&mut self) -> &mut H3RequestStream {
        self.stream
            .as_mut()
            .expect("stream owner missing during request operation")
    }

    fn take_stream(&mut self) -> H3RequestStream {
        self.stream
            .take()
            .expect("stream owner missing after request operation")
    }
}

impl Drop for OwnedStreamGuard {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.as_mut() {
            let mut owner = H3StopOwner::new(stream);
            if self.state.is_cancelled() {
                self.state.apply_cancel_to_owner(&mut owner);
            } else {
                self.state.apply_drop_cleanup_to_owner(&mut owner);
            }
        }
    }
}

impl CancellableRequestState {
    #[allow(dead_code)]
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(CancellableRequestInner {
                cancelled: false,
                phase: RequestPhase::BeforeStreamOpen,
                send_finished: false,
                recv_terminal: false,
                receive_stop_sent: false,
                send_stop_sent: false,
                cancel_error_emitted: false,
            }),
            waker: AtomicWaker::new(),
        })
    }

    #[allow(dead_code)]
    pub(crate) fn mark_phase(&self, phase: RequestPhase) {
        self.inner.lock().expect("poisoned cancel state").phase = phase;
    }

    #[allow(dead_code)]
    pub(crate) fn mark_send_finished(&self) {
        self.inner
            .lock()
            .expect("poisoned cancel state")
            .send_finished = true;
    }

    pub(crate) fn mark_recv_terminal(&self) {
        let mut inner = self.inner.lock().expect("poisoned cancel state");
        inner.recv_terminal = true;
        inner.send_finished = true;
        inner.phase = RequestPhase::Complete;
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.inner.lock().expect("poisoned cancel state").cancelled
    }

    pub(crate) fn take_cancel_error_to_emit(&self) -> bool {
        let mut inner = self.inner.lock().expect("poisoned cancel state");
        if inner.cancel_error_emitted {
            false
        } else {
            inner.cancel_error_emitted = true;
            true
        }
    }

    #[allow(dead_code)]
    pub(crate) fn apply_cancel_to_owner(&self, owner: &mut impl StopOwner) {
        let (stop_receive, stop_send) = {
            let mut inner = self.inner.lock().expect("poisoned cancel state");
            if !inner.cancelled {
                return;
            }

            let stop_receive = !inner.recv_terminal && !inner.receive_stop_sent;
            let stop_send = !inner.send_finished && !inner.send_stop_sent;

            if stop_receive {
                inner.receive_stop_sent = true;
            }
            if stop_send {
                inner.send_stop_sent = true;
            }

            (stop_receive, stop_send)
        };

        if stop_receive {
            owner.stop_receive();
        }
        if stop_send {
            owner.stop_send();
        }
    }

    pub(crate) fn apply_drop_cleanup_to_owner(&self, owner: &mut impl StopOwner) {
        let (stop_receive, stop_send) = {
            let mut inner = self.inner.lock().expect("poisoned cancel state");

            let stop_receive = !inner.recv_terminal && !inner.receive_stop_sent;
            let stop_send = !inner.send_finished && !inner.send_stop_sent;

            if stop_receive {
                inner.receive_stop_sent = true;
            }
            if stop_send {
                inner.send_stop_sent = true;
            }

            (stop_receive, stop_send)
        };

        if stop_receive {
            owner.stop_receive();
        }
        if stop_send {
            owner.stop_send();
        }
    }
}

impl RequestCancelHandle {
    #[allow(dead_code)]
    pub(crate) fn new(state: Arc<CancellableRequestState>) -> Self {
        Self { state }
    }

    /// Cancels the associated request.
    pub fn cancel(&self) {
        let should_wake = {
            let mut inner = self.state.inner.lock().expect("poisoned cancel state");
            let should_wake = !inner.cancelled;
            inner.cancelled = true;
            should_wake
        };

        if should_wake {
            self.state.waker.wake();
        }
    }

    /// Returns whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.state
            .inner
            .lock()
            .expect("poisoned cancel state")
            .cancelled
    }
}

pub(crate) struct CancellableH3Body {
    stream: Option<H3RequestStream>,
    sender: H3Sender,
    state: Arc<CancellableRequestState>,
}

impl CancellableH3Body {
    #[allow(dead_code)]
    pub(crate) fn new(
        stream: H3RequestStream,
        sender: H3Sender,
        state: Arc<CancellableRequestState>,
    ) -> Self {
        state.mark_phase(RequestPhase::ReadingResponseBody);
        state.mark_send_finished();
        Self {
            stream: Some(stream),
            sender,
            state,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn cancel_handle(&self) -> RequestCancelHandle {
        RequestCancelHandle::new(self.state.clone())
    }

    fn cleanup_unfinished_response_body(&mut self) {
        if let Some(stream) = self.stream.as_mut() {
            let mut owner = H3StopOwner::new(stream);
            self.state.apply_drop_cleanup_to_owner(&mut owner);
        }
    }
}

impl fmt::Debug for CancellableH3Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = &self.sender;
        f.debug_struct("CancellableH3Body").finish_non_exhaustive()
    }
}

impl CancellableBytesStream {
    pub(crate) fn new(body: CancellableH3Body) -> Self {
        Self { body: Some(body) }
    }

    /// Returns a handle that can cancel this response body stream.
    pub fn cancel_handle(&self) -> RequestCancelHandle {
        self.body
            .as_ref()
            .expect("cancellable stream polled after completion")
            .cancel_handle()
    }
}

impl Stream for CancellableBytesStream {
    type Item = Result<Bytes, Error>;

    #[instrument(skip(self, cx))]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(body) = self.body.as_mut() else {
            return Poll::Ready(None);
        };
        let state = body.state.clone();
        state.waker.register(cx.waker());

        if state.is_cancelled() {
            if let Some(stream) = body.stream.as_mut() {
                let mut owner = H3StopOwner::new(stream);
                state.apply_cancel_to_owner(&mut owner);
            }
            if state.take_cancel_error_to_emit() {
                self.body = None;
                return Poll::Ready(Some(Err(Error::Cancelled)));
            }
            self.body = None;
            return Poll::Ready(None);
        }

        let item = {
            let stream = body
                .stream
                .as_mut()
                .expect("stream owner missing before terminal");
            ready!(stream.poll_recv_data(cx)).transpose()
        };

        if state.is_cancelled() {
            if let Some(stream) = body.stream.as_mut() {
                let mut owner = H3StopOwner::new(stream);
                state.apply_cancel_to_owner(&mut owner);
            }
            if state.take_cancel_error_to_emit() {
                self.body = None;
                return Poll::Ready(Some(Err(Error::Cancelled)));
            }
            self.body = None;
            return Poll::Ready(None);
        }

        match item {
            Some(Ok(mut frame)) => {
                trace!("received a frame of {} bytes", frame.remaining());
                let bytes = frame.copy_to_bytes(frame.remaining());
                Poll::Ready(Some(Ok(bytes)))
            }
            Some(Err(err)) => {
                body.state.mark_recv_terminal();
                self.body = None;
                if err.is_h3_no_error() {
                    debug!("received H3_NO_ERROR");
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Err(Error::Transport(err.into()))))
                }
            }
            None => {
                body.state.mark_recv_terminal();
                self.body = None;
                Poll::Ready(None)
            }
        }
    }
}

impl Drop for CancellableBytesStream {
    fn drop(&mut self) {
        if let Some(body) = self.body.as_mut() {
            body.cleanup_unfinished_response_body();
        }
    }
}

impl Drop for CancellableH3Body {
    fn drop(&mut self) {
        let _ = &self.sender;
        self.cleanup_unfinished_response_body();
    }
}

pub(crate) struct LegacyCompatibleH3ResponseBody {
    body: Option<CancellableH3Body>,
}

impl LegacyCompatibleH3ResponseBody {
    pub(crate) fn new(body: CancellableH3Body) -> Self {
        Self { body: Some(body) }
    }
}

type BodyStreamItem = Result<Frame<Bytes>, Error>;

impl http_body::Body for LegacyCompatibleH3ResponseBody {
    type Data = Bytes;
    type Error = Error;

    #[instrument(skip(self, cx))]
    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<BodyStreamItem>> {
        let Some(body) = self.body.as_mut() else {
            return Poll::Ready(None);
        };
        let Some(stream) = body.stream.as_mut() else {
            return Poll::Ready(None);
        };

        match ready!(stream.poll_recv_data(cx)).transpose() {
            Some(Ok(mut frame)) => {
                trace!("received a frame of {} bytes", frame.remaining());
                let bytes = frame.copy_to_bytes(frame.remaining());
                Poll::Ready(Some(Ok(Frame::data(bytes))))
            }
            Some(Err(err)) => {
                body.state.mark_recv_terminal();
                body.stream.take();
                if err.is_h3_no_error() {
                    debug!("received H3_NO_ERROR");
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Err(Error::Transport(err.into()))))
                }
            }
            None => {
                body.state.mark_recv_terminal();
                body.stream.take();
                Poll::Ready(None)
            }
        }
    }
}

impl Drop for LegacyCompatibleH3ResponseBody {
    fn drop(&mut self) {
        if let Some(body) = self.body.as_mut() {
            body.cleanup_unfinished_response_body();
        }
    }
}

pub(crate) struct H3StopOwner<'a> {
    stream: &'a mut H3RequestStream,
}

impl<'a> H3StopOwner<'a> {
    pub(crate) fn new(stream: &'a mut H3RequestStream) -> Self {
        Self { stream }
    }
}

impl StopOwner for H3StopOwner<'_> {
    fn stop_receive(&mut self) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.stream.stop_sending(REQUEST_CANCEL_CODE);
        }));
        if result.is_err() {
            tracing::debug!(
                "receive-side stop_sending unavailable during pending read; downgraded to best-effort cleanup"
            );
        }
    }

    fn stop_send(&mut self) {
        self.stream.stop_stream(REQUEST_CANCEL_CODE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeStopOwner {
        receive_stops: usize,
        send_stops: usize,
    }

    impl StopOwner for FakeStopOwner {
        fn stop_receive(&mut self) {
            self.receive_stops += 1;
        }

        fn stop_send(&mut self) {
            self.send_stops += 1;
        }
    }

    #[test]
    fn cancel_is_idempotent() {
        let state = CancellableRequestState::new();
        let handle = RequestCancelHandle::new(state.clone());
        let mut owner = FakeStopOwner::default();

        handle.cancel();
        handle.cancel();
        state.apply_cancel_to_owner(&mut owner);
        state.apply_cancel_to_owner(&mut owner);

        assert_eq!(owner.send_stops, 1);
        assert_eq!(owner.receive_stops, 1);
        assert!(handle.is_cancelled());
    }

    #[test]
    fn get_response_body_cancel_does_not_stop_send_side() {
        let state = CancellableRequestState::new();
        state.mark_phase(RequestPhase::ReadingResponseBody);
        state.mark_send_finished();
        RequestCancelHandle::new(state.clone()).cancel();

        let mut owner = FakeStopOwner::default();
        state.apply_cancel_to_owner(&mut owner);

        assert_eq!(owner.send_stops, 0);
        assert_eq!(owner.receive_stops, 1);
    }

    #[test]
    fn fixed_body_send_cancel_stops_request_side() {
        let state = CancellableRequestState::new();
        state.mark_phase(RequestPhase::SendingRequestBody);
        RequestCancelHandle::new(state.clone()).cancel();

        let mut owner = FakeStopOwner::default();
        state.apply_cancel_to_owner(&mut owner);

        assert_eq!(owner.send_stops, 1);
        assert_eq!(owner.receive_stops, 1);
    }

    #[test]
    fn finish_pending_cancel_stops_request_side() {
        let state = CancellableRequestState::new();
        state.mark_phase(RequestPhase::FinishingRequest);
        RequestCancelHandle::new(state.clone()).cancel();

        let mut owner = FakeStopOwner::default();
        state.apply_cancel_to_owner(&mut owner);

        assert_eq!(owner.send_stops, 1);
        assert_eq!(owner.receive_stops, 1);
    }

    #[test]
    fn eof_then_drop_does_not_stop_sending() {
        let state = CancellableRequestState::new();
        state.mark_recv_terminal();

        let mut owner = FakeStopOwner::default();
        state.apply_drop_cleanup_to_owner(&mut owner);

        assert_eq!(owner.receive_stops, 0);
        assert_eq!(owner.send_stops, 0);
    }
}
