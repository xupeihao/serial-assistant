use crate::file_dialog;
use crate::serial_io;
use crate::upgrade_protocol::{Event, Firmware, Protocol};
use crate::window::wide;
use crate::window_layout::WindowLayout;
use std::cell::RefCell;
use std::io;
use std::path::Path;
use std::ptr::null;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver},
};
use std::time::Duration;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub struct UpgradeWindow {
    state: Box<RefCell<Upgrade>>,
}

impl UpgradeWindow {
    pub fn create(owner: HWND) -> io::Result<Self> {
        let state = Box::new(RefCell::new(Upgrade {
            window: std::ptr::null_mut(),
            worker: None,
            cancel: Arc::new(AtomicBool::new(false)),
            cancelable: true,
            paired: false,
            layout: WindowLayout::default(),
            initial_size: (0, 0),
            original: Vec::new(),
        }));
        let window = unsafe {
            CreateDialogParamW(
                GetModuleHandleW(null()),
                102usize as *const u16,
                owner,
                Some(dialog_proc),
                (&*state as *const RefCell<Upgrade>) as LPARAM,
            )
        };
        if window.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { state })
    }

    pub fn hwnd(&self) -> HWND {
        self.state.borrow().window
    }

    pub fn show(&self) {
        unsafe {
            ShowWindow(self.hwnd(), SW_SHOW);
            SetForegroundWindow(self.hwnd());
        }
    }

    pub fn request_close(&self) -> bool {
        let ui = self.state.borrow();
        if ui.worker.is_none() {
            return true;
        }
        if ui.cancelable {
            ui.cancel.store(true, Ordering::Relaxed);
        }
        drop(ui);
        self.show();
        set(self.hwnd(), 113, "请等待升级结束后再关闭主窗口");
        false
    }
}

impl Drop for UpgradeWindow {
    fn drop(&mut self) {
        let state = self.state.borrow();
        state.cancel.store(true, Ordering::Relaxed);
        unsafe {
            if IsWindow(state.window) != 0 {
                DestroyWindow(state.window);
            }
        }
    }
}

struct Upgrade {
    window: HWND,
    worker: Option<Receiver<Event>>,
    cancel: Arc<AtomicBool>,
    cancelable: bool,
    paired: bool,
    layout: WindowLayout,
    initial_size: (i32, i32),
    original: Vec<(HWND, RECT, i32)>,
}

fn text(window: HWND, id: i32) -> String {
    unsafe {
        let item = GetDlgItem(window, id);
        let mut buffer = vec![0; GetWindowTextLengthW(item) as usize + 1];
        let count = GetWindowTextW(item, buffer.as_mut_ptr(), buffer.len() as i32);
        String::from_utf16_lossy(&buffer[..count as usize])
    }
}

fn set(window: HWND, id: i32, value: &str) {
    unsafe { SetDlgItemTextW(window, id, wide(value).as_ptr()) };
}

fn choice(window: HWND, id: i32) -> usize {
    unsafe { SendDlgItemMessageW(window, id, CB_GETCURSEL, 0, 0).max(0) as usize }
}

fn fill(window: HWND, id: i32, values: &[&str]) {
    unsafe {
        SendDlgItemMessageW(window, id, CB_RESETCONTENT, 0, 0);
        for value in values {
            SendDlgItemMessageW(window, id, CB_ADDSTRING, 0, wide(value).as_ptr() as LPARAM);
        }
        if !values.is_empty() {
            SendDlgItemMessageW(window, id, CB_SETCURSEL, 0, 0);
        }
    }
}

impl Upgrade {
    fn init(&mut self, window: HWND) {
        self.window = window;
        fill(window, 102, &["由 AppNS 命令进入", "手动复位或上电进入"]);
        fill(window, 103, &["仅 AppNS", "AppS 和 AppNS 配套升级"]);
        unsafe {
            SendDlgItemMessageW(window, 111, EM_SETLIMITTEXT, 160000, 0);
            SendDlgItemMessageW(window, 104, EM_SETLIMITTEXT, 32767, 0);
            SendDlgItemMessageW(window, 106, EM_SETLIMITTEXT, 32767, 0);
        }
        let mut client = RECT::default();
        unsafe { GetClientRect(window, &mut client) };
        self.initial_size = (client.right, client.bottom);
        let mut child = unsafe { GetWindow(window, GW_CHILD) };
        while !child.is_null() {
            let mut rect = RECT::default();
            unsafe {
                GetWindowRect(child, &mut rect);
                MapWindowPoints(std::ptr::null_mut(), window, (&raw mut rect).cast(), 2);
            }
            self.original
                .push((child, rect, unsafe { GetDlgCtrlID(child) }));
            child = unsafe { GetWindow(child, GW_HWNDNEXT) };
        }
        self.refresh();
    }

    fn resize(&self) {
        let mut client = RECT::default();
        unsafe { GetClientRect(self.window, &mut client) };
        let dx = client.right - self.initial_size.0;
        let dy = client.bottom - self.initial_size.1;
        self.layout.apply(
            self.window,
            96,
            self.original.iter().map(|&(hwnd, rect, id)| {
                let mut x = rect.left;
                let mut y = rect.top;
                let mut width = rect.right - rect.left;
                let mut height = rect.bottom - rect.top;
                match id {
                    102 | 103 | 104 | 106 | 111 | 112 | 113 => width += dx,
                    105 | 107 | 118 => x += dx,
                    _ => (),
                }
                if id == 111 {
                    height += dy;
                }
                if id == 112 || id == 113 {
                    y += dy;
                }
                // Combo box height includes the popup, while GetWindowRect gives the closed height.
                if [100, 102, 103].contains(&id) {
                    height += 150;
                }
                (hwnd, x, y, width, height)
            }),
        );
    }

    fn update(&self) {
        let idle = self.worker.is_none();
        for id in [100, 101, 102, 103, 104, 105, 106, 107, 108, 110] {
            let enabled = match id {
                110 => !idle && self.cancelable,
                104 | 105 => idle && self.paired,
                108 => idle && !text(self.window, 100).is_empty(),
                _ => idle,
            };
            unsafe { EnableWindow(GetDlgItem(self.window, id), i32::from(enabled)) };
        }
    }

    fn refresh(&mut self) {
        let previous = text(self.window, 100);
        match serialport::available_ports() {
            Ok(mut ports) => {
                ports.sort_by(|a, b| a.port_name.cmp(&b.port_name));
                fill(
                    self.window,
                    100,
                    &ports
                        .iter()
                        .map(|p| p.port_name.as_str())
                        .collect::<Vec<_>>(),
                );
                if let Some(index) = ports.iter().position(|p| p.port_name == previous) {
                    unsafe { SendDlgItemMessageW(self.window, 100, CB_SETCURSEL, index, 0) };
                }
            }
            Err(error) => set(self.window, 113, &format!("刷新失败 · {error}")),
        }
        self.update();
    }

    fn log(&self, message: &str) {
        let now = serial_io::local_time();
        let line = wide(&format!(
            "{:02}:{:02}:{:02}.{:03}  {message}\r\n",
            now.wHour, now.wMinute, now.wSecond, now.wMilliseconds
        ));
        unsafe {
            let log = GetDlgItem(self.window, 111);
            if GetWindowTextLengthW(log) > 120000 {
                SetWindowTextW(log, wide("").as_ptr());
            }
            SendMessageW(log, EM_SETSEL, i32::MAX as usize, i32::MAX as isize);
            SendMessageW(log, EM_REPLACESEL, 0, line.as_ptr() as LPARAM);
            SendMessageW(log, WM_VSCROLL, SB_BOTTOM as usize, 0);
        }
    }

    fn start(&mut self) {
        if self.worker.is_some() {
            return;
        }
        let images = (|| {
            let secure = if self.paired {
                Some(Firmware::load(Path::new(&text(self.window, 104)), false)?)
            } else {
                None
            };
            let ns = Firmware::load(Path::new(&text(self.window, 106)), true)?;
            Ok::<_, io::Error>((secure, ns))
        })();
        let images = match images {
            Ok(images) => images,
            Err(error) => {
                set(self.window, 113, &error.to_string());
                self.log(&error.to_string());
                return;
            }
        };
        let name = text(self.window, 100);
        let manual = choice(self.window, 102) == 1;
        let (tx, rx) = mpsc::channel();
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        // A timer is required before the worker can deliver any result.
        if unsafe { SetTimer(self.window, 1, 40, None) } == 0 {
            set(self.window, 113, "无法启动升级状态检查");
            return;
        }
        self.log(&format!("开始升级 · {name}"));
        let thread = std::thread::Builder::new()
            .name("n6-upgrade".into())
            .spawn(move || {
                let events = |event| {
                    let _ = tx.send(event);
                };
                let result = (|| {
                    let mut port = serialport::new(name, 115200)
                        .timeout(Duration::from_millis(100))
                        .open()
                        .map_err(|error| {
                            io::Error::other(format!(
                                "打开串口失败 · {error}。同一 COM 口请先在主窗口关闭"
                            ))
                        })?;
                    port.set_timeout(Duration::from_millis(100))?;
                    let mut protocol = Protocol::new(UpgradePort(port), &events, &cancel);
                    let (secure, ns) = images;
                    protocol.run(secure, ns, manual)
                })();
                events(Event::Done(result.map_err(|error| error.to_string())));
            });
        if let Err(error) = thread {
            unsafe { KillTimer(self.window, 1) };
            set(self.window, 113, &error.to_string());
            return;
        }
        self.worker = Some(rx);
        self.cancelable = true;
        unsafe { SendDlgItemMessageW(self.window, 112, PBM_SETPOS, 0, 0) };
        set(self.window, 113, "正在连接串口");
        self.update();
    }

    fn poll(&mut self) {
        for _ in 0..128 {
            let Some(rx) = &self.worker else {
                return;
            };
            match rx.try_recv() {
                Ok(Event::Log(text)) => self.log(&text),
                Ok(Event::Progress(value, text)) => {
                    unsafe { SendDlgItemMessageW(self.window, 112, PBM_SETPOS, value as usize, 0) };
                    set(self.window, 113, &text);
                }
                Ok(Event::Cancelable(allowed)) => {
                    self.cancelable = allowed;
                    self.update();
                }
                Ok(Event::Done(result)) => {
                    if let Err(error) = result {
                        self.log(&error);
                        set(self.window, 113, &error);
                    }
                    self.worker = None;
                    unsafe { KillTimer(self.window, 1) };
                    self.update();
                }
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.worker = None;
                    unsafe { KillTimer(self.window, 1) };
                    set(self.window, 113, "升级线程已退出，请检查日志");
                    self.update();
                }
            }
        }
    }
}

// serialport's timeout applies to both reads and writes on Windows.
struct UpgradePort(Box<dyn serialport::SerialPort>);
impl io::Read for UpgradePort {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}
impl io::Write for UpgradePort {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.set_timeout(Duration::from_secs(2))?;
        let result = self.0.write(bytes);
        self.0.set_timeout(Duration::from_millis(100))?;
        result
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

unsafe extern "system" fn dialog_proc(
    window: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    if msg == WM_INITDIALOG {
        unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, lparam) };
        let state = unsafe { &*(lparam as *const RefCell<Upgrade>) };
        state.borrow_mut().init(window);
        unsafe { SetFocus(GetDlgItem(window, 100)) };
        return 0;
    }
    let state = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const RefCell<Upgrade>;
    let Some(state) = (unsafe { state.as_ref() }) else {
        return 0;
    };
    if msg == WM_COMMAND && (wparam == 105 || wparam == 107) {
        let result = file_dialog::open_firmware(window);
        match result {
            Ok(Some(path)) => set(
                window,
                if wparam == 105 { 104 } else { 106 },
                &path.to_string_lossy(),
            ),
            Err(error) => set(window, 113, &error.to_string()),
            _ => (),
        }
        return 1;
    }
    if msg == WM_GETMINMAXINFO {
        if let Ok(ui) = state.try_borrow() {
            if ui.initial_size.0 == 0 {
                return 0;
            }
            let mut bounds = RECT::default();
            let mut client = RECT::default();
            unsafe {
                GetWindowRect(window, &mut bounds);
                GetClientRect(window, &mut client);
            }
            let dpi = unsafe { GetDpiForWindow(window) }.max(96);
            let info = unsafe { &mut *(lparam as *mut MINMAXINFO) };
            info.ptMinTrackSize.x = (660 * dpi / 96) as i32;
            let log_height = ui
                .original
                .iter()
                .find(|(_, _, id)| *id == 111)
                .map_or(0, |(_, rect, _)| rect.bottom - rect.top);
            info.ptMinTrackSize.y =
                ui.initial_size.1 - log_height + (90 * dpi / 96) as i32 + bounds.bottom
                    - bounds.top
                    - client.bottom;
        }
        return 1;
    }
    if let Ok(mut ui) = state.try_borrow_mut() {
        match msg {
            WM_TIMER => ui.poll(),
            WM_SIZE if wparam != SIZE_MINIMIZED as usize => ui.resize(),
            WM_CLOSE => {
                if ui.worker.is_some() {
                    if ui.cancelable {
                        ui.cancel.store(true, Ordering::Relaxed);
                    }
                    set(
                        window,
                        113,
                        if ui.cancelable {
                            "正在停止，请稍候"
                        } else {
                            "板端正在安装，请等待安装结果"
                        },
                    );
                } else {
                    unsafe { ShowWindow(window, SW_HIDE) };
                }
            }
            WM_COMMAND => match wparam & 0xffff {
                100 => ui.update(),
                101 => ui.refresh(),
                103 => {
                    ui.paired = choice(window, 103) == 1;
                    ui.update();
                }
                108 => ui.start(),
                110 => {
                    if ui.cancelable {
                        ui.cancel.store(true, Ordering::Relaxed);
                    }
                }
                2 => {
                    unsafe { PostMessageW(window, WM_CLOSE, 0, 0) };
                }
                _ => return 0,
            },
            _ => return 0,
        }
        return 1;
    }
    0
}
