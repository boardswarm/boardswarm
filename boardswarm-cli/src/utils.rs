use std::{pin::Pin, task::Poll};

use bytes::BytesMut;
use futures::ready;
use tokio::io::{AsyncSeek, AsyncWrite};

enum BatchState {
    Empty,
    Batched(BytesMut),
}

pub struct BatchWriter<T> {
    blocksize: usize,
    writer: T,
    state: BatchState,
    pending_seek: Option<std::io::SeekFrom>,
}

impl<T> BatchWriter<T> {
    pub fn new(writer: T, blocksize: usize) -> Self {
        let state = BatchState::Empty;
        Self {
            blocksize,
            writer,
            state,
            pending_seek: None,
        }
    }
}

impl<T> AsyncWrite for BatchWriter<T>
where
    T: AsyncWrite + Unpin,
{
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        let me = self.get_mut();
        match me.state {
            BatchState::Empty => {
                if buf.len() >= me.blocksize {
                    Pin::new(&mut me.writer).poll_write(cx, buf)
                } else {
                    let mut batch = BytesMut::with_capacity(me.blocksize);
                    batch.extend_from_slice(buf);
                    me.state = BatchState::Batched(batch);
                    Poll::Ready(Ok(buf.len()))
                }
            }
            BatchState::Batched(ref mut b) => {
                let cached = b.len();
                if buf.len() + cached < me.blocksize {
                    b.extend_from_slice(buf);
                    Poll::Ready(Ok(buf.len()))
                } else {
                    let remaining = me.blocksize - cached;
                    b.extend_from_slice(&buf[0..remaining]);
                    match Pin::new(&mut me.writer).poll_write(cx, b) {
                        Poll::Ready(Ok(written)) if written >= b.len() => {
                            me.state = BatchState::Empty;
                            Poll::Ready(Ok(remaining))
                        }
                        Poll::Ready(Ok(written)) => {
                            let _ = b.split_to(written);
                            Poll::Ready(Ok(remaining))
                        }
                        Poll::Ready(Err(e)) => {
                            b.truncate(cached);
                            Poll::Ready(Err(e))
                        }
                        Poll::Pending => {
                            b.truncate(cached);
                            Poll::Pending
                        }
                    }
                }
            }
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        let me = self.get_mut();
        loop {
            match me.state {
                BatchState::Empty => return Pin::new(&mut me.writer).poll_flush(cx),
                BatchState::Batched(ref mut b) => {
                    let written = ready!(Pin::new(&mut me.writer).poll_write(cx, b))?;
                    if written >= b.len() {
                        me.state = BatchState::Empty;
                    } else {
                        let _ = b.split_to(written);
                    }
                }
            }
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        self.poll_flush(cx)
    }
}

impl<T> AsyncSeek for BatchWriter<T>
where
    T: AsyncSeek + AsyncWrite + Unpin,
{
    fn start_seek(
        self: std::pin::Pin<&mut Self>,
        position: std::io::SeekFrom,
    ) -> std::io::Result<()> {
        let me = self.get_mut();
        match me.state {
            BatchState::Empty => Pin::new(&mut me.writer).start_seek(position),
            BatchState::Batched(_) => {
                me.pending_seek = Some(position);
                Ok(())
            }
        }
    }

    fn poll_complete(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<u64>> {
        let me = self.get_mut();
        match me.state {
            BatchState::Empty => Pin::new(&mut me.writer).poll_complete(cx),
            BatchState::Batched(_) => {
                ready!(Pin::new(&mut *me).poll_flush(cx))?;
                Pin::new(&mut me.writer).start_seek(me.pending_seek.take().unwrap())?;
                Pin::new(&mut me.writer).poll_complete(cx)
            }
        }
    }
}
