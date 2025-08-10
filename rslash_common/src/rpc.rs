use futures::FutureExt;
use std::{pin::Pin, task::Poll};

use stubborn_io::tokio::{StubbornIo, UnderlyingIo};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::Duration;
use tracing::warn;

pub struct RetryingTcpStream<T, C>
where
    T: UnderlyingIo<C> + AsyncRead,
    C: Clone + Send + Unpin + 'static,
{
    underlying: StubbornIo<T, C>,
}

impl<T, C> AsyncRead for RetryingTcpStream<T, C>
where
    T: UnderlyingIo<C> + AsyncRead,
    C: Clone + Send + Unpin + 'static,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match Pin::new(&mut self.underlying).poll_read(cx, buf) {
            Poll::Ready(x) => match x {
                Ok(x) => Poll::Ready(Ok(x)),
                Err(e) => {
                    warn!("Error with underlying: {}", e);
                    match Box::pin(tokio::time::sleep(Duration::from_millis(500))).poll_unpin(cx) {
                        Poll::Ready(_) => {}
                        Poll::Pending => return Poll::Pending,
                    };
                    self.poll_read(cx, buf)
                }
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T, C> AsyncWrite for RetryingTcpStream<T, C>
where
    T: UnderlyingIo<C> + AsyncWrite + AsyncRead,
    C: Clone + Send + Unpin + 'static,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        match Pin::new(&mut self.underlying).poll_write(cx, buf) {
            Poll::Ready(x) => match x {
                Ok(x) => Poll::Ready(Ok(x)),
                Err(e) => {
                    warn!("Error with underlying: {}", e);
                    match Box::pin(tokio::time::sleep(Duration::from_millis(500))).poll_unpin(cx) {
                        Poll::Ready(_) => {}
                        Poll::Pending => return Poll::Pending,
                    };
                    self.poll_write(cx, buf)
                }
            },
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.underlying).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.underlying).poll_shutdown(cx)
    }
}

impl<T, C> RetryingTcpStream<T, C>
where
    T: UnderlyingIo<C> + AsyncWrite + AsyncRead,
    C: Clone + Send + Unpin + 'static,
{
    pub fn new(underlying: StubbornIo<T, C>) -> Self {
        Self { underlying }
    }
}
