use serialport::{DataBits, FlowControl, Parity, SerialPort, StopBits};
use std::io::{self, ErrorKind};
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
    Received(Vec<Packet>),
    Sent {
        packet: Packet,
        requested: usize,
        error: Option<String>,
    },
    Closed(Option<String>),
}

pub struct Packet {
    pub time: SYSTEMTIME,
    pub bytes: Vec<u8>,
}

#[derive(Default)]
struct ReceiveRecords {
    pending: Option<Packet>,
    last_read: Option<Instant>,
}

impl ReceiveRecords {
    fn take_if_idle(&mut self, now: Instant) -> Option<Packet> {
        if self
            .last_read
            .is_some_and(|last| now.duration_since(last) >= RECEIVE_IDLE)
        {
            self.pending.take()
        } else {
            None
        }
    }

    fn push(&mut self, bytes: &[u8], now: Instant, time: SYSTEMTIME) -> Vec<Packet> {
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
            });
            record.bytes.push(byte);
            if byte == b'\n' || (record.bytes.len() >= MAX_RECEIVE_RECORD && byte != b'\r') {
                records.extend(self.pending.take());
            }
        }
        self.last_read = Some(now);
        records
    }
}

pub fn local_time() -> SYSTEMTIME {
    let mut time = SYSTEMTIME::default();
    unsafe { GetLocalTime(&mut time) };
    time
}

pub struct Connection {
    pub commands: Option<SyncSender<Vec<u8>>>,
    pub events: Receiver<Event>,
}

pub fn connect(settings: Settings) -> io::Result<Connection> {
    let (commands, command_rx) = mpsc::sync_channel(1);
    let (event_tx, events) = mpsc::sync_channel(32);
    std::thread::Builder::new()
        .name("serial-io".into())
        .spawn(move || {
            let result = serialport::new(settings.port, settings.baud)
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

            let reason = exchange(&mut *port, settings.baud, command_rx, &event_tx);
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
    baud: u32,
    commands: Receiver<Vec<u8>>,
    events: &SyncSender<Event>,
) -> Option<String> {
    let mut input = [0; 8192];
    let mut incoming = ReceiveRecords::default();
    let mut outgoing: Option<(Vec<u8>, usize)> = None;
    // Keep each synchronous driver write short so reads and closing get a turn.
    let write_chunk = (baud as usize / 120).clamp(1, 8192);
    let closed = loop {
        match commands.try_recv() {
            Ok(data) => outgoing = Some((data, 0)),
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
        },
        requested,
        error,
    })
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
