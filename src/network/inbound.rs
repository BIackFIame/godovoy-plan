//! One owner per bound port. Workers borrow sockets only for nonblocking operations,
//! so pause can release the OS handles before returning, even during an idle poll.
use super::*;
use mio::{Events, Interest, Poll, Registry, Token, Waker, event::Source};
use std::sync::{Condvar, OnceLock};

struct Signal {
    registry: Registry,
    waker: Waker,
}
impl Signal {
    fn new() -> io::Result<(Self, Poll)> {
        let poll = Poll::new()?;
        let registry = poll.registry().try_clone()?;
        let waker = Waker::new(&registry, Token(1))?;
        Ok((Self { registry, waker }, poll))
    }
}

#[derive(Default)]
pub(super) struct Ports {
    tcp: Option<mio::net::TcpListener>,
    udp: Option<mio::net::UdpSocket>,
    tcp_ready: bool,
    udp_ready: bool,
    retry: Option<Instant>,
    warning: Option<String>,
}

pub(super) struct Inbound {
    config: Config,
    pub(super) ports: Mutex<Ports>,
    resumed: Condvar,
    tcp_signal: Option<Signal>,
    udp_signal: Option<Signal>,
    pub(super) sync_thread: OnceLock<thread::Thread>,
}

pub(super) struct Wait {
    poll: Option<Poll>,
    events: Events,
}
impl Wait {
    fn new(enabled: bool) -> (Option<Signal>, Self) {
        let (signal, poll) =
            if let Some((signal, poll)) = enabled.then(Signal::new).and_then(Result::ok) {
                (Some(signal), Some(poll))
            } else {
                (None, None)
            };
        (
            signal,
            Self {
                poll,
                events: Events::with_capacity(4),
            },
        )
    }
    pub(super) fn until(&mut self, registered: bool, timeout: Duration) {
        if registered && let Some(poll) = &mut self.poll {
            if let Err(error) = poll.poll(&mut self.events, Some(timeout))
                && error.kind() != io::ErrorKind::Interrupted
            {
                self.poll = None;
            }
        } else {
            thread::park_timeout(timeout.min(Duration::from_millis(40)));
        }
    }
}
impl Inbound {
    pub(super) fn new(config: Config) -> (Self, Wait, Wait) {
        let (tcp_signal, tcp) = Wait::new(true);
        let (udp_signal, udp) = Wait::new(config.discovery_port.is_some());
        (
            Self {
                config,
                ports: Default::default(),
                resumed: Condvar::new(),
                tcp_signal,
                udp_signal,
                sync_thread: OnceLock::new(),
            },
            tcp,
            udp,
        )
    }
    fn register(signal: &Option<Signal>, socket: &mut impl Source) -> bool {
        signal.as_ref().is_some_and(|s| {
            s.registry
                .register(socket, Token(0), Interest::READABLE)
                .is_ok()
        })
    }
    pub(super) fn wait_for_resume(&self, core: &Core) {
        let ports = self.ports.lock().unwrap();
        drop(
            self.resumed
                .wait_while(ports, |_| {
                    core.paused.load(Ordering::Acquire) && !core.stopped.load(Ordering::Acquire)
                })
                .unwrap(),
        );
    }

    pub(super) fn wake(&self) {
        self.resumed.notify_all();
        for s in [&self.tcp_signal, &self.udp_signal].into_iter().flatten() {
            let _ = s.waker.wake();
        }
        if let Some(worker) = self.sync_thread.get() {
            worker.unpark();
        }
    }
    pub(super) fn bind(&self, ports: &mut Ports, core: &Core) -> Fallible<()> {
        ports.retry = None;
        ports.warning = None;
        if ports.tcp.is_none()
            && let Err(error) = self.bind_tcp(ports, core)
        {
            ports.warning = Some(format!(
                "Обмен приостановлен. Не удалось открыть сетевой порт. {error}"
            ));
            ports.retry = Some(Instant::now() + Duration::from_secs(3));
            return Err(error);
        }
        if ports.udp.is_none()
            && let Some(port) = self.config.discovery_port
        {
            match discovery_socket(*self.config.listen.ip(), port) {
                Ok(socket) => {
                    let mut socket = mio::net::UdpSocket::from_std(socket);
                    ports.udp_ready = Self::register(&self.udp_signal, &mut socket);
                    ports.udp = Some(socket);
                }
                Err(e) => {
                    ports.warning = Some(format!(
                        "Автопоиск недоступен. Можно подключаться по адресу. {e}"
                    ));
                    ports.retry = Some(Instant::now() + Duration::from_secs(3));
                }
            }
        }
        Ok(())
    }
    fn bind_tcp(&self, ports: &mut Ports, core: &Core) -> Fallible<()> {
        let configured = self.config.listen;
        let previous = core.port.load(Ordering::Acquire);
        let listener = if configured.port() == 0 && previous != 0 {
            TcpListener::bind(SocketAddrV4::new(*configured.ip(), previous))
                .or_else(|_| TcpListener::bind(configured))?
        } else {
            TcpListener::bind(configured)?
        };
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let mut listener = mio::net::TcpListener::from_std(listener);
        ports.tcp_ready = Self::register(&self.tcp_signal, &mut listener);
        ports.tcp = Some(listener);
        core.port.store(port, Ordering::Release);
        Ok(())
    }
    pub(super) fn warning(&self) -> Option<String> {
        self.ports.lock().unwrap().warning.clone()
    }
    #[cfg(test)]
    pub(super) fn discovery_bound(&self) -> bool {
        self.ports.lock().unwrap().udp.is_some()
    }
    pub(super) fn retry_at(&self) -> Option<Instant> {
        self.ports.lock().unwrap().retry
    }
    pub(super) fn retry(&self, core: &Core) {
        let mut ports = self.ports.lock().unwrap();
        if core.stopped.load(Ordering::Acquire)
            || ports.retry.is_none_or(|due| due > Instant::now())
        {
            return;
        }
        if self.bind(&mut ports, core).is_ok() {
            if core.paused.swap(false, Ordering::AcqRel) {
                core.generation.fetch_add(1, Ordering::AcqRel);
            }
            self.wake();
        }
    }
    pub(super) fn close(&self, ports: &mut Ports) {
        if let (Some(signal), Some(socket)) = (&self.tcp_signal, ports.tcp.as_mut()) {
            let _ = signal.registry.deregister(socket);
        }
        if let (Some(signal), Some(socket)) = (&self.udp_signal, ports.udp.as_mut()) {
            let _ = signal.registry.deregister(socket);
        }
        *ports = Ports::default();
    }
    pub(super) fn accept(&self) -> (bool, io::Result<(mio::net::TcpStream, SocketAddr)>) {
        let ports = self.ports.lock().unwrap();
        (
            ports.tcp_ready || ports.tcp.is_none(),
            ports
                .tcp
                .as_ref()
                .map_or_else(|| Err(io::ErrorKind::WouldBlock.into()), |s| s.accept()),
        )
    }
    pub(super) fn receive(&self, buf: &mut [u8]) -> (bool, io::Result<(usize, SocketAddr)>) {
        let ports = self.ports.lock().unwrap();
        (
            ports.udp_ready || ports.udp.is_none(),
            ports.udp.as_ref().map_or_else(
                || Err(io::ErrorKind::WouldBlock.into()),
                |s| s.recv_from(buf),
            ),
        )
    }
    pub(super) fn announce(&self, beacon: &[u8], port: u16) {
        let ports = self.ports.lock().unwrap();
        if let Some(socket) = &ports.udp {
            let local = *self.config.listen.ip();
            let destination = if local.is_loopback() {
                local
            } else {
                Ipv4Addr::BROADCAST
            };
            let _ = socket.send_to(beacon, SocketAddrV4::new(destination, port).into());
        }
    }
}
