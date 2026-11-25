use crate::preferences::{save_settings_file, settings_directory};
use crate::serial_io::{Command, Connection, Event, Packet, ReceiveRecords, local_time};
use crate::upgrade_protocol;
use serde::{Deserialize, Serialize};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::windows::io::AsRawSocket;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::time::{Duration, Instant};
use windows_sys::Win32::Networking::WinSock::{
    POLLNVAL, POLLRDNORM, POLLWRNORM, SOCKET_ERROR, WSAGetLastError, WSAPOLLFD, WSAPoll,
};

const UDP_BATCH_PACKETS: usize = 128;
const UDP_BATCH_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub enum Mode {
    #[default]
    Serial,
    TcpClient,
    TcpServer,
    Udp,
}

impl Mode {
    pub const ALL: [Self; 4] = [Self::Serial, Self::TcpClient, Self::TcpServer, Self::Udp];
    pub const LABELS: [&str; 4] = ["串口", "TCP 客户端", "TCP 服务端", "UDP"];

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|&mode| mode == self).unwrap_or(0)
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Preferences {
    pub mode: Mode,
    pub remote_host: String,
    pub remote_port: String,
    pub local_host: String,
    pub local_port: String,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            mode: Mode::Serial,
            remote_host: "127.0.0.1".into(),
            remote_port: "9000".into(),
            local_host: "0.0.0.0".into(),
            local_port: "9001".into(),
        }
    }
}

impl Preferences {
    pub fn load() -> Self {
        settings_directory()
            .and_then(|dir| std::fs::read(dir.join("network.json")).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> io::Result<()> {
        let path = settings_directory()
            .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "LOCALAPPDATA"))?
            .join("network.json");
        save_settings_file(&path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn settings(&self) -> io::Result<Settings> {
        fn address(host: &str, port: &str, local: bool) -> io::Result<SocketAddr> {
            let name = if local { "本地" } else { "远端" };
            let ip: IpAddr = host
                .trim()
                .parse()
                .map_err(|_| io::Error::other(format!("请输入有效的{name} IPv4 或 IPv6 地址")))?;
            if !local && ip.is_unspecified() {
                return Err(io::Error::other(
                    "远端 IP 需要填写设备实际地址，不能使用 0.0.0.0 或 ::",
                ));
            }
            let port: u16 = port.trim().parse().map_err(|_| {
                io::Error::other(format!(
                    "{name}端口应为 {} 到 65535 的整数",
                    if local { 0 } else { 1 }
                ))
            })?;
            if !local && port == 0 {
                return Err(io::Error::other("远端端口不能为 0"));
            }
            Ok(SocketAddr::new(ip, port))
        }
        match self.mode {
            Mode::Serial => Err(io::Error::other("请选择网络连接类型")),
            Mode::TcpClient => Ok(Settings::TcpClient(address(
                &self.remote_host,
                &self.remote_port,
                false,
            )?)),
            Mode::TcpServer => Ok(Settings::TcpServer(address(
                &self.local_host,
                &self.local_port,
                true,
            )?)),
            Mode::Udp => {
                let local = address(&self.local_host, &self.local_port, true)?;
                let remote = address(&self.remote_host, &self.remote_port, false)?;
                if local.is_ipv4() != remote.is_ipv4() {
                    return Err(io::Error::other(
                        "UDP 本地和远端地址需要同时使用 IPv4 或同时使用 IPv6",
                    ));
                }
                Ok(Settings::Udp { local, remote })
            }
        }
    }
}

pub enum Settings {
    TcpClient(SocketAddr),
    TcpServer(SocketAddr),
    Udp {
        local: SocketAddr,
        remote: SocketAddr,
    },
}

impl Settings {
    pub fn description(&self) -> String {
        match self {
            Self::TcpClient(remote) => format!("TCP 客户端 → {remote}"),
            Self::TcpServer(local) => format!("TCP 服务端 {local}"),
            Self::Udp { local, remote } => format!("UDP {local} → {remote}"),
        }
    }
}

pub fn connect(settings: Settings) -> io::Result<Connection> {
    let (commands, requests) = mpsc::sync_channel(1);
    let (events, receiver) = mpsc::sync_channel(32);
    std::thread::Builder::new()
        .name("network-io".into())
        .spawn(move || {
            let result = match settings {
                Settings::TcpClient(remote) => client(remote, &requests, &events),
                Settings::TcpServer(local) => server(local, &requests, &events),
                Settings::Udp { local, remote } => udp(local, remote, &requests, &events),
            };
            let _ = events.send(Event::Closed(
                result.err().map(|error| format!("网络连接结束 · {error}")),
            ));
        })?;
    Ok(Connection {
        commands: Some(commands),
        events: receiver,
    })
}

fn emit(events: &SyncSender<Event>, event: Event) -> io::Result<()> {
    events
        .send(event)
        .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "网络会话已关闭"))
}

fn state(
    events: &SyncSender<Event>,
    ready: bool,
    description: String,
    reset: bool,
) -> io::Result<()> {
    emit(
        events,
        Event::NetworkState {
            ready,
            description,
            reset,
        },
    )
}

fn resolve(command: Command, events: &SyncSender<Event>) -> io::Result<Option<Vec<u8>>> {
    match command {
        Command::Send(bytes) => Ok(Some(bytes)),
        Command::SendTime { format, ending } => Ok(Some(format.payload(local_time(), ending))),
        Command::Upgrade(_) => {
            emit(
                events,
                Event::Upgrade(upgrade_protocol::Event::Done(Err(
                    "N6 升级仅支持串口连接".into()
                ))),
            )?;
            Ok(None)
        }
    }
}

fn sent(
    events: &SyncSender<Event>,
    mut bytes: Vec<u8>,
    written: usize,
    peer: SocketAddr,
    error: Option<String>,
) -> io::Result<()> {
    let requested = bytes.len();
    bytes.truncate(written);
    emit(
        events,
        Event::Sent {
            packet: Packet {
                time: local_time(),
                bytes,
                peer: Some(peer),
            },
            requested,
            error,
        },
    )
}

fn received(
    events: &SyncSender<Event>,
    mut packets: Vec<Packet>,
    peer: SocketAddr,
) -> io::Result<()> {
    if !packets.is_empty() {
        for packet in &mut packets {
            packet.peer = Some(peer);
        }
        emit(events, Event::Received(packets))?;
    }
    Ok(())
}

struct Outgoing {
    bytes: Vec<u8>,
    written: usize,
    progress: Instant,
}

impl Outgoing {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            written: 0,
            progress: Instant::now(),
        }
    }
}

struct Peer {
    stream: TcpStream,
    address: SocketAddr,
    incoming: ReceiveRecords,
    outgoing: Option<Outgoing>,
}

impl Peer {
    fn new(stream: TcpStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        stream.set_nodelay(true)?;
        Ok(Self {
            address: stream.peer_addr()?,
            stream,
            incoming: ReceiveRecords::default(),
            outgoing: None,
        })
    }

    fn send(&mut self, bytes: Vec<u8>, events: &SyncSender<Event>) -> io::Result<()> {
        if self.outgoing.is_some() {
            sent(
                events,
                bytes,
                0,
                self.address,
                Some("上一条数据仍在发送".into()),
            )
        } else {
            self.outgoing = Some(Outgoing::new(bytes));
            Ok(())
        }
    }

    fn tick(&mut self, events: &SyncSender<Event>) -> io::Result<()> {
        if let Some(outgoing) = &mut self.outgoing {
            let end = (outgoing.written + 8192).min(outgoing.bytes.len());
            match self.stream.write(&outgoing.bytes[outgoing.written..end]) {
                Ok(0) => return Err(io::Error::new(ErrorKind::WriteZero, "TCP 写入中断")),
                Ok(count) => {
                    outgoing.written += count;
                    outgoing.progress = Instant::now();
                }
                Err(error) if retryable(&error) => (),
                Err(error) => return Err(error),
            }
            if outgoing.written == outgoing.bytes.len() {
                let outgoing = self.outgoing.take().unwrap();
                sent(events, outgoing.bytes, outgoing.written, self.address, None)?;
            } else if outgoing.progress.elapsed() >= Duration::from_secs(5) {
                return Err(io::Error::new(ErrorKind::TimedOut, "TCP 发送超时"));
            }
        }
        let mut buffer = [0; 8192];
        let packets = match self.stream.read(&mut buffer) {
            Ok(0) => {
                return Err(io::Error::new(
                    ErrorKind::ConnectionAborted,
                    "对端已断开连接",
                ));
            }
            Ok(count) => self
                .incoming
                .push(&buffer[..count], Instant::now(), local_time()),
            Err(error) if retryable(&error) => self
                .incoming
                .take_if_idle(Instant::now())
                .into_iter()
                .collect(),
            Err(error) => return Err(error),
        };
        received(events, packets, self.address)
    }

    fn finish(&mut self, events: &SyncSender<Event>, reason: &str) {
        let _ = received(events, self.incoming.finish(), self.address);
        if let Some(outgoing) = self.outgoing.take() {
            let _ = sent(
                events,
                outgoing.bytes,
                outgoing.written,
                self.address,
                Some(reason.into()),
            );
        }
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

fn retryable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::WouldBlock | ErrorKind::Interrupted | ErrorKind::TimedOut
    )
}

fn client(
    remote: SocketAddr,
    commands: &Receiver<Command>,
    events: &SyncSender<Event>,
) -> io::Result<()> {
    let stream = TcpStream::connect_timeout(&remote, Duration::from_secs(3))?;
    let local = stream.local_addr()?;
    let mut peer = Peer::new(stream)?;
    state(
        events,
        true,
        format!("TCP 客户端 {local} ↔ {remote} · 已连接"),
        true,
    )?;
    let result = loop {
        match commands.try_recv() {
            Ok(command) => {
                if let Some(bytes) = resolve(command, events)? {
                    peer.send(bytes, events)?;
                }
            }
            Err(TryRecvError::Disconnected) => break Ok(()),
            Err(TryRecvError::Empty) => (),
        }
        if let Err(error) = peer.tick(events) {
            break Err(error);
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    peer.finish(events, "连接已关闭");
    result
}

fn server(
    local: SocketAddr,
    commands: &Receiver<Command>,
    events: &SyncSender<Event>,
) -> io::Result<()> {
    let listener = TcpListener::bind(local)?;
    listener.set_nonblocking(true)?;
    let local = listener.local_addr()?;
    state(
        events,
        false,
        format!("TCP 服务端 {local} · 正在监听，等待客户端"),
        true,
    )?;
    let mut peer: Option<Peer> = None;
    let result = loop {
        match commands.try_recv() {
            Ok(command) => {
                if let Some(bytes) = resolve(command, events)? {
                    if let Some(peer) = &mut peer {
                        peer.send(bytes, events)?;
                    } else {
                        sent(events, bytes, 0, local, Some("尚无客户端连接".into()))?;
                    }
                }
            }
            Err(TryRecvError::Disconnected) => break Ok(()),
            Err(TryRecvError::Empty) => (),
        }
        match listener.accept() {
            Ok((stream, address)) => {
                if peer.is_none() {
                    peer = Some(Peer::new(stream)?);
                    state(
                        events,
                        true,
                        format!("TCP 服务端 {local} ↔ {address} · 已连接"),
                        false,
                    )?;
                } else {
                    let _ = stream.shutdown(Shutdown::Both);
                }
            }
            Err(error) if retryable(&error) => (),
            Err(error) => break Err(error),
        }
        if let Some(active) = &mut peer
            && let Err(error) = active.tick(events)
        {
            active.finish(events, &error.to_string());
            peer = None;
            state(
                events,
                false,
                format!("TCP 服务端 {local} · {error}，继续等待客户端"),
                false,
            )?;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    if let Some(peer) = &mut peer {
        peer.finish(events, "监听已关闭");
    }
    result
}

fn wait_udp(socket: &UdpSocket, writing: bool) -> io::Result<()> {
    let mut descriptor = WSAPOLLFD {
        fd: socket.as_raw_socket() as usize,
        events: POLLRDNORM | if writing { POLLWRNORM } else { 0 },
        revents: 0,
    };
    // A readable socket wakes immediately. The timeout only bounds how long an idle
    // worker waits before checking new send requests or a closed command channel.
    if unsafe { WSAPoll(&mut descriptor, 1, 10) } == SOCKET_ERROR {
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    if descriptor.revents & POLLNVAL != 0 {
        return Err(io::Error::new(ErrorKind::NotConnected, "UDP 套接字已失效"));
    }
    Ok(())
}

fn udp(
    local: SocketAddr,
    remote: SocketAddr,
    commands: &Receiver<Command>,
    events: &SyncSender<Event>,
) -> io::Result<()> {
    let socket = UdpSocket::bind(local)?;
    socket.set_nonblocking(true)?;
    if local.is_ipv4() {
        socket.set_broadcast(true)?;
    }
    let local = socket.local_addr()?;
    state(
        events,
        true,
        format!("UDP {local} → {remote} · 已打开"),
        true,
    )?;
    let mut outgoing: Option<Outgoing> = None;
    let mut buffer = vec![0; 65536];
    let result = loop {
        match commands.try_recv() {
            Ok(command) => {
                if let Some(bytes) = resolve(command, events)? {
                    if bytes.len() > 65507 {
                        sent(
                            events,
                            bytes,
                            0,
                            remote,
                            Some("UDP 单个报文最多 65507 字节".into()),
                        )?;
                    } else if outgoing.is_some() {
                        sent(events, bytes, 0, remote, Some("上一条报文仍在发送".into()))?;
                    } else {
                        outgoing = Some(Outgoing::new(bytes));
                    }
                }
            }
            Err(TryRecvError::Disconnected) => break Ok(()),
            Err(TryRecvError::Empty) => (),
        }
        if let Some(pending) = &outgoing {
            let outcome = match socket.send_to(&pending.bytes, remote) {
                Ok(count) => Some((
                    count,
                    (count != pending.bytes.len()).then(|| "UDP 报文未完整写入".into()),
                )),
                Err(error)
                    if retryable(&error) && pending.progress.elapsed() < Duration::from_secs(5) =>
                {
                    None
                }
                Err(error) => Some((0, Some(error.to_string()))),
            };
            if let Some((count, error)) = outcome {
                sent(events, outgoing.take().unwrap().bytes, count, remote, error)?;
            }
        }
        let mut packets = Vec::with_capacity(UDP_BATCH_PACKETS);
        let mut bytes = 0;
        let mut drained = false;
        let mut receive_error = None;
        // Drain queued datagrams without a per-packet delay. Batch limits give
        // sending and closing a turn, and bound each UI event's memory usage.
        for _ in 0..UDP_BATCH_PACKETS {
            match socket.recv_from(&mut buffer) {
                Ok((count, peer)) => {
                    packets.push(Packet {
                        time: local_time(),
                        bytes: buffer[..count].to_vec(),
                        peer: Some(peer),
                    });
                    bytes += count;
                    if bytes >= UDP_BATCH_BYTES {
                        break;
                    }
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    drained = true;
                    break;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::Interrupted
                            | ErrorKind::ConnectionReset
                            | ErrorKind::ConnectionRefused
                    ) =>
                {
                    ()
                }
                Err(error) => {
                    receive_error = Some(error);
                    break;
                }
            }
        }
        if !packets.is_empty() {
            emit(events, Event::Received(packets))?;
        }
        if let Some(error) = receive_error {
            break Err(error);
        }
        if drained {
            wait_udp(&socket, outgoing.is_some())?;
        }
    };
    if let Some(outgoing) = outgoing {
        let _ = sent(events, outgoing.bytes, 0, remote, Some("UDP 已关闭".into()));
    }
    result
}
