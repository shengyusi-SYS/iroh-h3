use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::task::AtomicWaker;
use h3::error::Code;
use iroh_h3::OpenStreams;

use crate::error::Error;
use crate::response::Response;

#[allow(dead_code)]
pub(crate) type H3RequestStream = h3::client::RequestStream<iroh_h3::BidiStream<Bytes>, Bytes>;
pub(crate) type H3Sender = h3::client::SendRequest<OpenStreams, Bytes>;

/// A cancellable request that has been created but not yet resolved.
#[allow(dead_code)]
pub struct PendingRequest {
    state: Arc<CancellableRequestState>,
    sender: Option<H3Sender>,
    response: PhantomData<Result<Response, Error>>,
}

/// A response byte stream associated with a cancellation handle.
#[allow(dead_code)]
pub struct CancellableBytesStream {
    state: Arc<CancellableRequestState>,
    item: PhantomData<Result<Bytes, Error>>,
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

#[allow(dead_code)]
pub(crate) const REQUEST_CANCEL_CODE: Code = Code::H3_REQUEST_CANCELLED;

#[allow(dead_code)]
pub(crate) trait StopOwner {
    fn stop_receive(&mut self);
    fn stop_send(&mut self);
}

#[allow(dead_code)]
impl CancellableRequestState {
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

    pub(crate) fn mark_phase(&self, phase: RequestPhase) {
        self.inner.lock().expect("poisoned cancel state").phase = phase;
    }

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

#[allow(dead_code)]
impl RequestCancelHandle {
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

#[allow(dead_code)]
pub(crate) struct H3StopOwner<'a> {
    stream: &'a mut H3RequestStream,
}

#[allow(dead_code)]
impl<'a> H3StopOwner<'a> {
    pub(crate) fn new(stream: &'a mut H3RequestStream) -> Self {
        Self { stream }
    }
}

impl StopOwner for H3StopOwner<'_> {
    fn stop_receive(&mut self) {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.stream.stop_sending(REQUEST_CANCEL_CODE);
        }));
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
    fn eof_then_drop_does_not_stop_sending() {
        let state = CancellableRequestState::new();
        state.mark_recv_terminal();

        let mut owner = FakeStopOwner::default();
        state.apply_drop_cleanup_to_owner(&mut owner);

        assert_eq!(owner.receive_stops, 0);
        assert_eq!(owner.send_stops, 0);
    }
}
