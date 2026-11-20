use axum::serve::Listener;
use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    time::{Sleep, sleep},
};

const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct TimedListener(pub TcpListener);

impl Listener for TimedListener {
    type Io = TimedSocket;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (socket, address) = Listener::accept(&mut self.0).await;
        (
            TimedSocket {
                socket,
                write_deadline: None,
            },
            address,
        )
    }
    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.0.local_addr()
    }
}

pub(super) struct TimedSocket {
    socket: TcpStream,
    write_deadline: Option<Pin<Box<Sleep>>>,
}

impl TimedSocket {
    fn with_write_timeout<T>(
        &mut self,
        context: &mut Context<'_>,
        result: Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        if result.is_ready() {
            self.write_deadline = None;
            return result;
        }
        let deadline = self
            .write_deadline
            .get_or_insert_with(|| Box::pin(sleep(WRITE_TIMEOUT)));
        if deadline.as_mut().poll(context).is_ready() {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP socket write stalled",
            )));
        }
        Poll::Pending
    }
}

impl AsyncRead for TimedSocket {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_read(context, bytes)
    }
}

impl AsyncWrite for TimedSocket {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.socket).poll_write(context, bytes);
        self.with_write_timeout(context, result)
    }
    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.socket).poll_flush(context);
        self.with_write_timeout(context, result)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_shutdown(context)
    }
}
