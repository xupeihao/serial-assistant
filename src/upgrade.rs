use crate::file_dialog;
use crate::upgrade_protocol::{Event, Firmware, Request};
use crate::window::wide;
use std::cell::RefCell;
use std::io;
use std::path::Path;
use std::ptr::null;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub const START_MESSAGE: u32 = WM_APP + 2;

pub struct UpgradeWindow {
    state: Box<RefCell<Upgrade>>,
}

impl UpgradeWindow {
    pub fn create(owner: HWND) -> io::Result<Self> {
        let state = Box::new(RefCell::new(Upgrade {
            window: std::ptr::null_mut(),
            owner,
            active: false,
            connected: false,
            pending: None,
            cancel: Arc::new(AtomicBool::new(false)),
            cancelable: true,
            paired: false,
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
        let hwnd = self.hwnd();
        unsafe {
            ShowWindow(
                hwnd,
                if IsIconic(hwnd) != 0 {
                    SW_RESTORE
                } else {
                    SW_SHOW
                },
            );
            SetForegroundWindow(hwnd);
        }
    }

    pub fn set_connection(&self, connected: bool) {
        let mut ui = self.state.borrow_mut();
        ui.connected = connected;
        if !ui.active {
            set(
                ui.window,
                118,
                if connected {
                    "使用主窗口串口 · 升级日志显示在主窗口"
                } else {
                    "请先在主窗口打开串口 · 升级日志显示在主窗口"
                },
            );
        }
        ui.update();
    }

    pub fn take_request(&self) -> Option<Request> {
        self.state.borrow_mut().pending.take()
    }

    pub fn event(&self, event: &Event) {
        let mut ui = self.state.borrow_mut();
        match event {
            Event::Progress(value, text) => {
                unsafe { SendDlgItemMessageW(ui.window, 112, PBM_SETPOS, *value as usize, 0) };
                set(ui.window, 113, text);
            }
            Event::Cancelable(allowed) => ui.cancelable = *allowed,
            Event::Done(result) => {
                ui.active = false;
                ui.pending = None;
                set(
                    ui.window,
                    113,
                    match result {
                        Ok(()) => "升级完成，已恢复主窗口收发",
                        Err(error) => error,
                    },
                );
            }
            _ => (),
        }
        ui.update();
    }

    pub fn request_close(&self) -> bool {
        let ui = self.state.borrow();
        if !ui.active {
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
    owner: HWND,
    active: bool,
    connected: bool,
    pending: Option<Request>,
    cancel: Arc<AtomicBool>,
    cancelable: bool,
    paired: bool,
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
            SendDlgItemMessageW(window, 104, EM_SETLIMITTEXT, 32767, 0);
            SendDlgItemMessageW(window, 106, EM_SETLIMITTEXT, 32767, 0);
        }
        self.update();
    }

    fn update(&self) {
        let idle = !self.active;
        for id in [102, 103, 104, 105, 106, 107, 108, 110] {
            let enabled = match id {
                110 => !idle && self.cancelable,
                104 | 105 => idle && self.paired,
                108 => idle && self.connected,
                _ => idle,
            };
            unsafe { EnableWindow(GetDlgItem(self.window, id), i32::from(enabled)) };
        }
    }

    fn start(&mut self) {
        if self.active {
            return;
        }
        if !self.connected {
            set(self.window, 113, "请先在主窗口打开串口");
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
                return;
            }
        };
        self.cancel = Arc::new(AtomicBool::new(false));
        let (secure, nonsecure) = images;
        self.pending = Some(Request {
            secure,
            nonsecure,
            manual: choice(self.window, 102) == 1,
            cancel: self.cancel.clone(),
        });
        self.active = true;
        self.cancelable = true;
        unsafe { SendDlgItemMessageW(self.window, 112, PBM_SETPOS, 0, 0) };
        set(self.window, 113, "正在接管主窗口串口");
        if unsafe { PostMessageW(self.owner, START_MESSAGE, 0, 0) } == 0 {
            self.active = false;
            self.pending = None;
            set(self.window, 113, "无法提交升级请求，请重试");
        }
        self.update();
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
        unsafe { SetFocus(GetDlgItem(window, 102)) };
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
    if let Ok(mut ui) = state.try_borrow_mut() {
        match msg {
            WM_CLOSE => {
                if ui.active {
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
