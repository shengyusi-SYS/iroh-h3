use bytes::Bytes;
use futures::StreamExt;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use tracing::instrument;

use crate::cancel::{CancellableH3Body, LegacyCompatibleH3ResponseBody};
use crate::error::Error;

/// An HTTP/3 body that can be either a fixed set of bytes or a streaming body.
#[derive(Debug, Default)]
pub struct Body {
    inner: Inner,
}

impl Body {
    /// Create an empty body.
    pub fn empty() -> Self {
        Self {
            inner: Inner::Bytes(Bytes::new()),
        }
    }

    /// Create a body from the given bytes.
    pub fn bytes(bytes: Bytes) -> Self {
        Self {
            inner: Inner::Bytes(bytes),
        }
    }

    /// Consume the body and return its contents as bytes.
    #[instrument]
    pub async fn into_bytes(self) -> Result<Bytes, Error> {
        match self.inner {
            Inner::Bytes(bytes) => Ok(bytes),
            Inner::Stream(box_body) => collect_box_body(box_body).await,
            Inner::CancellableH3(body) => {
                collect_box_body(LegacyCompatibleH3ResponseBody::new(body).boxed()).await
            }
        }
    }

    /// Consume the body and return it as a streaming body.
    pub fn into_stream(self) -> BoxBody<Bytes, Error> {
        match self.inner {
            Inner::Stream(box_body) => box_body,
            Inner::Bytes(bytes) => Full::new(bytes).map_err(Error::from).boxed(),
            Inner::CancellableH3(body) => LegacyCompatibleH3ResponseBody::new(body).boxed(),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn into_fixed_bytes_for_cancellable_request(self) -> Result<Bytes, Error> {
        match self.inner {
            Inner::Bytes(bytes) => Ok(bytes),
            Inner::Stream(_) | Inner::CancellableH3(_) => Err(Error::RequestBodyNotCancellable),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn cancellable_h3(body: CancellableH3Body) -> Self {
        Self {
            inner: Inner::CancellableH3(body),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn into_cancellable_h3(self) -> Result<CancellableH3Body, Error> {
        match self.inner {
            Inner::CancellableH3(body) => Ok(body),
            Inner::Bytes(_) | Inner::Stream(_) => Err(Error::BodyNotCancellable),
        }
    }

    /// Take the body, replacing it with an empty body.
    pub fn take(&mut self) -> Self {
        if let Inner::Bytes(bytes) = &self.inner {
            return Self::bytes(bytes.clone());
        };
        std::mem::take(self)
    }
}

impl<E> From<BoxBody<Bytes, E>> for Body
where
    E: Into<Error> + 'static,
{
    fn from(value: BoxBody<Bytes, E>) -> Self {
        Self {
            inner: Inner::Stream(value.map_err(E::into).boxed()),
        }
    }
}

#[derive(Debug)]
enum Inner {
    Bytes(Bytes),
    Stream(BoxBody<Bytes, Error>),
    #[allow(dead_code)]
    CancellableH3(CancellableH3Body),
}

impl Default for Inner {
    fn default() -> Self {
        Self::Bytes(Bytes::default())
    }
}

async fn collect_box_body(box_body: BoxBody<Bytes, Error>) -> Result<Bytes, Error> {
    let mut stream = box_body.into_data_stream();
    let mut buffer = Vec::new();
    while let Some(chunk) = stream.next().await.transpose()? {
        buffer.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn non_cancellable_body_returns_body_not_cancellable() {
        let body = Body::bytes(Bytes::from_static(b"plain"));

        let result = body.into_cancellable_h3();

        assert!(matches!(result, Err(Error::BodyNotCancellable)));
    }

    #[tokio::test]
    async fn streaming_request_body_send_cancellable_returns_request_body_not_cancellable() {
        use futures::stream;
        use http_body::Frame;
        use http_body_util::{StreamBody, combinators::BoxBody};

        let stream = stream::iter([Ok::<_, Error>(Frame::data(Bytes::from_static(b"x")))]);
        let body = Body::from(BoxBody::new(StreamBody::new(stream)));

        let result = body.into_fixed_bytes_for_cancellable_request();

        assert!(matches!(result, Err(Error::RequestBodyNotCancellable)));
    }
}
