use std::io;
use std::os::unix::net::SocketAddr;
use std::path::PathBuf;

use futures::{Async, AsyncSink, Poll, Sink, StartSend, Stream};

#[cfg(feature = "unstable-futures")]
use futures2::{self, task};
#[cfg(feature = "unstable-futures")]
use futures_sink;

use UnixDatagram;

/// Encoding of frames via buffers.
///
/// This trait is used when constructing an instance of `UnixDatagramFramed` and
/// provides the `In` and `Out` types which are decoded and encoded from the
/// socket, respectively.
///
/// Because Unix datagrams are a connectionless protocol, the `decode` method
/// receives the address where data came from and the `encode` method is also
/// responsible for determining the remote host to which the datagram should be
/// sent
///
/// The trait itself is implemented on a type that can track state for decoding
/// or encoding, which is particularly useful for streaming parsers. In many
/// cases, though, this type will simply be a unit struct (e.g. `struct
/// HttpCodec`).
pub trait UnixDatagramCodec {
    /// The type of decoded frames.
    type In;

    /// The type of frames to be encoded.
    type Out;

    /// Attempts to decode a frame from the provided buffer of bytes.
    ///
    /// This method is called by `UnixDatagramFramed` on a single datagram which
    /// has been read from a socket. The `buf` argument contains the data that
    /// was received from the remote address, and `src` is the address the data
    /// came from. Note that typically this method should require the entire
    /// contents of `buf` to be valid or otherwise return an error with
    /// trailing data.
    ///
    /// Finally, if the bytes in the buffer are malformed then an error is
    /// returned indicating why. This informs `Framed` that the stream is now
    /// corrupt and should be terminated.
    fn decode(&mut self, src: &SocketAddr, buf: &[u8]) -> io::Result<Self::In>;

    /// Encodes a frame into the buffer provided.
    ///
    /// This method will encode `msg` into the byte buffer provided by `buf`.
    /// The `buf` provided is an internal buffer of the `Framed` instance and
    /// will be written out when possible.
    ///
    /// The encode method also determines the destination to which the buffer
    /// should be directed, which will be returned as a `SocketAddr`.
    fn encode(&mut self, msg: Self::Out, buf: &mut Vec<u8>) -> io::Result<PathBuf>;
}

/// A unified `Stream` and `Sink` interface to an underlying
/// `UnixDatagramSocket`, using the `UnixDatagramCodec` trait to encode and
/// decode frames.
///
/// You can acquire a `UnixDatagramFramed` instance by using the
/// `UnixDatagramSocket::framed` adapter.
pub struct UnixDatagramFramed<C> {
    socket: UnixDatagram,
    codec: C,
    rd: Vec<u8>,
    wr: Vec<u8>,
    out_addr: PathBuf,
}

impl<C: UnixDatagramCodec> Stream for UnixDatagramFramed<C> {
    type Item = C::In;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<C::In>, io::Error> {
        let (n, addr) = try_ready!(self.socket.recv_from(&mut self.rd));
        trace!("received {} bytes, decoding", n);
        let frame = try!(self.codec.decode(&addr, &self.rd[..n]));
        trace!("frame decoded from buffer");
        Ok(Async::Ready(Some(frame)))
    }
}

#[cfg(feature = "unstable-futures")]
impl<C: UnixDatagramCodec> futures2::Stream for UnixDatagramFramed<C> {
    type Item = C::In;
    type Error = io::Error;

    fn poll_next(&mut self, cx: &mut task::Context) -> futures2::Poll<Option<C::In>, io::Error> {
        let (n, addr) = try_ready2!(self.socket.recv_from2(cx, &mut self.rd));
        trace!("received {} bytes, decoding", n);
        let frame = try!(self.codec.decode(&addr, &self.rd[..n]));
        trace!("frame decoded from buffer");
        Ok(futures2::Async::Ready(Some(frame)))
    }
}

impl<C: UnixDatagramCodec> Sink for UnixDatagramFramed<C> {
    type SinkItem = C::Out;
    type SinkError = io::Error;

    fn start_send(&mut self, item: C::Out) -> StartSend<C::Out, io::Error> {
        if self.wr.len() > 0 {
            try!(self.poll_complete());
            if self.wr.len() > 0 {
                return Ok(AsyncSink::NotReady(item));
            }
        }

        self.out_addr = try!(self.codec.encode(item, &mut self.wr));
        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), io::Error> {
        trace!("flushing framed transport");

        if self.wr.is_empty() {
            return Ok(Async::Ready(()));
        }

        trace!("writing; remaining={}", self.wr.len());
        let n = try_ready!(self.socket.send_to(&self.wr, &self.out_addr));
        trace!("written {}", n);
        let wrote_all = n == self.wr.len();
        self.wr.clear();
        if wrote_all {
            Ok(Async::Ready(()))
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "failed to write entire datagram to socket",
            ))
        }
    }

    fn close(&mut self) -> Poll<(), io::Error> {
        try_ready!(self.poll_complete());
        Ok(().into())
    }
}

#[cfg(feature = "unstable-futures")]
impl<C: UnixDatagramCodec> futures_sink::Sink for UnixDatagramFramed<C> {
    type SinkItem = C::Out;
    type SinkError = io::Error;

    fn poll_ready(&mut self, cx: &mut task::Context) -> futures2::Poll<(), io::Error> {
        if self.wr.len() > 0 {
            try!(self.poll_flush(cx));
            if self.wr.len() > 0 {
                return Ok(futures2::Async::Pending);
            }
        }
        Ok(().into())
    }

    fn start_send(&mut self, item: C::Out) -> Result<(), io::Error> {
        self.out_addr = try!(self.codec.encode(item, &mut self.wr));
        Ok(())
    }

    fn poll_flush(&mut self, cx: &mut task::Context) -> futures2::Poll<(), io::Error> {
        trace!("flushing framed transport");

        if self.wr.is_empty() {
            return Ok(futures2::Async::Ready(()));
        }

        trace!("writing; remaining={}", self.wr.len());
        let n = try_ready2!(self.socket.send_to2(cx, &self.wr, &self.out_addr));
        trace!("written {}", n);
        let wrote_all = n == self.wr.len();
        self.wr.clear();
        if wrote_all {
            Ok(futures2::Async::Ready(()))
        } else {
            Err(io::Error::new(
                io::ErrorKind::Other,
                "failed to write entire datagram to socket",
            ))
        }
    }

    fn poll_close(&mut self, cx: &mut task::Context) -> futures2::Poll<(), io::Error> {
        self.poll_flush(cx)
    }
}

pub fn new<C: UnixDatagramCodec>(socket: UnixDatagram, codec: C) -> UnixDatagramFramed<C> {
    UnixDatagramFramed {
        socket: socket,
        codec: codec,
        out_addr: PathBuf::new(),
        rd: vec![0; 64 * 1024],
        wr: Vec::with_capacity(8 * 1024),
    }
}

impl<C> UnixDatagramFramed<C> {
    /// Returns a reference to the underlying I/O stream wrapped by `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise being
    /// worked with.
    pub fn get_ref(&self) -> &UnixDatagram {
        &self.socket
    }

    /// Returns a mutable reference to the underlying I/O stream wrapped by
    /// `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise being
    /// worked with.
    pub fn get_mut(&mut self) -> &mut UnixDatagram {
        &mut self.socket
    }

    /// Consumes the `Framed`, returning its underlying I/O stream.
    ///
    /// Note that care should be taken to not tamper with the underlying stream
    /// of data coming in as it may corrupt the stream of frames otherwise being
    /// worked with.
    pub fn into_inner(self) -> UnixDatagram {
        self.socket
    }
}
