use crate::{
    config::Resource,
    event::Event,
    internal_events::{ConnectionOpen, OpenGauge, TcpSendAckError, TcpSocketConnectionError},
    shutdown::ShutdownSignal,
    sources::util::TcpError,
    tcp::TcpKeepaliveConfig,
    tls::{MaybeTlsIncomingStream, MaybeTlsListener, MaybeTlsSettings},
    Pipeline,
};
use bytes::Bytes;
use futures::{future::BoxFuture, FutureExt, Sink, SinkExt, StreamExt};
use listenfd::ListenFd;
use serde::{de, Deserialize, Deserializer, Serialize};
use smallvec::SmallVec;
use socket2::SockRef;
use std::{fmt, io, mem::drop, net::SocketAddr, time::Duration};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    time::sleep,
};
use tokio_util::codec::{Decoder, FramedRead};
use tracing_futures::Instrument;

async fn make_listener(
    addr: SocketListenAddr,
    mut listenfd: ListenFd,
    tls: &MaybeTlsSettings,
) -> Option<MaybeTlsListener> {
    match addr {
        SocketListenAddr::SocketAddr(addr) => match tls.bind(&addr).await {
            Ok(listener) => Some(listener),
            Err(error) => {
                error!(message = "Failed to bind to listener socket.", %error);
                None
            }
        },
        SocketListenAddr::SystemdFd(offset) => match listenfd.take_tcp_listener(offset) {
            Ok(Some(listener)) => match TcpListener::from_std(listener) {
                Ok(listener) => Some(listener.into()),
                Err(error) => {
                    error!(message = "Failed to bind to listener socket.", %error);
                    None
                }
            },
            Ok(None) => {
                error!("Failed to take listen FD, not open or already taken.");
                None
            }
            Err(error) => {
                error!(message = "Failed to take listen FD.", %error);
                None
            }
        },
    }
}

pub trait TcpSource: Clone + Send + Sync + 'static
where
    <<Self as TcpSource>::Decoder as tokio_util::codec::Decoder>::Item: std::marker::Send,
{
    // Should be default: `std::io::Error`.
    // Right now this is unstable: https://github.com/rust-lang/rust/issues/29661
    type Error: From<io::Error> + TcpError + std::fmt::Debug + std::fmt::Display + Send;
    type Item: Into<SmallVec<[Event; 1]>> + Send;
    type Decoder: Decoder<Item = (Self::Item, usize), Error = Self::Error> + Send + 'static;

    fn decoder(&self) -> Self::Decoder;

    fn handle_events(&self, _events: &mut [Event], _host: Bytes, _byte_size: usize) {}

    fn build_ack(&self, _item: &Self::Item) -> Bytes {
        Bytes::new()
    }

    fn run(
        self,
        addr: SocketListenAddr,
        keepalive: Option<TcpKeepaliveConfig>,
        shutdown_timeout_secs: u64,
        tls: MaybeTlsSettings,
        receive_buffer_bytes: Option<usize>,
        shutdown_signal: ShutdownSignal,
        out: Pipeline,
    ) -> crate::Result<crate::sources::Source> {
        let out = out.sink_map_err(|error| error!(message = "Error sending event.", %error));

        let listenfd = ListenFd::from_env();

        Ok(Box::pin(async move {
            let listener = match make_listener(addr, listenfd, &tls).await {
                None => return Err(()),
                Some(listener) => listener,
            };

            info!(
                message = "Listening.",
                addr = %listener
                    .local_addr()
                    .map(SocketListenAddr::SocketAddr)
                    .unwrap_or(addr)
            );

            let tripwire = shutdown_signal.clone();
            let tripwire = async move {
                let _ = tripwire.await;
                sleep(Duration::from_secs(shutdown_timeout_secs)).await;
            }
            .shared();

            let connection_gauge = OpenGauge::new();
            let shutdown_clone = shutdown_signal.clone();

            listener
                .accept_stream()
                .take_until(shutdown_clone)
                .for_each(move |connection| {
                    let shutdown_signal = shutdown_signal.clone();
                    let tripwire = tripwire.clone();
                    let source = self.clone();
                    let out = out.clone();
                    let connection_gauge = connection_gauge.clone();

                    async move {
                        let socket = match connection {
                            Ok(socket) => socket,
                            Err(error) => {
                                error!(
                                    message = "Failed to accept socket.",
                                    %error
                                );
                                return;
                            }
                        };

                        let peer_addr = socket.peer_addr().ip().to_string();
                        let span = info_span!("connection", %peer_addr);
                        let host = Bytes::from(peer_addr);

                        let tripwire = tripwire
                            .map(move |_| {
                                info!(
                                    message = "Resetting connection (still open after seconds).",
                                    seconds = ?shutdown_timeout_secs
                                );
                            })
                            .boxed();

                        span.in_scope(|| {
                            let peer_addr = socket.peer_addr();
                            debug!(message = "Accepted a new connection.", peer_addr = %peer_addr);

                            let open_token =
                                connection_gauge.open(|count| emit!(ConnectionOpen { count }));

                            let fut = handle_stream(
                                shutdown_signal,
                                socket,
                                keepalive,
                                receive_buffer_bytes,
                                source,
                                tripwire,
                                host,
                                out,
                            );

                            tokio::spawn(
                                fut.map(move |()| drop(open_token)).instrument(span.clone()),
                            );
                        });
                    }
                })
                .map(Ok)
                .await
        }))
    }
}

async fn handle_stream<T>(
    mut shutdown_signal: ShutdownSignal,
    mut socket: MaybeTlsIncomingStream<TcpStream>,
    keepalive: Option<TcpKeepaliveConfig>,
    receive_buffer_bytes: Option<usize>,
    source: T,
    mut tripwire: BoxFuture<'static, ()>,
    host: Bytes,
    mut out: impl Sink<Event> + Send + 'static + Unpin,
) where
    <<T as TcpSource>::Decoder as tokio_util::codec::Decoder>::Item: std::marker::Send,
    T: TcpSource,
{
    tokio::select! {
        result = socket.handshake() => {
            if let Err(error) = result {
                emit!(TcpSocketConnectionError { error });
                return;
            }
        },
        _ = &mut shutdown_signal => {
            return;
        }
    };

    if let Some(keepalive) = keepalive {
        if let Err(error) = socket.set_keepalive(keepalive) {
            warn!(message = "Failed configuring TCP keepalive.", %error);
        }
    }

    if let Some(receive_buffer_bytes) = receive_buffer_bytes {
        if let Err(error) = socket.set_receive_buffer_bytes(receive_buffer_bytes) {
            warn!(message = "Failed configuring receive buffer size on TCP socket.", %error);
        }
    }

    let mut reader = FramedRead::new(socket, source.decoder());

    loop {
        tokio::select! {
            _ = &mut tripwire => break,
            _ = &mut shutdown_signal => {
                debug!("Start graceful shutdown.");
                // Close our write part of TCP socket to signal the other side
                // that it should stop writing and close the channel.
                let socket = reader.get_ref();
                if let Some(stream) = socket.get_ref() {
                    let socket = SockRef::from(stream);
                    if let Err(error) = socket.shutdown(std::net::Shutdown::Write) {
                        warn!(message = "Failed in signalling to the other side to close the TCP channel.", %error);
                    }
                } else {
                    // Connection hasn't yet been established so we are done here.
                    debug!("Closing connection that hasn't yet been fully established.");
                    break;
                }
            },
            res = reader.next() => {
                match res {
                    Some(Ok((item, byte_size))) => {
                        let ack = source.build_ack(&item);
                        let mut events = item.into();
                        source.handle_events(&mut events, host.clone(), byte_size);
                        for event in events {
                            match out.send(event).await {
                                Ok(_) => {
                                    let stream = reader.get_mut();
                                    if let Err(error) = stream.write_all(&ack).await {
                                        emit!(TcpSendAckError{ error });
                                        break;
                                    }
                                }
                                Err(_) => {
                                    warn!("Failed to send event.");
                                    break;
                                }
                            }
                        }
                    }
                    Some(Err(error)) => {
                        if !<<T as TcpSource>::Error as TcpError>::can_continue(&error) {
                            warn!(message = "Failed to read data from TCP source.", %error);
                            break;
                        }
                    }
                    None => {
                        debug!("Connection closed.");
                        break
                    },
                }
            }
            else => break,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SocketListenAddr {
    SocketAddr(SocketAddr),
    #[serde(deserialize_with = "parse_systemd_fd")]
    SystemdFd(usize),
}

impl fmt::Display for SocketListenAddr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::SocketAddr(ref addr) => addr.fmt(f),
            Self::SystemdFd(offset) => write!(f, "systemd socket #{}", offset),
        }
    }
}

impl From<SocketAddr> for SocketListenAddr {
    fn from(addr: SocketAddr) -> Self {
        Self::SocketAddr(addr)
    }
}

impl From<SocketListenAddr> for Resource {
    fn from(addr: SocketListenAddr) -> Resource {
        match addr {
            SocketListenAddr::SocketAddr(addr) => Resource::tcp(addr),
            SocketListenAddr::SystemdFd(offset) => Self::SystemFdOffset(offset),
        }
    }
}

fn parse_systemd_fd<'de, D>(des: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    let s: &'de str = Deserialize::deserialize(des)?;
    match s {
        "systemd" => Ok(0),
        s if s.starts_with("systemd#") => s[8..]
            .parse::<usize>()
            .map_err(de::Error::custom)?
            .checked_sub(1)
            .ok_or_else(|| de::Error::custom("systemd indices start from 1, found 0")),
        _ => Err(de::Error::custom("must start with \"systemd\"")),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use serde::Deserialize;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    #[derive(Debug, Deserialize)]
    struct Config {
        addr: SocketListenAddr,
    }

    #[test]
    fn parse_socket_listen_addr() {
        let test: Config = toml::from_str(r#"addr="127.1.2.3:1234""#).unwrap();
        assert_eq!(
            test.addr,
            SocketListenAddr::SocketAddr(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(127, 1, 2, 3),
                1234,
            )))
        );
        let test: Config = toml::from_str(r#"addr="systemd""#).unwrap();
        assert_eq!(test.addr, SocketListenAddr::SystemdFd(0));
        let test: Config = toml::from_str(r#"addr="systemd#3""#).unwrap();
        assert_eq!(test.addr, SocketListenAddr::SystemdFd(2));
    }
}
