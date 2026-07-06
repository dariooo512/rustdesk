use hbb_common::{
    anyhow,
    bytes::{Bytes, BytesMut},
    bytes_codec::BytesCodec,
    config, log,
    tcp::{DynTcpStream, FramedStream},
    tokio::{
        self,
        io::{AsyncRead, AsyncWrite, ReadBuf},
        net::UdpSocket,
        sync::mpsc,
        sync::oneshot,
    },
    tokio_util, ResultType, Stream,
};
use kcp_sys::{
    endpoint::KcpEndpoint,
    packet_def::{KcpPacket, KcpPacketHeader},
    stream,
};
use std::{
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

pub struct KcpStream {
    _endpoint: KcpEndpoint,
    stop_sender: Option<oneshot::Sender<()>>,
}

impl KcpStream {
    fn create_framed(stream: stream::KcpStream, local_addr: Option<SocketAddr>) -> Stream {
        Stream::Tcp(FramedStream(
            tokio_util::codec::Framed::new(DynTcpStream(Box::new(stream)), BytesCodec::new()),
            local_addr.unwrap_or(config::Config::get_any_listen_addr(true)),
            None,
            0,
        ))
    }

    pub async fn accept(
        udp_socket: Arc<UdpSocket>,
        timeout: std::time::Duration,
        init_packet: Option<BytesMut>,
    ) -> ResultType<(Self, Stream)> {
        let mut endpoint = KcpEndpoint::new();
        endpoint.run().await;

        let (input, output) = (
            endpoint.input_sender(),
            endpoint
                .output_receiver()
                .ok_or_else(|| anyhow::anyhow!("Failed to get output receiver"))?,
        );
        let (stop_sender, stop_receiver) = oneshot::channel();
        if let Some(packet) = init_packet {
            if packet.len() >= std::mem::size_of::<KcpPacketHeader>() {
                input.send(packet.into()).await?;
            }
        }
        Self::kcp_io(udp_socket.clone(), input, output, stop_receiver).await;

        let conn_id = tokio::time::timeout(timeout, endpoint.accept()).await??;
        if let Some(stream) = stream::KcpStream::new(&endpoint, conn_id) {
            Ok((
                Self {
                    _endpoint: endpoint,
                    stop_sender: Some(stop_sender),
                },
                Self::create_framed(stream, udp_socket.local_addr().ok()),
            ))
        } else {
            Err(anyhow::anyhow!("Failed to create KcpStream"))
        }
    }

    pub async fn connect(
        udp_socket: Arc<UdpSocket>,
        timeout: std::time::Duration,
    ) -> ResultType<(Self, Stream)> {
        let mut endpoint = KcpEndpoint::new();
        endpoint.run().await;

        let (input, output) = (
            endpoint.input_sender(),
            endpoint
                .output_receiver()
                .ok_or_else(|| anyhow::anyhow!("Failed to get output receiver"))?,
        );
        let (stop_sender, stop_receiver) = oneshot::channel();
        Self::kcp_io(udp_socket.clone(), input, output, stop_receiver).await;

        let conn_id = endpoint.connect(timeout, 0, 0, Bytes::new()).await?;
        if let Some(stream) = stream::KcpStream::new(&endpoint, conn_id) {
            Ok((
                Self {
                    _endpoint: endpoint,
                    stop_sender: Some(stop_sender),
                },
                Self::create_framed(stream, udp_socket.local_addr().ok()),
            ))
        } else {
            Err(anyhow::anyhow!("Failed to create KcpStream"))
        }
    }

    /// RentaMac: self-contained KCP connect for the relay path, where there is no
    /// slot to thread the `KcpStream` guard through. The endpoint and io loop are
    /// bundled into the returned `Stream` (via `KcpConnOwned`), so the connection is
    /// torn down when the stream drops. Uses a UDP socket already connected to the
    /// relay (send/recv on the dedicated socket).
    pub async fn connect_owned(
        udp_socket: Arc<UdpSocket>,
        timeout: Duration,
    ) -> ResultType<Stream> {
        let mut endpoint = KcpEndpoint::new();
        endpoint.run().await;

        let input = endpoint.input_sender();
        let mut output = endpoint
            .output_receiver()
            .ok_or_else(|| anyhow::anyhow!("Failed to get output receiver"))?;
        let (stop_io_tx, mut stop_io_rx) = oneshot::channel();
        let (stop_ep_tx, stop_ep_rx) = oneshot::channel();

        let udp = udp_socket.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 1500];
            loop {
                tokio::select! {
                    _ = &mut stop_io_rx => break,
                    Some(data) = output.recv() => {
                        if let Err(e) = udp.send(&data.inner()).await {
                            log::debug!("KCP owned send error: {e:?}");
                            break;
                        }
                    }
                    result = udp.recv_from(&mut buf) => {
                        match result {
                            Ok((n, _)) => {
                                if n >= std::mem::size_of::<KcpPacketHeader>() {
                                    input.send(BytesMut::from(&buf[..n]).into()).await.ok();
                                }
                            }
                            Err(e) => {
                                log::debug!("KCP owned recv_from error: {e:?}");
                                break;
                            }
                        }
                    }
                    else => break,
                }
            }
        });

        let conn_id = endpoint.connect(timeout, 0, 0, Bytes::new()).await?;
        let inner = stream::KcpStream::new(&endpoint, conn_id)
            .ok_or_else(|| anyhow::anyhow!("Failed to create KcpStream"))?;
        // Keep the endpoint alive alongside the stream, off the boxed KcpConnOwned.
        tokio::spawn(async move {
            let _endpoint = endpoint;
            let _ = stop_ep_rx.await;
        });

        let local_addr = udp_socket
            .local_addr()
            .unwrap_or_else(|_| config::Config::get_any_listen_addr(true));
        let conn = KcpConnOwned {
            inner,
            stop_io: Some(stop_io_tx),
            stop_ep: Some(stop_ep_tx),
        };
        Ok(Stream::Tcp(FramedStream(
            tokio_util::codec::Framed::new(DynTcpStream(Box::new(conn)), BytesCodec::new()),
            local_addr,
            None,
            0,
        )))
    }

    async fn kcp_io(
        udp_socket: Arc<UdpSocket>,
        input: mpsc::Sender<KcpPacket>,
        mut output: mpsc::Receiver<KcpPacket>,
        mut stop_receiver: oneshot::Receiver<()>,
    ) {
        let udp = udp_socket.clone();
        tokio::spawn(async move {
            let mut buf = vec![0; 1500];
            loop {
                tokio::select! {
                    _ = &mut stop_receiver => {
                        log::debug!("KCP io loop received stop signal");
                        break;
                    }
                    Some(data) = output.recv() => {
                        if let Err(e) = udp.send(&data.inner()).await {
                            log::debug!("KCP send error: {:?}", e);
                            break;
                        }
                    }
                    result = udp.recv_from(&mut buf) => {
                        match result {
                            Ok((size, _)) => {
                                if size < std::mem::size_of::<KcpPacketHeader>() {
                                    continue;
                                }
                                input
                                    .send(BytesMut::from(&buf[..size]).into())
                                    .await.ok();
                            }
                            Err(e) => {
                                log::debug!("KCP recv_from error: {:?}", e);
                                break;
                            }
                        }
                    }
                    else => {
                        log::debug!("KCP endpoint input closed");
                        break;
                    }
                }
            }
        });
    }
}

impl Drop for KcpStream {
    fn drop(&mut self) {
        if let Some(sender) = self.stop_sender.take() {
            let _ = sender.send(());
        }
    }
}

/// RentaMac: a self-contained KCP connection surfaced as a byte stream for the relay
/// path. It owns the io-loop and endpoint stop signals, so dropping it (when the
/// relay `Stream` is dropped) tears the connection down without a separate guard.
/// See `KcpStream::connect_owned`.
struct KcpConnOwned {
    inner: stream::KcpStream,
    stop_io: Option<oneshot::Sender<()>>,
    stop_ep: Option<oneshot::Sender<()>>,
}

impl AsyncRead for KcpConnOwned {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for KcpConnOwned {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl Drop for KcpConnOwned {
    fn drop(&mut self) {
        if let Some(s) = self.stop_io.take() {
            let _ = s.send(());
        }
        if let Some(s) = self.stop_ep.take() {
            let _ = s.send(());
        }
    }
}
