use std::io::{self, ErrorKind, Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const ACK: u8 = 6;
const NAK: u8 = 0x15;
const CAN: u8 = 0x18;
const EOT: u8 = 4;
const CRC_REQUEST: u8 = b'C';

pub enum Event {
    Log(String),
    Progress(u32, String),
    Cancelable(bool),
    Done(Result<(), String>),
    TransferCount { received: u64, sent: u64 },
}

pub struct Request {
    pub secure: Option<Firmware>,
    pub nonsecure: Firmware,
    pub manual: bool,
    pub cancel: Arc<AtomicBool>,
}

pub struct Firmware {
    bytes: Vec<u8>,
    nonsecure: bool,
}

fn issue(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

impl Firmware {
    pub fn load(path: &Path, nonsecure: bool) -> io::Result<Self> {
        if path.as_os_str().is_empty() {
            return Err(issue(if nonsecure {
                "请选择 AppNS 镜像"
            } else {
                "请选择 AppS 镜像"
            }));
        }
        let size = std::fs::metadata(path)?.len();
        if !(0x408..=0x80000).contains(&size) {
            return Err(issue("镜像长度超出当前 N6 工程允许的范围"));
        }
        Self::validate(std::fs::read(path)?, nonsecure)
    }

    fn validate(bytes: Vec<u8>, nonsecure: bool) -> io::Result<Self> {
        let word = |offset| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        if !(0x408..=0x80000).contains(&bytes.len())
            || word(0) != 0x324d5453
            || word(0x68) != 0x00020300
            || word(0x98) != 0
        {
            return Err(issue("镜像需要是当前 N6 工程生成的 -trusted.bin"));
        }
        let extension = word(0x88);
        let length = word(0x6c);
        if extension > 0x360
            || 0xa0 + u64::from(extension) + u64::from(length) != bytes.len() as u64
        {
            return Err(issue("镜像头声明的长度与文件不一致"));
        }
        let (stack, base) = if nonsecure {
            (0x24180000, 0x24100000)
        } else {
            (0x34100000, 0x34000000)
        };
        let reset = word(0x404);
        if word(0x400) != stack
            || reset & 1 == 0
            || reset & !1 < base + 0x400
            || reset & !1 > base + bytes.len() as u32 - 2
        {
            return Err(issue(if nonsecure {
                "该文件不是本工程的 AppNS 镜像"
            } else {
                "该文件不是本工程的 AppS 镜像"
            }));
        }
        let checksum = bytes[0xa0 + extension as usize..]
            .iter()
            .fold(0u32, |sum, &byte| sum.wrapping_add(u32::from(byte)));
        if checksum != word(0x64) {
            return Err(issue("STM32 镜像校验和不符"));
        }
        Ok(Self { bytes, nonsecure })
    }
}

pub struct Protocol<'a, P> {
    port: P,
    events: &'a dyn Fn(Event),
    cancel: &'a AtomicBool,
    installation: bool,
    line: Vec<u8>,
}

impl<'a, P: Read + Write> Protocol<'a, P> {
    pub fn new(port: P, events: &'a dyn Fn(Event), cancel: &'a AtomicBool) -> Self {
        Self {
            port,
            events,
            cancel,
            installation: false,
            line: Vec::new(),
        }
    }

    fn check_cancel(&self) -> io::Result<()> {
        if !self.installation && self.cancel.load(Ordering::Relaxed) {
            Err(io::Error::new(ErrorKind::Interrupted, "已停止发送"))
        } else {
            Ok(())
        }
    }

    fn progress(&self, value: usize, text: impl Into<String>) {
        (self.events)(Event::Progress(value as u32, text.into()));
    }

    fn read_byte(&mut self) -> io::Result<Option<u8>> {
        let mut byte = [0];
        match self.port.read(&mut byte) {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(byte[0])),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn read_line(&mut self, deadline: Option<Instant>) -> io::Result<String> {
        while deadline.is_none_or(|limit| Instant::now() < limit) {
            self.check_cancel()?;
            if let Some(value) = self.read_byte()? {
                if value == b'\n' {
                    let text = String::from_utf8_lossy(&self.line)
                        .trim_end_matches('\r')
                        .to_owned();
                    self.line.clear();
                    if !text.is_empty() {
                        (self.events)(Event::Log(text.clone()));
                        return Ok(text);
                    }
                } else if (value == b'\r' || value >= 32) && self.line.len() < 512 {
                    self.line.push(value);
                }
            }
        }
        Err(io::Error::new(ErrorKind::TimedOut, "等待板端提示超时"))
    }

    fn wait_line(
        &mut self,
        prefix: &str,
        seconds: u64,
        during_upgrade: bool,
    ) -> io::Result<String> {
        let deadline = Some(Instant::now() + Duration::from_secs(seconds));
        loop {
            let text = self.read_line(deadline).map_err(|error| {
                if error.kind() == ErrorKind::TimedOut {
                    io::Error::new(ErrorKind::TimedOut, format!("等待板端提示超时 · {prefix}"))
                } else {
                    error
                }
            })?;
            if text.starts_with(prefix) {
                return Ok(text);
            }
            if text == "BUSY" {
                return Err(issue("应用回复 BUSY，请等待板端启动完成后重试"));
            }
            if during_upgrade
                && ["[YMODEM]", "[UPGRADE] 1 AppNS", "[UPGRADE] R Retry"]
                    .iter()
                    .any(|prefix| text.starts_with(prefix))
            {
                return Err(issue("板端未继续预期步骤，请查看串口日志"));
            }
        }
    }

    pub fn run(
        &mut self,
        secure: Option<Firmware>,
        nonsecure: Firmware,
        manual: bool,
    ) -> io::Result<()> {
        self.check_cancel()?;
        self.progress(
            0,
            if manual {
                "现在复位或重新上电，等待 FSBL"
            } else {
                "请求 AppNS 进入升级"
            },
        );
        if !manual {
            self.port.write_all(b"MCUUPDATE\r\n")?;
            self.wait_line("REBOOTING", 5, false)?;
        }
        let recovery = self.open_menu(if manual { 60 } else { 12 })?;
        self.port.write_all(&[if recovery {
            b'D'
        } else if secure.is_some() {
            b'2'
        } else {
            b'1'
        }])?;
        let previous = secure.as_ref().map_or(0, |image| image.bytes.len());
        let total = previous + nonsecure.bytes.len();
        if let Some(secure) = secure {
            self.send_image(secure, 0, total, false)?;
            self.wait_line("[UPGRADE] Image staged and verified", 25, true)?;
        }
        self.send_image(nonsecure, previous, total, true)?;
        self.progress(99, "等待板端校验、安装和重启");
        self.wait_line("[UPGRADE] Installed,", 180, true)?;
        (self.events)(Event::Log("板端已确认安装完成，等待新应用打印".into()));
        let startup = self
            .wait_line("STM32N6 AppNS", 20, false)
            .and_then(|_| self.wait_line("Version: ", 3, false));
        match startup {
            Ok(version) => self.progress(100, format!("升级完成 · {version}")),
            Err(error) if error.kind() == ErrorKind::TimedOut => {
                self.progress(100, "安装已确认，未收到 AppNS 版本打印");
                (self.events)(Event::Log(
                    "请在主窗口检查 AppNS 启动日志，或监听重新上电后的输出".into(),
                ));
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    fn open_menu(&mut self, seconds: u64) -> io::Result<bool> {
        let deadline = Some(Instant::now() + Duration::from_secs(seconds));
        loop {
            match self.read_line(deadline)?.as_str() {
                "[BOOT] Press U for update" => self.port.write_all(b"u")?,
                "[UPGRADE] 1 AppNS  2 AppS + AppNS  0 Boot" => return Ok(false),
                "[UPGRADE] R Retry  D Download the same update again" => {
                    (self.events)(Event::Log(
                        "板端需要恢复。请使用与中断前完全相同的镜像组合".into(),
                    ));
                    return Ok(true);
                }
                _ => (),
            }
        }
    }

    fn read_control(
        &mut self,
        seconds: u64,
        first: u8,
        second: Option<u8>,
    ) -> io::Result<Option<u8>> {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let mut cancel = false;
        while Instant::now() < deadline {
            self.check_cancel()?;
            if let Some(value) = self.read_byte()? {
                if value == first || second == Some(value) {
                    return Ok(Some(value));
                }
                if value == CAN && cancel {
                    return Err(issue("板端取消了 YMODEM 传输"));
                }
                cancel = value == CAN;
            }
        }
        Ok(None)
    }

    fn send_acked(&mut self, data: &[u8]) -> io::Result<()> {
        for _ in 0..8 {
            self.check_cancel()?;
            self.port.write_all(data)?;
            if self.read_control(3, ACK, Some(NAK))? == Some(ACK) {
                return Ok(());
            }
        }
        Err(issue("YMODEM 连续重传失败。请检查串口连线和板端日志"))
    }

    fn expect_c(&mut self, seconds: u64, error: &str) -> io::Result<()> {
        if self.read_control(seconds, CRC_REQUEST, None)? == Some(CRC_REQUEST) {
            Ok(())
        } else {
            Err(io::Error::new(ErrorKind::TimedOut, error))
        }
    }

    fn send_image(
        &mut self,
        image: Firmware,
        previous: usize,
        total: usize,
        last: bool,
    ) -> io::Result<()> {
        let target = if image.nonsecure { "AppNS" } else { "AppS" };
        self.wait_line(&format!("[YMODEM] Send {target} -trusted.bin"), 12, true)?;
        self.progress(previous * 98 / total, format!("正在发送 {target}"));
        let mut end_block_started = false;
        let result = (|| {
            self.expect_c(12, "板端未发送 YMODEM 握手字符 C")?;
            let filename = if image.nonsecure {
                "N6_AppliNonSecure-trusted.bin"
            } else {
                "N6_AppliSecure-trusted.bin"
            };
            let metadata = format!("{filename}\0{}", image.bytes.len());
            let mut header = [0; 128];
            header[..metadata.len()].copy_from_slice(metadata.as_bytes());
            self.send_acked(&packet(0, &header))?;
            self.expect_c(10, "文件名块已发送，板端未请求数据块")?;
            let mut offset = 0;
            let mut index = 1u8;
            while offset < image.bytes.len() {
                let remaining = image.bytes.len() - offset;
                let mut block = vec![0x1a; if remaining > 128 { 1024 } else { 128 }];
                let count = remaining.min(block.len());
                block[..count].copy_from_slice(&image.bytes[offset..offset + count]);
                self.send_acked(&packet(index, &block))?;
                index = index.wrapping_add(1);
                offset += count;
                self.progress(
                    (previous + offset) * 98 / total,
                    format!("正在发送 {target}  {offset} / {} 字节", image.bytes.len()),
                );
            }
            self.send_acked(&[EOT])?;
            self.expect_c(10, "板端未请求 YMODEM 结束块")?;
            self.check_cancel()?;
            if last {
                self.installation = true;
                (self.events)(Event::Cancelable(false));
            }
            end_block_started = true;
            self.send_acked(&packet(0, &[0; 128]))
        })();
        if result.is_err() && !end_block_started {
            let _ = self.port.write_all(&[CAN, CAN]);
        }
        result
    }
}

fn packet(index: u8, data: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(data.len() + 5);
    packet.extend([if data.len() == 128 { 1 } else { 2 }, index, index ^ 0xff]);
    packet.extend(data);
    let crc = data.iter().fold(0u16, |mut crc, &byte| {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = (crc << 1) ^ if crc & 0x8000 != 0 { 0x1021 } else { 0 };
        }
        crc
    });
    packet.extend(crc.to_be_bytes());
    packet
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    pub(crate) fn image(nonsecure: bool) -> Firmware {
        Firmware::validate(image_bytes(nonsecure), nonsecure).unwrap()
    }

    pub(crate) fn image_bytes(nonsecure: bool) -> Vec<u8> {
        let mut bytes = vec![0; 0x408];
        let (stack, base) = if nonsecure {
            (0x24180000u32, 0x24100000u32)
        } else {
            (0x34100000, 0x34000000)
        };
        for (offset, value) in [
            (0, 0x324d5453),
            (0x68, 0x00020300),
            (0x88, 0x360),
            (0x6c, 8),
            (0x400, stack),
            (0x404, base + 0x401),
        ] {
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        let sum: u32 = bytes[0x400..].iter().map(|&b| u32::from(b)).sum();
        bytes[0x64..0x68].copy_from_slice(&sum.to_le_bytes());
        bytes
    }

    struct FakePort {
        input: VecDeque<u8>,
        writes: Rc<RefCell<Vec<Vec<u8>>>>,
    }
    impl Read for FakePort {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            bytes[0] = self
                .input
                .pop_front()
                .ok_or_else(|| issue("测试板端应答耗尽"))?;
            Ok(1)
        }
    }
    impl Write for FakePort {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes.borrow_mut().push(bytes.to_vec());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn validates_trusted_images_and_ymodem_crc() {
        let ns = image(true);
        assert!(Firmware::validate(ns.bytes.clone(), false).is_err());
        let mut bad = ns.bytes;
        bad[0x407] ^= 1;
        assert!(Firmware::validate(bad, true).is_err());
        let test = packet(7, b"123456789");
        assert_eq!(&test[..3], &[2, 7, 248]);
        assert_eq!(&test[test.len() - 2..], &[0x31, 0xc3]);
        assert!(Firmware::validate(vec![0; 5], true).is_err());
    }

    #[test]
    fn full_upgrade_handles_both_targets_recovery_and_eot_retry() {
        for (paired, recovery) in [(false, false), (true, false), (false, true), (true, true)] {
            let mut response = if recovery {
                b"[UPGRADE] R Retry  D Download the same update again\n".to_vec()
            } else {
                b"REBOOTING\n[BOOT] Press U for update\n[UPGRADE] 1 AppNS  2 AppS + AppNS  0 Boot\n"
                    .to_vec()
            };
            for target in if paired {
                vec!["AppS", "AppNS"]
            } else {
                vec!["AppNS"]
            } {
                response.extend(format!("[YMODEM] Send {target} -trusted.bin\n").bytes());
                response.extend([
                    CRC_REQUEST,
                    ACK,
                    CRC_REQUEST,
                    ACK,
                    ACK,
                    NAK,
                    ACK,
                    CRC_REQUEST,
                    ACK,
                ]);
                if target == "AppS" {
                    response.extend(b"[UPGRADE] Image staged and verified\n");
                }
            }
            response.extend(b"[UPGRADE] Installed, done\nSTM32N6 AppNS\nVersion: test\n");
            let writes = Rc::new(RefCell::new(Vec::new()));
            let port = FakePort {
                input: response.into(),
                writes: writes.clone(),
            };
            let cancel = AtomicBool::new(false);
            let progress = RefCell::new(Vec::new());
            let callback = |event| {
                if let Event::Progress(value, _) = event {
                    progress.borrow_mut().push(value);
                }
                // The final ending block commits installation and must ignore later cancellation.
                if let Event::Cancelable(false) = event {
                    cancel.store(true, Ordering::Relaxed);
                }
            };
            let mut protocol = Protocol::new(port, &callback, &cancel);
            protocol
                .run(paired.then(|| image(false)), image(true), recovery)
                .unwrap();
            assert_eq!(progress.borrow().last(), Some(&100));
            let writes = writes.borrow();
            let start = if recovery {
                assert_eq!(writes[0], b"D");
                1
            } else {
                assert_eq!(
                    writes[..3],
                    [
                        b"MCUUPDATE\r\n".to_vec(),
                        b"u".to_vec(),
                        if paired { b"2".to_vec() } else { b"1".to_vec() }
                    ]
                );
                3
            };
            for (i, target) in if paired {
                vec![false, true]
            } else {
                vec![true]
            }
            .into_iter()
            .enumerate()
            {
                let block = &writes[start + i * 6..start + (i + 1) * 6];
                assert_eq!(block[0][0..3], [1, 0, 255]);
                assert_eq!(block[1], packet(1, &image(target).bytes[..1024]));
                let mut tail = [0x1a; 128];
                tail[..8].copy_from_slice(&image(target).bytes[1024..]);
                assert_eq!(block[2], packet(2, &tail));
                assert_eq!(block[3..5], [vec![EOT], vec![EOT]]);
                assert_eq!(block[5], packet(0, &[0; 128]));
            }
        }
    }

    #[test]
    fn cancellation_and_busy_do_not_start_transfer() {
        let writes = Rc::new(RefCell::new(Vec::new()));
        let callback = |_| ();
        let cancel = AtomicBool::new(true);
        let port = FakePort {
            input: VecDeque::new(),
            writes: writes.clone(),
        };
        assert_eq!(
            Protocol::new(port, &callback, &cancel)
                .run(None, image(true), true)
                .unwrap_err()
                .kind(),
            ErrorKind::Interrupted
        );
        assert!(writes.borrow().is_empty());
        cancel.store(false, Ordering::Relaxed);
        let port = FakePort {
            input: b"BUSY\n".to_vec().into(),
            writes: writes.clone(),
        };
        assert!(
            Protocol::new(port, &callback, &cancel)
                .run(None, image(true), false)
                .unwrap_err()
                .to_string()
                .contains("BUSY")
        );
        assert_eq!(writes.borrow().as_slice(), &[b"MCUUPDATE\r\n".to_vec()]);
    }
}
