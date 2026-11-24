use crate::presets::{Ending, TimeFormat};
use crate::upgrade_protocol::{self, Protocol, Request};
use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits};
use std::io::{self, ErrorKind, Read, Write};
use std::net::SocketAddr;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

const READ_TIMEOUT: Duration = Duration::from_millis(5);
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);
const RECEIVE_IDLE: Duration = Duration::from_millis(20);
const MAX_RECEIVE_RECORD: usize = 8192;

pub struct Settings {
    pub port: String,
    pub baud: u32,
    pub data_bits: DataBits,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub flow: FlowControl,
}

pub enum Event {
    Opened,
    NetworkState {
        ready: bool,
        description: String,
        reset: bool,
    },
    Received(Vec<Packet>),
    Sent {
        packet: Packet,
        requested: usize,
        error: Option<String>,
    },
    Closed(Option<String>),
    Upgrade(upgrade_protocol::Event),
}

pub enum Command {
    Send(Vec<u8>),
    SendTime { format: TimeFormat, ending: Ending },
    Upgrade(Box<Request>),
}

pub struct Packet {
    pub time: SYSTEMTIME,
    pub bytes: Vec<u8>,
    pub peer: Option<SocketAddr>,
}

#[derive(Default)]
pub(crate) struct ReceiveRecords {
    pending: Option<Packet>,
    last_read: Option<Instant>,
}

impl ReceiveRecords {
    pub(crate) fn take_if_idle(&mut self, now: Instant) -> Option<Packet> {
        if self
            .last_read
            .is_some_and(|last| now.duration_since(last) >= RECEIVE_IDLE)
        {
            self.pending.take()
        } else {
            None
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8], now: Instant, time: SYSTEMTIME) -> Vec<Packet> {
        let mut records = Vec::new();
        records.extend(self.take_if_idle(now));
        for &byte in bytes {
            // Keep a trailing CR until we know whether an LF follows it.
            if byte != b'\n'
                && self
                    .pending
                    .as_ref()
                    .is_some_and(|record| record.bytes.ends_with(b"\r"))
            {
                records.extend(self.pending.take());
            }
            let record = self.pending.get_or_insert_with(|| Packet {
                time,
                bytes: Vec::new(),
                peer: None,
            });
            record.bytes.push(byte);
            if byte == b'\n' || (record.bytes.len() >= MAX_RECEIVE_RECORD && byte != b'\r') {
                records.extend(self.pending.take());
            }
        }
        self.last_read = Some(now);
        records
    }

    pub(crate) fn finish(&mut self) -> Vec<Packet> {
        self.pending.take().into_iter().collect()
    }
}

pub fn local_time() -> SYSTEMTIME {
    let mut time = SYSTEMTIME::default();
    unsafe { GetLocalTime(&mut time) };
    time
}

pub struct Connection {
    pub commands: Option<SyncSender<Command>>,
    pub events: Receiver<Event>,
}

pub fn connect(settings: Settings) -> io::Result<Connection> {
    let (commands, command_rx) = mpsc::sync_channel(1);
    let (event_tx, events) = mpsc::sync_channel(32);
    std::thread::Builder::new()
        .name("serial-io".into())
        .spawn(move || {
            let result = serialport::new(&settings.port, settings.baud)
                .data_bits(settings.data_bits)
                .parity(settings.parity)
                .stop_bits(settings.stop_bits)
                .flow_control(settings.flow)
                .timeout(READ_TIMEOUT)
                .open();
            let mut port = match result {
                Ok(port) => port,
                Err(error) => {
                    let _ = event_tx.send(Event::Closed(Some(format!("打开失败，{error}"))));
                    return;
                }
            };
            if event_tx.send(Event::Opened).is_err() {
                return;
            }

            let reason = exchange(&mut *port, &settings, command_rx, &event_tx);
            drop(port);
            let _ = event_tx.send(Event::Closed(reason));
        })?;
    Ok(Connection {
        commands: Some(commands),
        events,
    })
}

fn exchange(
    port: &mut dyn SerialPort,
    settings: &Settings,
    commands: Receiver<Command>,
    events: &SyncSender<Event>,
) -> Option<String> {
    let mut input = [0; 8192];
    let mut incoming = ReceiveRecords::default();
    let mut outgoing: Option<(Vec<u8>, usize)> = None;
    // Keep each synchronous driver write short so reads and closing get a turn.
    let write_chunk = (settings.baud as usize / 120).clamp(1, 8192);
    let closed = loop {
        match commands.try_recv() {
            Ok(Command::Send(data)) => outgoing = Some((data, 0)),
            Ok(Command::SendTime { format, ending }) => {
                // Resolve the clock in the serial worker, immediately before its first write.
                outgoing = Some((format.payload(local_time(), ending), 0));
            }
            Ok(Command::Upgrade(request)) => {
                if outgoing.is_some() {
                    let _ = events.send(Event::Upgrade(upgrade_protocol::Event::Done(Err(
                        "正在发送，请稍后开始升级".into(),
                    ))));
                    continue;
                }
                if let Some(packet) = incoming.pending.take()
                    && events.send(Event::Received(vec![packet])).is_err()
                {
                    return None;
                }
                if let Err(error) = run_upgrade(port, settings, *request, events) {
                    break Some(format!("恢复串口设置失败 · {error}"));
                }
                incoming = ReceiveRecords::default();
                continue;
            }
            Err(TryRecvError::Empty) => (),
            Err(TryRecvError::Disconnected) => break None,
        }

        if let Some((data, written)) = &mut outgoing {
            let end = (*written + write_chunk).min(data.len());
            if let Err(error) = port.set_timeout(WRITE_TIMEOUT) {
                break Some(format!("串口设置失败，{error}"));
            }
            let (count, error) = write_chunk_once(port, &data[*written..end]);
            *written += count;
            if *written == data.len() || error.is_some() {
                let (data, written) = outgoing.take().unwrap();
                if report_send(events, data, written, error).is_err() {
                    return None;
                }
            }
            if let Err(restore_error) = port.set_timeout(READ_TIMEOUT) {
                break Some(format!("串口连接中断，{restore_error}"));
            }
        }

        let records = match port.read(&mut input) {
            Ok(0) => incoming.take_if_idle(Instant::now()).into_iter().collect(),
            Ok(size) => incoming.push(&input[..size], Instant::now(), local_time()),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::TimedOut | ErrorKind::Interrupted | ErrorKind::WouldBlock
                ) =>
            {
                incoming.take_if_idle(Instant::now()).into_iter().collect()
            }
            Err(error) => break Some(format!("串口连接中断，{error}")),
        };
        if !records.is_empty() && events.send(Event::Received(records)).is_err() {
            return None;
        }
    };
    if let Some(record) = incoming.pending.take() {
        let _ = events.send(Event::Received(vec![record]));
    }
    if let Some((data, written)) = outgoing {
        let _ = report_send(events, data, written, Some("发送中断".into()));
    }
    closed
}

fn report_send(
    events: &SyncSender<Event>,
    mut bytes: Vec<u8>,
    written: usize,
    error: Option<String>,
) -> Result<(), mpsc::SendError<Event>> {
    let requested = bytes.len();
    bytes.truncate(written);
    events.send(Event::Sent {
        packet: Packet {
            time: local_time(),
            bytes,
            peer: None,
        },
        requested,
        error,
    })
}

fn run_upgrade(
    port: &mut dyn SerialPort,
    settings: &Settings,
    request: Request,
    events: &SyncSender<Event>,
) -> io::Result<()> {
    let mut received = 0;
    let mut sent = 0;
    let result = (|| {
        port.set_baud_rate(115200)?;
        port.set_data_bits(DataBits::Eight)?;
        port.set_parity(Parity::None)?;
        port.set_stop_bits(StopBits::One)?;
        port.set_flow_control(FlowControl::None)?;
        port.set_timeout(Duration::from_millis(100))?;
        let callback = |event| {
            let _ = events.send(Event::Upgrade(event));
        };
        let io = UpgradePort {
            port,
            received: &mut received,
            sent: &mut sent,
        };
        Protocol::new(io, &callback, &request.cancel).run(
            request.secure,
            request.nonsecure,
            request.manual,
        )
    })();
    // Always restore the existing connection, including after cancellation or a protocol error.
    let restored = (|| {
        port.set_baud_rate(settings.baud)?;
        port.set_data_bits(settings.data_bits)?;
        port.set_parity(settings.parity)?;
        port.set_stop_bits(settings.stop_bits)?;
        port.set_flow_control(settings.flow)?;
        port.set_timeout(READ_TIMEOUT)?;
        Ok::<_, io::Error>(())
    })();
    let _ = events.send(Event::Upgrade(upgrade_protocol::Event::TransferCount {
        received,
        sent,
    }));
    let outcome = match &restored {
        Ok(()) => result.map_err(|error| error.to_string()),
        Err(error) => Err(format!("升级已结束，恢复串口失败 · {error}")),
    };
    let _ = events.send(Event::Upgrade(upgrade_protocol::Event::Done(outcome)));
    restored
}

struct UpgradePort<'a> {
    port: &'a mut dyn SerialPort,
    received: &'a mut u64,
    sent: &'a mut u64,
}

impl Read for UpgradePort<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.port.read(buffer)?;
        *self.received += count as u64;
        Ok(count)
    }
}

impl Write for UpgradePort<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.port.set_timeout(Duration::from_secs(2))?;
        let result = self.port.write(bytes);
        if let Ok(count) = result {
            *self.sent += count as u64;
        }
        self.port.set_timeout(Duration::from_millis(100))?;
        result
    }
    fn flush(&mut self) -> io::Result<()> {
        self.port.flush()
    }
}

fn write_chunk_once(port: &mut dyn SerialPort, bytes: &[u8]) -> (usize, Option<String>) {
    match port.write(bytes) {
        Ok(0) => (0, Some("发送超时或驱动未确认写入。".into())),
        Ok(count) => (count, None),
        Err(error) => (0, Some(format!("发送失败，{error}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receive_records_follow_line_endings_and_idle_gaps() {
        let mut records = ReceiveRecords::default();
        let start = Instant::now();
        let first = SYSTEMTIME {
            wMilliseconds: 123,
            ..Default::default()
        };
        let later = SYSTEMTIME {
            wMilliseconds: 456,
            ..first
        };
        assert!(records.push(b"v", start, first).is_empty());
        assert!(
            records
                .push(b"oltage[0]:8.400v", start + Duration::from_millis(4), later)
                .is_empty()
        );
        assert!(
            records
                .take_if_idle(start + Duration::from_millis(23))
                .is_none()
        );
        let line = records
            .take_if_idle(start + Duration::from_millis(24))
            .unwrap();
        assert_eq!(line.bytes, b"voltage[0]:8.400v");
        assert_eq!(line.time.wMilliseconds, 123);
        assert!(
            records
                .take_if_idle(start + Duration::from_millis(25))
                .is_none()
        );

        let next = start + Duration::from_secs(1);
        assert!(records.push(b"voltage[1]:8.395v\r", next, first).is_empty());
        let lines = records.push(b"\na\nb\rc", next + Duration::from_millis(2), later);
        let bytes: Vec<_> = lines.iter().map(|line| line.bytes.as_slice()).collect();
        assert_eq!(bytes, [b"voltage[1]:8.395v\r\n".as_slice(), b"a\n", b"b\r"]);
        let lines = records.push(
            b"d\n",
            next + RECEIVE_IDLE + Duration::from_millis(2),
            later,
        );
        let bytes: Vec<_> = lines.iter().map(|line| line.bytes.as_slice()).collect();
        assert_eq!(bytes, [b"c".as_slice(), b"d\n"]);
    }

    #[test]
    fn send_report_contains_only_confirmed_bytes() {
        let (events, receiver) = mpsc::sync_channel(1);
        report_send(&events, vec![0xAA, 0xBB, 0xCC], 1, Some("超时".into())).unwrap();
        let Event::Sent {
            packet,
            requested,
            error,
        } = receiver.recv().unwrap()
        else {
            panic!("expected send result");
        };
        assert_eq!(packet.bytes, [0xAA]);
        assert_eq!(requested, 3);
        assert_eq!(error.as_deref(), Some("超时"));
    }
}

#[cfg(test)]
mod shared_upgrade_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, atomic::AtomicBool};

    struct ExistingPort {
        settings: Settings,
        timeout: Duration,
        input: VecDeque<u8>,
        writes: Vec<Vec<u8>>,
        fail_baud: u32,
    }

    impl Read for ExistingPort {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            bytes[0] = self
                .input
                .pop_front()
                .ok_or_else(|| io::Error::other("response exhausted"))?;
            Ok(1)
        }
    }
    impl Write for ExistingPort {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes.push(bytes.to_vec());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl SerialPort for ExistingPort {
        fn name(&self) -> Option<String> {
            Some(self.settings.port.clone())
        }
        fn baud_rate(&self) -> serialport::Result<u32> {
            Ok(self.settings.baud)
        }
        fn data_bits(&self) -> serialport::Result<DataBits> {
            Ok(self.settings.data_bits)
        }
        fn flow_control(&self) -> serialport::Result<FlowControl> {
            Ok(self.settings.flow)
        }
        fn parity(&self) -> serialport::Result<Parity> {
            Ok(self.settings.parity)
        }
        fn stop_bits(&self) -> serialport::Result<StopBits> {
            Ok(self.settings.stop_bits)
        }
        fn timeout(&self) -> Duration {
            self.timeout
        }
        fn set_baud_rate(&mut self, value: u32) -> serialport::Result<()> {
            if value == self.fail_baud {
                return Err(serialport::Error::new(
                    serialport::ErrorKind::NoDevice,
                    "device disconnected",
                ));
            }
            self.settings.baud = value;
            Ok(())
        }
        fn set_data_bits(&mut self, value: DataBits) -> serialport::Result<()> {
            self.settings.data_bits = value;
            Ok(())
        }
        fn set_flow_control(&mut self, value: FlowControl) -> serialport::Result<()> {
            self.settings.flow = value;
            Ok(())
        }
        fn set_parity(&mut self, value: Parity) -> serialport::Result<()> {
            self.settings.parity = value;
            Ok(())
        }
        fn set_stop_bits(&mut self, value: StopBits) -> serialport::Result<()> {
            self.settings.stop_bits = value;
            Ok(())
        }
        fn set_timeout(&mut self, value: Duration) -> serialport::Result<()> {
            self.timeout = value;
            Ok(())
        }
        fn write_request_to_send(&mut self, _: bool) -> serialport::Result<()> {
            panic!("must not toggle RTS")
        }
        fn write_data_terminal_ready(&mut self, _: bool) -> serialport::Result<()> {
            panic!("must not toggle DTR")
        }
        fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
            unimplemented!()
        }
        fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
            unimplemented!()
        }
        fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
            unimplemented!()
        }
        fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
            unimplemented!()
        }
        fn bytes_to_read(&self) -> serialport::Result<u32> {
            Ok(self.input.len() as u32)
        }
        fn bytes_to_write(&self) -> serialport::Result<u32> {
            Ok(0)
        }
        fn clear(&self, _: serialport::ClearBuffer) -> serialport::Result<()> {
            panic!("must not discard data")
        }
        fn try_clone(&self) -> serialport::Result<Box<dyn SerialPort>> {
            panic!("must use the existing port")
        }
        fn set_break(&self) -> serialport::Result<()> {
            unimplemented!()
        }
        fn clear_break(&self) -> serialport::Result<()> {
            unimplemented!()
        }
    }

    fn original() -> Settings {
        Settings {
            port: "EXISTING".into(),
            baud: 9600,
            data_bits: DataBits::Seven,
            parity: Parity::Even,
            stop_bits: StopBits::Two,
            flow: FlowControl::Hardware,
        }
    }

    #[test]
    fn upgrade_reuses_the_open_port_and_restores_it_after_success_cancel_or_failure() {
        // Each case uses one existing port object, then performs ordinary I/O on that object.
        for mode in 0..5 {
            let input = match mode {
                0 | 4 => {
                    let mut response = b"REBOOTING\n[BOOT] Press U for update\n[UPGRADE] 1 AppNS  2 AppS + AppNS  0 Boot\n[YMODEM] Send AppNS -trusted.bin\n".to_vec();
                    response.extend([b'C', 6, b'C', 6, 6, 6, b'C', 6]);
                    response.extend(b"[UPGRADE] Installed, done\nSTM32N6 AppNS\nVersion: test\n");
                    response
                }
                2 => b"BUSY\n".to_vec(),
                _ => Vec::new(),
            };
            let expected_rx = input.len() as u64;
            let mut port = ExistingPort {
                settings: original(),
                timeout: READ_TIMEOUT,
                input: input.into(),
                writes: Vec::new(),
                fail_baud: match mode {
                    3 => 115200,
                    4 => 9600,
                    _ => 0,
                },
            };
            let request = Request {
                secure: None,
                nonsecure: upgrade_protocol::tests::image(true),
                manual: false,
                cancel: Arc::new(AtomicBool::new(mode == 1)),
            };
            let (sender, receiver) = mpsc::sync_channel(128);
            let restored = run_upgrade(&mut port, &original(), request, &sender);
            assert_eq!(restored.is_ok(), mode != 4);
            let mut counted = false;
            let mut finished = false;
            for event in receiver.try_iter() {
                match event {
                    Event::Upgrade(upgrade_protocol::Event::TransferCount { received, sent }) => {
                        assert_eq!(received, expected_rx);
                        assert_eq!(
                            sent,
                            port.writes.iter().map(|b| b.len() as u64).sum::<u64>()
                        );
                        counted = true;
                    }
                    Event::Upgrade(upgrade_protocol::Event::Done(result)) => {
                        assert!(counted);
                        assert_eq!(result.is_ok(), mode == 0);
                        finished = true;
                    }
                    _ => (),
                }
            }
            assert!(finished);
            if mode == 4 {
                continue;
            }
            assert_eq!(port.baud_rate().unwrap(), 9600);
            assert_eq!(port.data_bits().unwrap(), DataBits::Seven);
            assert_eq!(port.parity().unwrap(), Parity::Even);
            assert_eq!(port.stop_bits().unwrap(), StopBits::Two);
            assert_eq!(port.flow_control().unwrap(), FlowControl::Hardware);
            assert_eq!(port.timeout(), READ_TIMEOUT);
            if mode == 1 || mode == 3 {
                assert!(port.writes.is_empty());
            }
            port.input.extend(b"normal");
            let mut byte = [0];
            port.read_exact(&mut byte).unwrap();
            assert_eq!(&byte, b"n");
            assert_eq!(write_chunk_once(&mut port, b"AT?"), (3, None));
            assert_eq!(port.writes.last().unwrap(), b"AT?");
        }
    }
}
