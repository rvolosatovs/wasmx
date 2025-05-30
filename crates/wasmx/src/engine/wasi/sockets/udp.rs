use core::future::Future;
use core::net::SocketAddr;

use std::sync::Arc;

use cap_net_ext::{AddressFamily, Blocking, UdpSocketExt as _};
use io_lifetimes::raw::{FromRawSocketlike as _, IntoRawSocketlike as _};
use io_lifetimes::AsSocketlike as _;
use rustix::fd::AsFd;
use rustix::io::Errno;
use rustix::net::{connect, connect_unspec};
use tracing::debug;

use crate::engine::bindings::wasi::sockets::network::{
    ErrorCode, IpAddressFamily, IpSocketAddress,
};
use crate::engine::wasi::sockets::util::{
    get_unicast_hop_limit, is_valid_address_family, is_valid_remote_address, receive_buffer_size,
    send_buffer_size, set_receive_buffer_size, set_send_buffer_size, set_unicast_hop_limit,
};
use crate::engine::wasi::sockets::SocketAddressFamily;

pub struct IncomingDatagramStream;
pub struct OutgoingDatagramStream;

/// Theoretical maximum byte size of a UDP datagram, the real limit is lower,
/// but we do not account for e.g. the transport layer here for simplicity.
/// In practice, datagrams are typically less than 1500 bytes.
pub const MAX_UDP_DATAGRAM_SIZE: usize = u16::MAX as usize;

/// The state of a UDP socket.
///
/// This represents the various states a socket can be in during the
/// activities of binding, and connecting.
enum UdpState {
    /// The initial state for a newly-created socket.
    Default,

    /// Binding finished via `finish_bind`. The socket has an address but
    /// is not yet listening for connections.
    Bound,

    /// The socket is "connected" to a peer address.
    Connected(SocketAddr),
}

/// A host UDP socket, plus associated bookkeeping.
///
/// The inner state is wrapped in an Arc because the same underlying socket is
/// used for implementing the stream types.
pub struct UdpSocket {
    socket: Arc<tokio::net::UdpSocket>,

    /// The current state in the bind/connect progression.
    udp_state: UdpState,

    /// Socket address family.
    family: SocketAddressFamily,
}

impl UdpSocket {
    /// Create a new socket in the given family.
    pub fn new(family: AddressFamily) -> std::io::Result<Self> {
        // Delegate socket creation to cap_net_ext. They handle a couple of things for us:
        // - On Windows: call WSAStartup if not done before.
        // - Set the NONBLOCK and CLOEXEC flags. Either immediately during socket creation,
        //   or afterwards using ioctl or fcntl. Exact method depends on the platform.

        let fd = cap_std::net::UdpSocket::new(family, Blocking::No)?;

        let socket_address_family = match family {
            AddressFamily::Ipv4 => SocketAddressFamily::Ipv4,
            AddressFamily::Ipv6 => {
                rustix::net::sockopt::set_ipv6_v6only(&fd, true)?;
                SocketAddressFamily::Ipv6
            }
        };

        let socket = tokio::net::UdpSocket::try_from(unsafe {
            std::net::UdpSocket::from_raw_socketlike(fd.into_raw_socketlike())
        })?;
        Ok(Self {
            socket: Arc::new(socket),
            udp_state: UdpState::Default,
            family: socket_address_family,
        })
    }

    pub fn bind(&mut self, addr: SocketAddr) -> Result<(), ErrorCode> {
        if !matches!(self.udp_state, UdpState::Default) {
            return Err(ErrorCode::InvalidState);
        }
        if !is_valid_address_family(addr.ip(), self.family) {
            return Err(ErrorCode::InvalidArgument);
        }
        bind(&self.socket, addr)?;
        self.udp_state = UdpState::Bound;
        Ok(())
    }

    pub fn disconnect(&mut self) -> Result<(), ErrorCode> {
        if !matches!(self.udp_state, UdpState::Connected(..)) {
            return Err(ErrorCode::InvalidState);
        }
        disconnect(&self.socket)?;
        self.udp_state = UdpState::Bound;
        Ok(())
    }

    pub fn connect(&mut self, addr: SocketAddr) -> Result<(), ErrorCode> {
        if !is_valid_address_family(addr.ip(), self.family) || !is_valid_remote_address(addr) {
            return Err(ErrorCode::InvalidArgument);
        }

        // We disconnect & (re)connect in two distinct steps for two reasons:
        // - To leave our socket instance in a consistent state in case the
        //   connect fails.
        // - When reconnecting to a different address, Linux sometimes fails
        //   if there isn't a disconnect in between.

        // Step #1: Disconnect
        if let UdpState::Connected(..) = self.udp_state {
            disconnect(&self.socket)?;
            self.udp_state = UdpState::Bound;
        }
        // Step #2: (Re)connect
        connect(&self.socket, &addr).map_err(|error| match error {
            Errno::AFNOSUPPORT => ErrorCode::InvalidArgument, // See `bind` implementation.
            Errno::INPROGRESS => {
                debug!("UDP connect returned EINPROGRESS, which should never happen");
                ErrorCode::Unknown
            }
            err => err.into(),
        })?;
        self.udp_state = UdpState::Connected(addr);
        Ok(())
    }

    pub fn send(&self, buf: Vec<u8>) -> impl Future<Output = Result<(), ErrorCode>> + use<> {
        let socket = if let UdpState::Connected(..) = self.udp_state {
            Ok(Arc::clone(&self.socket))
        } else {
            Err(ErrorCode::InvalidArgument)
        };
        async move {
            let socket = socket?;
            send(&socket, &buf).await
        }
    }

    pub fn send_to(
        &self,
        buf: Vec<u8>,
        addr: SocketAddr,
    ) -> impl Future<Output = Result<(), ErrorCode>> + use<> {
        enum Mode {
            Send(Arc<tokio::net::UdpSocket>),
            SendTo(Arc<tokio::net::UdpSocket>, SocketAddr),
        }
        let socket = match &self.udp_state {
            UdpState::Default | UdpState::Bound => Ok(Mode::SendTo(Arc::clone(&self.socket), addr)),
            UdpState::Connected(caddr) if addr == *caddr => {
                Ok(Mode::Send(Arc::clone(&self.socket)))
            }
            UdpState::Connected(..) => Err(ErrorCode::InvalidArgument),
        };
        async move {
            match socket? {
                Mode::Send(socket) => send(&socket, &buf).await,
                Mode::SendTo(socket, addr) => send_to(&socket, &buf, addr).await,
            }
        }
    }

    pub fn receive(
        &self,
    ) -> impl Future<Output = Result<(Vec<u8>, IpSocketAddress), ErrorCode>> + use<> {
        enum Mode {
            Recv(Arc<tokio::net::UdpSocket>, IpSocketAddress),
            RecvFrom(Arc<tokio::net::UdpSocket>),
        }
        let socket = match self.udp_state {
            UdpState::Default => Err(ErrorCode::InvalidState),
            UdpState::Bound => Ok(Mode::RecvFrom(Arc::clone(&self.socket))),
            UdpState::Connected(addr) => Ok(Mode::Recv(Arc::clone(&self.socket), addr.into())),
        };
        async move {
            let socket = socket?;
            let mut buf = vec![0; MAX_UDP_DATAGRAM_SIZE];
            let (n, addr) = match socket {
                Mode::Recv(socket, addr) => {
                    let n = socket.recv(&mut buf).await?;
                    (n, addr)
                }
                Mode::RecvFrom(socket) => {
                    let (n, addr) = socket.recv_from(&mut buf).await?;
                    (n, addr.into())
                }
            };
            buf.truncate(n);
            Ok((buf, addr))
        }
    }

    pub fn local_address(&self) -> Result<IpSocketAddress, ErrorCode> {
        if matches!(self.udp_state, UdpState::Default) {
            return Err(ErrorCode::InvalidState);
        }
        let addr = self
            .socket
            .as_socketlike_view::<std::net::UdpSocket>()
            .local_addr()?;
        Ok(addr.into())
    }

    pub fn remote_address(&self) -> Result<IpSocketAddress, ErrorCode> {
        if !matches!(self.udp_state, UdpState::Connected(..)) {
            return Err(ErrorCode::InvalidState);
        }
        let addr = self
            .socket
            .as_socketlike_view::<std::net::UdpSocket>()
            .peer_addr()?;
        Ok(addr.into())
    }

    pub fn address_family(&self) -> IpAddressFamily {
        match self.family {
            SocketAddressFamily::Ipv4 => IpAddressFamily::Ipv4,
            SocketAddressFamily::Ipv6 => IpAddressFamily::Ipv6,
        }
    }

    pub fn unicast_hop_limit(&self) -> Result<u8, ErrorCode> {
        get_unicast_hop_limit(&self.socket, self.family)
    }

    pub fn set_unicast_hop_limit(&self, value: u8) -> Result<(), ErrorCode> {
        set_unicast_hop_limit(&self.socket, self.family, value)
    }

    pub fn receive_buffer_size(&self) -> Result<u64, ErrorCode> {
        receive_buffer_size(&self.socket)
    }

    pub fn set_receive_buffer_size(&self, value: u64) -> Result<(), ErrorCode> {
        set_receive_buffer_size(&self.socket, value)?;
        Ok(())
    }

    pub fn send_buffer_size(&self) -> Result<u64, ErrorCode> {
        send_buffer_size(&self.socket)
    }

    pub fn set_send_buffer_size(&self, value: u64) -> Result<(), ErrorCode> {
        set_send_buffer_size(&self.socket, value)?;
        Ok(())
    }
}

fn bind(sockfd: impl AsFd, addr: SocketAddr) -> Result<(), ErrorCode> {
    rustix::net::bind(sockfd, &addr).map_err(|err| match err {
        // See: https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-bind#:~:text=WSAENOBUFS
        // Windows returns WSAENOBUFS when the ephemeral ports have been exhausted.
        #[cfg(windows)]
        Errno::NOBUFS => ErrorCode::AddressInUse,
        Errno::AFNOSUPPORT => ErrorCode::InvalidArgument,
        _ => err.into(),
    })
}

fn disconnect(sockfd: impl AsFd) -> Result<(), ErrorCode> {
    match connect_unspec(sockfd) {
        // BSD platforms return an error even if the UDP socket was disconnected successfully.
        //
        // MacOS was kind enough to document this: https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/connect.2.html
        // > Datagram sockets may dissolve the association by connecting to an
        // > invalid address, such as a null address or an address with the address
        // > family set to AF_UNSPEC (the error EAFNOSUPPORT will be harmlessly
        // > returned).
        //
        // ... except that this appears to be incomplete, because experiments
        // have shown that MacOS actually returns EINVAL, depending on the
        // address family of the socket.
        #[cfg(target_os = "macos")]
        Err(Errno::INVAL | Errno::AFNOSUPPORT) => Ok(()),
        Err(err) => Err(err.into()),
        Ok(()) => Ok(()),
    }
}

async fn send(socket: &tokio::net::UdpSocket, mut buf: &[u8]) -> Result<(), ErrorCode> {
    loop {
        let n = socket.send(buf).await?;
        if n == buf.len() {
            return Ok(());
        }
        buf = &buf[n..]
    }
}

async fn send_to(
    socket: &tokio::net::UdpSocket,
    mut buf: &[u8],
    addr: SocketAddr,
) -> Result<(), ErrorCode> {
    loop {
        let n = socket.send_to(buf, addr).await?;
        if n == buf.len() {
            return Ok(());
        }
        buf = &buf[n..]
    }
}
