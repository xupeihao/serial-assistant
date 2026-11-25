use crate::codec::{self, ReceiveFormat};
use crate::control::{self, ControlWindow};
use crate::file_dialog;
use crate::network_io::{self, Mode};
use crate::preferences::{
    BUFFER_LABELS, BUFFER_SIZES, LOG_FONT_COUNT, LOG_FONT_SIZES, Preferences,
};
use crate::serial_io::{self, Command, Connection, Event, Packet, Settings};
use crate::upgrade::{self, UpgradeWindow};
use crate::upgrade_protocol::Event as UpgradeEvent;
use crate::window_layout::WindowLayout;
use encoding_rs::{Encoding, GBK, UTF_8};
use serialport::{DataBits, FlowControl, Parity, StopBits};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::io;
use std::ptr::{null, null_mut};
use std::sync::mpsc::TryRecvError;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::System::SystemServices::{
    SS_CENTERIMAGE, SS_ENDELLIPSIS, SS_LEFT, SS_RIGHT,
};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

const TITLE: windows_sys::core::PCWSTR = w!("串口助手");
const CLASS_NAME: windows_sys::core::PCWSTR = w!("RustSerialAssistant");
const POLL_TIMER: usize = 1;
const MENU_EXTENSION_BASE: usize = 401;
const TOGGLE_EXTENSION_BASE: usize = 501;

#[derive(Clone, Copy)]
enum Extension {
    Control,
    Upgrade,
}

// Menu order also assigns Alt+1 through Alt+9 to registered extensions.
const EXTENSIONS: &[Extension] = &[Extension::Control, Extension::Upgrade];

impl Extension {
    fn name(self) -> &'static str {
        match self {
            Self::Control => "控制",
            Self::Upgrade => "N6 串口升级",
        }
    }
}

struct ExtensionShortcuts(HACCEL);

impl ExtensionShortcuts {
    fn new() -> io::Result<Self> {
        let keys: Vec<_> = (0..9u16)
            .flat_map(|index| {
                [b'1' as u16 + index, VK_NUMPAD1 + index].map(|key| ACCEL {
                    fVirt: FALT | FVIRTKEY,
                    key,
                    cmd: (TOGGLE_EXTENSION_BASE + index as usize) as u16,
                })
            })
            .collect();
        let table = unsafe { CreateAcceleratorTableW(keys.as_ptr(), keys.len() as i32) };
        if table.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(table))
        }
    }

    fn translate(&self, window: HWND, msg: &MSG) -> bool {
        let number = (b'1' as usize..=b'9' as usize).contains(&msg.wParam)
            || (VK_NUMPAD1 as usize..=VK_NUMPAD9 as usize).contains(&msg.wParam);
        if msg.message == WM_SYSKEYDOWN
            && msg.lParam & (1 << 30) != 0
            && number
            && unsafe { GetKeyState(VK_CONTROL as i32) >= 0 && GetKeyState(VK_SHIFT as i32) >= 0 }
        {
            return true;
        }
        unsafe { TranslateAcceleratorW(window, self.0, msg) != 0 }
    }
}

impl Drop for ExtensionShortcuts {
    fn drop(&mut self) {
        unsafe { DestroyAcceleratorTable(self.0) };
    }
}
const HISTORY_BASE_BYTES: usize = 96 * 1024;
const HISTORY_BASE_RECORDS: usize = 4096;
// Rich Edit messages from richedit.h.
const EM_EXLIMITTEXT: u32 = WM_USER + 53;
const EM_SETBKGNDCOLOR: u32 = WM_USER + 67;
const EM_SETCHARFORMAT: u32 = WM_USER + 68;
const SCF_ALL: usize = 4;
const CFM_FACE: u32 = 0x2000_0000;
const CFM_SIZE: u32 = 0x8000_0000;
const LOG_FONTS: [(&str, &str); LOG_FONT_COUNT] = [
    ("微软雅黑", "Microsoft YaHei UI"),
    ("等线", "DengXian"),
    ("宋体", "SimSun"),
    ("Consolas", "Consolas"),
];

// CHARFORMATW from richedit.h. Rich Edit uses twips for the point size.
#[repr(C)]
#[derive(Default)]
struct CharFormat {
    size: u32,
    mask: u32,
    effects: u32,
    height: i32,
    offset: i32,
    color: u32,
    charset: u8,
    pitch_and_family: u8,
    face: [u16; 32],
}
const WINDOW_STYLE: u32 = WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN;
const WINDOW_EX_STYLE: u32 = WS_EX_COMPOSITED;
const MARGIN: i32 = 16;
const DIVIDER_SIZE: i32 = 14;
const RECEIVE_TOP: i32 = 42;
const MIN_RECEIVE_HEIGHT: i32 = 95;
const MIN_SERIAL_WIDTH: i32 = 306;
const MIN_SEND_WIDTH: i32 = 388;
const MIN_LOWER_HEIGHT: i32 = 185;
const FOOTER_HEIGHT: i32 = 37;
const MIN_CLIENT_WIDTH: i32 = 936;
const WORKSPACE_COLOR: COLORREF = 0x00f7_f5f3;
const PANEL_BORDER: COLORREF = 0x00e4_ded8;
const DIVIDER_COLOR: COLORREF = 0x00b5_aca3;
const MIN_CLIENT_HEIGHT: i32 =
    RECEIVE_TOP + MIN_RECEIVE_HEIGHT + DIVIDER_SIZE + MIN_LOWER_HEIGHT + FOOTER_HEIGHT;

#[derive(Clone, Copy)]
enum Divider {
    Horizontal,
    Vertical,
}

struct Drag {
    divider: Divider,
    offset: i32,
}

#[derive(Clone, Copy, PartialEq)]
struct PaneLayout {
    width: i32,
    height: i32,
    serial_width: i32,
    lower_height: i32,
}

impl PaneLayout {
    fn new(width: i32, height: i32, serial_width: i32, lower_height: i32) -> Self {
        let width = width.max(MIN_CLIENT_WIDTH);
        let height = height.max(MIN_CLIENT_HEIGHT);
        Self {
            width,
            height,
            serial_width: serial_width.clamp(
                MIN_SERIAL_WIDTH,
                width - 2 * MARGIN - DIVIDER_SIZE - MIN_SEND_WIDTH,
            ),
            lower_height: lower_height.clamp(
                MIN_LOWER_HEIGHT,
                height - FOOTER_HEIGHT - DIVIDER_SIZE - RECEIVE_TOP - MIN_RECEIVE_HEIGHT,
            ),
        }
    }

    fn lower_top(&self) -> i32 {
        self.height - FOOTER_HEIGHT - self.lower_height
    }

    fn divider_rect(&self, divider: Divider) -> RECT {
        let lower_top = self.lower_top();
        let serial_end = MARGIN + self.serial_width;
        match divider {
            Divider::Horizontal => RECT {
                left: MARGIN,
                top: lower_top - DIVIDER_SIZE,
                right: self.width - MARGIN,
                bottom: lower_top,
            },
            Divider::Vertical => RECT {
                left: serial_end,
                top: lower_top,
                right: serial_end + DIVIDER_SIZE,
                bottom: self.height - FOOTER_HEIGHT,
            },
        }
    }

    fn divider_at(&self, point: POINT) -> Option<Divider> {
        [Divider::Horizontal, Divider::Vertical]
            .into_iter()
            .find(|&divider| {
                let rect = self.divider_rect(divider);
                (rect.left..rect.right).contains(&point.x)
                    && (rect.top..rect.bottom).contains(&point.y)
            })
    }

    fn borders(&self) -> [RECT; 2] {
        let divider = self.divider_rect(Divider::Vertical);
        [(MARGIN, divider.left), (divider.right, self.width - MARGIN)].map(|(left, right)| RECT {
            left,
            right,
            top: divider.top,
            bottom: divider.bottom,
        })
    }

    fn grips(&self) -> [RECT; 2] {
        [Divider::Horizontal, Divider::Vertical].map(|divider| {
            let area = self.divider_rect(divider);
            let x = (area.left + area.right) / 2;
            let y = (area.top + area.bottom) / 2;
            match divider {
                Divider::Horizontal => RECT {
                    left: x - 18,
                    top: y - 1,
                    right: x + 18,
                    bottom: y + 2,
                },
                Divider::Vertical => RECT {
                    left: x - 1,
                    top: y - 18,
                    right: x + 2,
                    bottom: y + 18,
                },
            }
        })
    }
}

#[derive(Clone, Copy)]
#[repr(usize)]
enum Id {
    ReceiveLabel = 100,
    HexDisplay,
    Follow,
    EncodingLabel,
    Encoding,
    ClearRx,
    Receive,
    SerialGroup,
    PortLabel,
    Port,
    Refresh,
    BaudLabel,
    Baud,
    DataLabel,
    DataBits,
    ParityLabel,
    Parity,
    StopLabel,
    StopBits,
    FlowLabel,
    Flow,
    Connect,
    SendGroup,
    HexSend,
    EndingLabel,
    Ending,
    ClearTx,
    SendText,
    Send,
    Status,
    Counter,
    Timestamps,
    LogFontLabel,
    LogFont,
    LogFontSize,
    Extensions,
    SaveLog,
    ConnectionModeLabel,
    ConnectionMode,
    RemoteHostLabel,
    RemoteHost,
    RemotePortLabel,
    RemotePort,
    LocalHostLabel,
    LocalHost,
    LocalPortLabel,
    LocalPort,
    NetworkHint,
    BufferLabel,
    BufferSize,
}

impl Id {
    const COUNT: usize = Self::BufferSize as usize - Self::ReceiveLabel as usize + 1;

    fn index(self) -> usize {
        self as usize - Self::ReceiveLabel as usize
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Label,
    RightLabel,
    Button,
    PrimaryButton,
    Check,
    List,
    EditableList,
    Input,
    SingleInput,
    Log,
}

struct Ui {
    window: HWND,
    controls: [HWND; Id::COUNT],
    dpi: u32,
    font: HFONT,
    mono_font: HFONT,
    serial_width: i32,
    lower_height: i32,
    drag: Option<Drag>,
    child_layout: WindowLayout,
    drawn_layout: Cell<Option<PaneLayout>>,
}

impl Ui {
    fn new(window: HWND, instance: HINSTANCE, dpi: u32) -> io::Result<Self> {
        use Id::*;
        use Kind::*;
        let specs = [
            (ReceiveLabel, Label, "收发记录"),
            (HexDisplay, Check, "HEX 显示"),
            (Follow, Check, "自动滚动"),
            (EncodingLabel, Label, "收发编码"),
            (Encoding, List, ""),
            (Timestamps, Check, "时间戳"),
            (LogFontLabel, Label, "字体"),
            (LogFont, List, ""),
            (LogFontSize, List, ""),
            (Extensions, Button, "拓展 ▾"),
            (SaveLog, Button, "保存记录"),
            (ClearRx, Button, "清空记录"),
            (Receive, Log, ""),
            (SerialGroup, Label, "连接设置"),
            (ConnectionModeLabel, Label, "类型"),
            (ConnectionMode, List, ""),
            (RemoteHostLabel, Label, "远端IP"),
            (RemoteHost, SingleInput, ""),
            (RemotePortLabel, Label, "远端口"),
            (RemotePort, SingleInput, ""),
            (LocalHostLabel, Label, "本地IP"),
            (LocalHost, SingleInput, ""),
            (LocalPortLabel, Label, "本地口"),
            (LocalPort, SingleInput, ""),
            (NetworkHint, Label, ""),
            (PortLabel, Label, "端口"),
            (Port, List, ""),
            (Refresh, Button, "刷新"),
            (BaudLabel, Label, "波特率"),
            (Baud, EditableList, ""),
            (DataLabel, Label, "数据位"),
            (DataBits, List, ""),
            (ParityLabel, Label, "校验位"),
            (Parity, List, ""),
            (StopLabel, Label, "停止位"),
            (StopBits, List, ""),
            (FlowLabel, Label, "流控"),
            (Flow, List, ""),
            (Connect, PrimaryButton, "打开串口"),
            (SendGroup, Label, "发送区"),
            (HexSend, Check, "HEX 发送"),
            (EndingLabel, Label, "追加换行"),
            (Ending, List, ""),
            (ClearTx, Button, "清空发送"),
            (SendText, Input, ""),
            (Send, Button, "发送\r\nCtrl+Enter"),
            (Status, Label, ""),
            (Counter, RightLabel, ""),
            (BufferLabel, Label, "缓存"),
            (BufferSize, List, ""),
        ];
        let mut ui = Self {
            window,
            controls: [null_mut(); Id::COUNT],
            dpi,
            font: make_font("Microsoft YaHei UI", dpi),
            mono_font: make_font("Consolas", dpi),
            serial_width: 330,
            lower_height: 210,
            drag: None,
            child_layout: WindowLayout::default(),
            drawn_layout: Cell::new(None),
        };

        for (id, kind, text) in specs {
            let (class, extended_style, style) = match kind {
                Label => (w!("STATIC"), 0, SS_LEFT | SS_ENDELLIPSIS | SS_CENTERIMAGE),
                RightLabel => (w!("STATIC"), 0, SS_RIGHT | SS_CENTERIMAGE),
                Button => (
                    w!("BUTTON"),
                    0,
                    WS_TABSTOP | (BS_PUSHBUTTON | BS_MULTILINE) as u32,
                ),
                PrimaryButton => (w!("BUTTON"), 0, WS_TABSTOP | BS_OWNERDRAW as u32),
                Check => (w!("BUTTON"), 0, WS_TABSTOP | BS_AUTOCHECKBOX as u32),
                List => (
                    w!("COMBOBOX"),
                    0,
                    WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
                ),
                EditableList => (
                    w!("COMBOBOX"),
                    0,
                    WS_TABSTOP | WS_VSCROLL | (CBS_DROPDOWN | CBS_AUTOHSCROLL) as u32,
                ),
                Input => (
                    w!("EDIT"),
                    0,
                    WS_TABSTOP
                        | WS_VSCROLL
                        | WS_BORDER
                        | (ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32,
                ),
                SingleInput => (
                    w!("EDIT"),
                    WS_EX_CLIENTEDGE,
                    WS_TABSTOP | ES_AUTOHSCROLL as u32,
                ),
                Log => (
                    w!("RICHEDIT50W"),
                    0,
                    WS_TABSTOP
                        | WS_VSCROLL
                        | WS_BORDER
                        | (ES_MULTILINE | ES_AUTOVSCROLL | ES_READONLY) as u32,
                ),
            };
            let control = unsafe {
                CreateWindowExW(
                    extended_style,
                    class,
                    wide(text).as_ptr(),
                    WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | style,
                    0,
                    0,
                    10,
                    10,
                    window,
                    id as usize as HMENU,
                    instance,
                    null(),
                )
            };
            if control.is_null() {
                return Err(io::Error::last_os_error());
            }
            ui.controls[id.index()] = control;
            let font = if matches!(kind, Input) {
                ui.mono_font
            } else {
                ui.font
            };
            ui.message(id, WM_SETFONT, font as usize, 0);
        }

        ui.message(Receive, EM_SETBKGNDCOLOR, 0, 0x00ff_ffff);
        ui.message(SendText, EM_SETLIMITTEXT, 65536, 0);
        ui.message(Follow, BM_SETCHECK, BST_CHECKED as usize, 0);
        ui.fill(Encoding, &["UTF-8", "GBK"], Some(0));
        ui.fill(ConnectionMode, &Mode::LABELS, Some(0));
        for id in [RemoteHost, LocalHost] {
            ui.message(id, EM_SETLIMITTEXT, 64, 0);
        }
        for id in [RemotePort, LocalPort] {
            ui.message(id, EM_SETLIMITTEXT, 5, 0);
        }
        ui.fill(
            Baud,
            &[
                "1200", "2400", "4800", "9600", "19200", "38400", "57600", "115200", "230400",
                "460800", "921600", "1000000", "2000000", "3000000",
            ],
            Some(7),
        );
        ui.fill(DataBits, &["5", "6", "7", "8"], Some(3));
        ui.fill(Parity, &["无", "偶", "奇"], Some(0));
        ui.fill(StopBits, &["1", "2"], Some(0));
        ui.fill(Flow, &["无", "RTS/CTS", "XON/XOFF"], Some(0));
        ui.fill(Ending, &["无", "CRLF", "LF", "CR"], Some(0));
        Ok(ui)
    }

    fn init_log_font(&self, preferences: &Preferences) {
        self.fill(
            Id::BufferSize,
            &BUFFER_LABELS,
            Some(
                BUFFER_SIZES
                    .iter()
                    .position(|&size| size == preferences.buffer_size)
                    .unwrap_or(1),
            ),
        );
        self.set_history_capacity(preferences.buffer_size);
        self.fill(
            Id::LogFont,
            &LOG_FONTS.map(|(label, _)| label),
            Some(preferences.log_font),
        );
        let sizes = LOG_FONT_SIZES.map(|size| format!("{size} pt"));
        self.fill(
            Id::LogFontSize,
            &sizes.each_ref().map(String::as_str),
            Some(preferences.log_font_size),
        );
        self.apply_log_font();
    }

    fn apply_log_font(&self) {
        let name = LOG_FONTS[self.selected(Id::LogFont)].1;
        let points = LOG_FONT_SIZES[self.selected(Id::LogFontSize)];
        let mut format = CharFormat {
            size: size_of::<CharFormat>() as u32,
            mask: CFM_FACE | CFM_SIZE,
            height: points * 20,
            ..Default::default()
        };
        for (slot, ch) in format.face.iter_mut().take(31).zip(name.encode_utf16()) {
            *slot = ch;
        }
        let param = (&raw const format) as LPARAM;
        self.message(Id::Receive, EM_SETCHARFORMAT, SCF_ALL, param);
        self.message(Id::Receive, EM_SETCHARFORMAT, 0, param);
        if self.checked(Id::Follow) {
            self.scroll_log();
        }
    }

    fn handle(&self, id: Id) -> HWND {
        self.controls[id.index()]
    }

    fn message(&self, id: Id, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe { SendMessageW(self.handle(id), message, wparam, lparam) }
    }

    fn fill(&self, id: Id, items: &[&str], selected: Option<usize>) {
        self.message(id, CB_RESETCONTENT, 0, 0);
        for item in items {
            self.message(id, CB_ADDSTRING, 0, wide(item).as_ptr() as LPARAM);
        }
        if let Some(index) = selected {
            self.message(id, CB_SETCURSEL, index, 0);
        }
    }

    fn selected(&self, id: Id) -> usize {
        self.message(id, CB_GETCURSEL, 0, 0).max(0) as usize
    }

    fn checked(&self, id: Id) -> bool {
        self.message(id, BM_GETCHECK, 0, 0) == BST_CHECKED as isize
    }

    fn text(&self, id: Id) -> String {
        let control = self.handle(id);
        unsafe {
            let length = GetWindowTextLengthW(control);
            let mut text = vec![0; length as usize + 1];
            let copied = GetWindowTextW(control, text.as_mut_ptr(), length + 1);
            String::from_utf16_lossy(&text[..copied as usize])
        }
    }

    fn set_text(&self, id: Id, text: &str) {
        unsafe { SetWindowTextW(self.handle(id), wide(text).as_ptr()) };
    }

    fn enable(&self, id: Id, enabled: bool) {
        unsafe { EnableWindow(self.handle(id), i32::from(enabled)) };
    }

    fn set_history_capacity(&self, bytes: usize) {
        let records = bytes * HISTORY_BASE_RECORDS / HISTORY_BASE_BYTES;
        self.message(
            Id::Receive,
            EM_EXLIMITTEXT,
            0,
            (bytes * 4 + records * 128) as isize,
        );
    }

    fn show(&self, id: Id, visible: bool) {
        let window = self.handle(id);
        unsafe {
            if (GetWindowLongPtrW(window, GWL_STYLE) as u32 & WS_VISIBLE != 0) != visible {
                ShowWindow(window, if visible { SW_SHOW } else { SW_HIDE });
            }
        }
    }

    fn panes(&self) -> PaneLayout {
        let mut rect = RECT::default();
        unsafe { GetClientRect(self.window, &mut rect) };
        PaneLayout::new(
            self.logical(rect.right),
            self.logical(rect.bottom),
            self.serial_width,
            self.lower_height,
        )
    }

    fn logical(&self, pixels: i32) -> i32 {
        pixels * 96 / self.dpi as i32
    }

    fn logical_point(&self, point: POINT) -> POINT {
        POINT {
            x: self.logical(point.x),
            y: self.logical(point.y),
        }
    }

    fn layout(&self) {
        use Id::*;
        let panes = self.panes();
        let PaneLayout {
            width,
            height,
            serial_width,
            lower_height,
        } = panes;
        let bottom = panes.lower_top();
        let serial_right = MARGIN + serial_width - 13;
        let field_width = (serial_width - 138) / 2;
        let right_column = 89 + field_width;
        let send_x = panes.divider_rect(Divider::Vertical).right;
        let controls = [
            (ReceiveLabel, MARGIN, 10, 62, 26),
            (HexDisplay, 82, 10, 81, 26),
            (Follow, 167, 10, 81, 26),
            (EncodingLabel, 254, 10, 56, 26),
            (Encoding, 312, 10, 70, 150),
            (Timestamps, 390, 10, 69, 26),
            (LogFontLabel, 465, 10, 30, 26),
            (LogFont, 497, 10, 78, 150),
            (LogFontSize, 581, 10, 71, 220),
            (Extensions, 662, 10, 70, 26),
            (SaveLog, width - 196, 10, 82, 26),
            (ClearRx, width - 106, 10, 90, 26),
            (
                Receive,
                MARGIN,
                RECEIVE_TOP,
                width - 2 * MARGIN,
                bottom - DIVIDER_SIZE - RECEIVE_TOP,
            ),
            (SerialGroup, 29, bottom + 2, serial_width - 26, 19),
            (ConnectionModeLabel, 29, bottom + 22, 50, 25),
            (ConnectionMode, 81, bottom + 22, serial_right - 81, 190),
            (RemoteHostLabel, 29, bottom + 52, 50, 25),
            (RemoteHost, 81, bottom + 52, serial_right - 81, 25),
            (RemotePortLabel, 29, bottom + 82, 50, 25),
            (RemotePort, 81, bottom + 82, field_width, 25),
            (LocalPortLabel, right_column, bottom + 82, 50, 25),
            (
                LocalPort,
                right_column + 52,
                bottom + 82,
                serial_right - right_column - 52,
                25,
            ),
            (LocalHostLabel, 29, bottom + 112, 50, 25),
            (LocalHost, 81, bottom + 112, serial_right - 81, 25),
            (NetworkHint, 29, bottom + 142, right_column - 37, 25),
            (PortLabel, 29, bottom + 52, 50, 25),
            (Port, 81, bottom + 52, serial_right - 161, 200),
            (Refresh, serial_right - 72, bottom + 52, 72, 25),
            (BaudLabel, 29, bottom + 82, 50, 25),
            (Baud, 81, bottom + 82, field_width, 225),
            (DataLabel, right_column, bottom + 82, 50, 25),
            (
                DataBits,
                right_column + 52,
                bottom + 82,
                serial_right - right_column - 52,
                150,
            ),
            (ParityLabel, 29, bottom + 112, 50, 25),
            (Parity, 81, bottom + 112, field_width, 150),
            (StopLabel, right_column, bottom + 112, 50, 25),
            (
                StopBits,
                right_column + 52,
                bottom + 112,
                serial_right - right_column - 52,
                150,
            ),
            (FlowLabel, 29, bottom + 142, 50, 25),
            (Flow, 81, bottom + 142, field_width, 150),
            (
                Connect,
                right_column,
                bottom + 140,
                serial_right - right_column,
                29,
            ),
            (SendGroup, send_x + 14, bottom + 2, width - send_x - 44, 19),
            (HexSend, send_x + 14, bottom + 22, 90, 25),
            (EndingLabel, send_x + 114, bottom + 22, 69, 25),
            (Ending, send_x + 185, bottom + 22, 73, 150),
            (ClearTx, width - 120, bottom + 22, 90, 25),
            (
                SendText,
                send_x + 14,
                bottom + 54,
                width - send_x - 144,
                lower_height - 67,
            ),
            (Send, width - 120, bottom + 54, 90, MIN_LOWER_HEIGHT - 67),
            (Status, MARGIN, height - 31, width - 456, 22),
            (BufferLabel, width - 424, height - 31, 36, 22),
            (BufferSize, width - 386, height - 34, 102, 150),
            (Counter, width - 260, height - 31, 244, 22),
        ];
        self.child_layout.apply(
            self.window,
            self.dpi,
            controls.map(|(id, x, y, w, h)| (self.handle(id), x, y, w, h)),
        );
        let previous = self.drawn_layout.replace(Some(panes));
        if previous != Some(panes) {
            for layout in previous.into_iter().chain([panes]) {
                self.invalidate_decorations(layout);
            }
        }
    }

    fn start_drag(&mut self, point: POINT) {
        let point = self.logical_point(point);
        let panes = self.panes();
        if let Some(divider) = panes.divider_at(point) {
            let rect = panes.divider_rect(divider);
            let offset = match divider {
                Divider::Horizontal => point.y - rect.top,
                Divider::Vertical => point.x - rect.left,
            };
            self.drag = Some(Drag { divider, offset });
            unsafe { SetCapture(self.window) };
        }
    }

    fn move_divider(&mut self, point: POINT) {
        let Some(drag) = &self.drag else { return };
        let point = self.logical_point(point);
        let panes = self.panes();
        let (width, height) = match drag.divider {
            Divider::Horizontal => (
                self.serial_width,
                panes.height - FOOTER_HEIGHT - DIVIDER_SIZE - point.y + drag.offset,
            ),
            Divider::Vertical => (point.x - drag.offset - MARGIN, self.lower_height),
        };
        let next = PaneLayout::new(panes.width, panes.height, width, height);
        match drag.divider {
            Divider::Horizontal if self.lower_height != next.lower_height => {
                self.lower_height = next.lower_height
            }
            Divider::Vertical if self.serial_width != next.serial_width => {
                self.serial_width = next.serial_width
            }
            _ => return,
        }
        self.layout();
    }

    fn finish_drag(&mut self) {
        if self.drag.take().is_some() && unsafe { GetCapture() } == self.window {
            unsafe { ReleaseCapture() };
        }
    }

    fn divider_cursor(&self) -> bool {
        let mut point = POINT::default();
        unsafe {
            if GetCursorPos(&mut point) == 0 || ScreenToClient(self.window, &mut point) == 0 {
                return false;
            }
        }
        let divider = self
            .drag
            .as_ref()
            .map(|drag| drag.divider)
            .or_else(|| self.panes().divider_at(self.logical_point(point)));
        let cursor = match divider {
            Some(Divider::Horizontal) => IDC_SIZENS,
            Some(Divider::Vertical) => IDC_SIZEWE,
            None => return false,
        };
        unsafe { SetCursor(LoadCursorW(null_mut(), cursor)) };
        true
    }

    fn paint_panes(&self, dc: HDC) {
        let panes = self.panes();
        for bounds in panes.borders() {
            fill_brush(dc, &self.pixel_rect(bounds), PANEL_BORDER, FrameRect);
        }
        for grip in panes.grips() {
            fill_brush(dc, &self.pixel_rect(grip), DIVIDER_COLOR, FillRect);
        }
    }

    fn pixel_rect(&self, rect: RECT) -> RECT {
        let s = |n| scale(n, self.dpi);
        RECT {
            left: s(rect.left),
            top: s(rect.top),
            right: s(rect.right),
            bottom: s(rect.bottom),
        }
    }

    fn invalidate_decorations(&self, layout: PaneLayout) {
        for border in layout.borders().map(|rect| self.pixel_rect(rect)) {
            for edge in [
                RECT {
                    bottom: border.top + 1,
                    ..border
                },
                RECT {
                    top: border.bottom - 1,
                    ..border
                },
                RECT {
                    right: border.left + 1,
                    ..border
                },
                RECT {
                    left: border.right - 1,
                    ..border
                },
            ] {
                unsafe { InvalidateRect(self.window, &edge, 0) };
            }
        }
        for grip in layout.grips() {
            unsafe { InvalidateRect(self.window, &self.pixel_rect(grip), 0) };
        }
    }

    fn show_log(&self, text: &str, replace: bool) {
        if !replace && text.is_empty() {
            return;
        }
        let follow = self.checked(Id::Follow);
        let mut selection_start = 0u32;
        let mut selection_end = 0u32;
        let first_line = self.message(Id::Receive, EM_GETFIRSTVISIBLELINE, 0, 0);
        if !follow {
            self.message(
                Id::Receive,
                EM_GETSEL,
                (&mut selection_start as *mut u32) as WPARAM,
                (&mut selection_end as *mut u32) as LPARAM,
            );
        }
        if replace {
            self.set_text(Id::Receive, text);
        } else {
            self.message(Id::Receive, EM_SETSEL, i32::MAX as usize, i32::MAX as isize);
            self.message(Id::Receive, EM_REPLACESEL, 0, wide(text).as_ptr() as LPARAM);
        }
        if follow {
            self.scroll_log();
        } else {
            self.message(
                Id::Receive,
                EM_SETSEL,
                selection_start as usize,
                selection_end as isize,
            );
            let current = self.message(Id::Receive, EM_GETFIRSTVISIBLELINE, 0, 0);
            self.message(Id::Receive, EM_LINESCROLL, 0, first_line - current);
        }
    }

    fn scroll_log(&self) {
        self.message(Id::Receive, WM_VSCROLL, SB_BOTTOM as usize, 0);
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.font);
            DeleteObject(self.mono_font);
        }
    }
}

struct App {
    ui: Ui,
    control: Option<ControlWindow>,
    upgrade: Option<UpgradeWindow>,
    session: Option<Connection>,
    connected: bool,
    upgrading: bool,
    pending_send: Option<usize>,
    pending_buffer_size: Option<usize>,
    buffer_update_in_flight: Option<usize>,
    port_name: String,
    history: VecDeque<LogEntry>,
    history_bytes: usize,
    receive_format: ReceiveFormat,
    first_receive_in_session: bool,
    preferences: Preferences,
    network_preferences: network_io::Preferences,
    rx: u64,
    tx: u64,
}

impl App {
    fn new(ui: Ui, preferences: Preferences, network_preferences: network_io::Preferences) -> Self {
        ui.init_log_font(&preferences);
        ui.message(
            Id::ConnectionMode,
            CB_SETCURSEL,
            network_preferences.mode.index(),
            0,
        );
        ui.set_text(Id::RemoteHost, &network_preferences.remote_host);
        ui.set_text(Id::RemotePort, &network_preferences.remote_port);
        ui.set_text(Id::LocalHost, &network_preferences.local_host);
        ui.set_text(Id::LocalPort, &network_preferences.local_port);
        let app = Self {
            ui,
            control: None,
            upgrade: None,
            session: None,
            connected: false,
            upgrading: false,
            pending_send: None,
            pending_buffer_size: None,
            buffer_update_in_flight: None,
            port_name: String::new(),
            history: VecDeque::new(),
            history_bytes: 0,
            receive_format: ReceiveFormat::new(false, UTF_8),
            first_receive_in_session: true,
            preferences,
            network_preferences,
            rx: 0,
            tx: 0,
        };
        if app.mode() == Mode::Serial {
            app.refresh_ports();
        } else {
            app.status("未连接 · 请设置网络地址和端口");
        }
        app.update_controls();
        app.update_counter();
        app
    }

    fn resized(&mut self, size: usize) {
        if size == SIZE_MINIMIZED as usize {
            return;
        }
        self.preferences.maximized = size == SIZE_MAXIMIZED as usize;
        if !self.preferences.maximized {
            let mut client = RECT::default();
            if unsafe { GetClientRect(self.ui.window, &mut client) } != 0 {
                let dpi = self.ui.dpi as i32;
                let rounded = |pixels| (pixels * 96 + dpi / 2) / dpi;
                self.preferences.width = rounded(client.right).max(MIN_CLIENT_WIDTH);
                self.preferences.height = rounded(client.bottom).max(MIN_CLIENT_HEIGHT);
            }
        }
        self.ui.layout();
    }

    fn save_preferences(&mut self) -> io::Result<()> {
        self.preferences.serial_width = self.ui.serial_width;
        self.preferences.lower_height = self.ui.lower_height;
        self.preferences.timestamps = self.ui.checked(Id::Timestamps);
        self.preferences.log_font = self.ui.selected(Id::LogFont);
        self.preferences.log_font_size = self.ui.selected(Id::LogFontSize);
        self.preferences.buffer_size = BUFFER_SIZES[self.ui.selected(Id::BufferSize)];
        self.network_preferences = self.read_network_preferences();
        self.preferences.save()?;
        self.network_preferences.save()
    }

    fn mode(&self) -> Mode {
        Mode::ALL[self.ui.selected(Id::ConnectionMode)]
    }

    fn read_network_preferences(&self) -> network_io::Preferences {
        network_io::Preferences {
            mode: self.mode(),
            remote_host: self.ui.text(Id::RemoteHost),
            remote_port: self.ui.text(Id::RemotePort),
            local_host: self.ui.text(Id::LocalHost),
            local_port: self.ui.text(Id::LocalPort),
        }
    }

    fn encoding(&self) -> &'static Encoding {
        [UTF_8, GBK][self.ui.selected(Id::Encoding)]
    }

    fn refresh_ports(&self) {
        let previous = self.ui.text(Id::Port);
        let mut ports = match serialport::available_ports() {
            Ok(ports) => ports,
            Err(error) => {
                self.status(&format!("未连接 · 刷新端口失败，{error}"));
                return;
            }
        };
        ports.sort_by_key(|info| {
            info.port_name
                .strip_prefix("COM")
                .and_then(|id| id.parse::<u32>().ok())
                .unwrap_or(u32::MAX)
        });
        let names: Vec<_> = ports.iter().map(|info| info.port_name.as_str()).collect();
        let selected = names
            .iter()
            .position(|&name| name == previous)
            .or_else(|| (!names.is_empty()).then_some(0));
        self.ui.fill(Id::Port, &names, selected);
        if names.is_empty() {
            self.status("未连接 · 未找到串口，连接设备后点击刷新");
        } else {
            self.status(&format!("未连接 · 找到 {} 个串口", names.len()));
        }
    }

    fn status(&self, text: &str) {
        self.ui.set_text(Id::Status, text);
        if let Some(control) = &self.control {
            control.status(text);
        }
    }

    fn update_counter(&self) {
        self.ui.set_text(
            Id::Counter,
            &format!("RX {} 字节     TX {} 字节", self.rx, self.tx),
        );
    }

    fn update_controls(&self) {
        let idle = self.session.is_none();
        let mode = self.mode();
        let serial = mode == Mode::Serial;
        self.ui.enable(Id::ConnectionMode, idle);
        for id in [
            Id::PortLabel,
            Id::Port,
            Id::Refresh,
            Id::BaudLabel,
            Id::Baud,
            Id::DataLabel,
            Id::DataBits,
            Id::ParityLabel,
            Id::Parity,
            Id::StopLabel,
            Id::StopBits,
            Id::FlowLabel,
            Id::Flow,
        ] {
            self.ui.show(id, serial);
        }
        for id in [
            Id::RemoteHostLabel,
            Id::RemoteHost,
            Id::RemotePortLabel,
            Id::RemotePort,
            Id::LocalHostLabel,
            Id::LocalHost,
            Id::LocalPortLabel,
            Id::LocalPort,
            Id::NetworkHint,
        ] {
            self.ui.show(id, !serial);
        }
        for id in [Id::RemoteHost, Id::RemotePort] {
            self.ui.enable(id, idle && mode != Mode::TcpServer);
        }
        for id in [Id::LocalHost, Id::LocalPort] {
            self.ui.enable(id, idle && mode != Mode::TcpClient);
        }
        self.ui.set_text(
            Id::NetworkHint,
            match mode {
                Mode::TcpServer => "单客户端监听",
                Mode::Udp => "按报文收发",
                _ => "连接远端设备",
            },
        );
        for id in [
            Id::Port,
            Id::Baud,
            Id::DataBits,
            Id::Parity,
            Id::StopBits,
            Id::Flow,
            Id::Refresh,
        ] {
            self.ui.enable(id, idle);
        }
        let closing = self
            .session
            .as_ref()
            .is_some_and(|session| session.commands.is_none());
        let button = if self.upgrading {
            "升级中…"
        } else if closing {
            "请稍候…"
        } else if !serial {
            match (mode, idle, self.connected) {
                (Mode::TcpServer, true, _) => "开始监听",
                (Mode::TcpServer, false, _) => "停止监听",
                (Mode::Udp, true, _) => "打开 UDP",
                (Mode::Udp, false, _) => "关闭 UDP",
                (_, true, _) => "连接网络",
                (_, false, false) => "取消连接",
                _ => "断开连接",
            }
        } else if idle {
            "打开串口"
        } else if self.connected {
            "关闭串口"
        } else {
            "请稍候…"
        };
        self.ui.set_text(Id::Connect, button);
        self.ui.enable(
            Id::Connect,
            !self.upgrading && !closing && (idle || self.connected || !serial),
        );
        self.ui.enable(
            Id::Send,
            self.connected && self.pending_send.is_none() && !self.upgrading && !self.buffer_busy(),
        );
        self.ui.enable(Id::Ending, !self.ui.checked(Id::HexSend));
        if let Some(control) = &self.control {
            control.set_connection(
                self.connected,
                self.pending_send.is_some() || self.upgrading || self.buffer_busy(),
            );
        }
        if let Some(upgrade) = &self.upgrade {
            upgrade.set_connection(
                serial && self.connected && self.pending_send.is_none() && !self.upgrading,
            );
        }
    }

    fn show_control(&mut self) {
        if self.control.is_none() {
            match ControlWindow::create(self.ui.window, self.ui.dpi) {
                Ok(control) => self.control = Some(control),
                Err(error) => {
                    self.status(&format!("无法打开控制窗口 · {error}"));
                    return;
                }
            }
        }
        let control = self.control.as_ref().unwrap();
        control.set_connection(
            self.connected,
            self.pending_send.is_some() || self.upgrading || self.buffer_busy(),
        );
        control.status(&self.ui.text(Id::Status));
        control.show();
    }

    fn extension_window(&self, extension: Extension) -> Option<HWND> {
        match extension {
            Extension::Control => self.control.as_ref().map(ControlWindow::hwnd),
            Extension::Upgrade => self.upgrade.as_ref().map(UpgradeWindow::hwnd),
        }
    }

    fn show_extension(&mut self, extension: Extension) {
        match extension {
            Extension::Control => self.show_control(),
            Extension::Upgrade => self.show_upgrade(),
        }
    }

    fn toggle_extension(&mut self, extension: Extension) {
        if let Some(window) = self.extension_window(extension)
            && unsafe { IsWindowVisible(window) != 0 && IsIconic(window) == 0 }
        {
            unsafe {
                ShowWindow(window, SW_MINIMIZE);
                SetForegroundWindow(self.ui.window);
                SetFocus(self.ui.handle(Id::SendText));
            }
        } else {
            self.show_extension(extension);
        }
    }

    fn dialog_for_message(&self, window: HWND) -> Option<HWND> {
        EXTENSIONS
            .iter()
            .find_map(|&extension| {
                self.extension_window(extension)
                    .filter(|&dialog| dialog == window || unsafe { IsChild(dialog, window) != 0 })
            })
            .or_else(|| {
                (window == self.ui.window || unsafe { IsChild(self.ui.window, window) != 0 })
                    .then_some(self.ui.window)
            })
    }

    fn minimize_main_shortcut(&self, msg: &MSG) -> bool {
        if msg.message != WM_KEYDOWN || msg.wParam != VK_ESCAPE as usize {
            return false;
        }
        let Some(dialog) = self.dialog_for_message(msg.hwnd) else {
            return false;
        };
        let down = |key| unsafe { GetKeyState(key as i32) < 0 };
        let (alt, ctrl, shift) = (down(VK_MENU), down(VK_CONTROL), down(VK_SHIFT));
        let repeated = msg.lParam & (1 << 30) != 0;
        if msg.wParam == VK_ESCAPE as usize && !alt && !ctrl && !shift && dialog == self.ui.window {
            let extension_visible = EXTENSIONS.iter().any(|&extension| {
                self.extension_window(extension)
                    .is_some_and(|window| unsafe {
                        IsWindowVisible(window) != 0 && IsIconic(window) == 0
                    })
            });
            let dropdown_open = [
                Id::Encoding,
                Id::ConnectionMode,
                Id::BufferSize,
                Id::LogFont,
                Id::LogFontSize,
                Id::Port,
                Id::Baud,
                Id::DataBits,
                Id::Parity,
                Id::StopBits,
                Id::Flow,
                Id::Ending,
            ]
            .into_iter()
            .any(|id| self.ui.message(id, CB_GETDROPPEDSTATE, 0, 0) != 0);
            if !extension_visible && !dropdown_open {
                if !repeated {
                    unsafe { ShowWindow(self.ui.window, SW_MINIMIZE) };
                }
                return true;
            }
        }
        false
    }

    fn send_preset(&mut self) {
        let command = self.control.as_ref().and_then(ControlWindow::take_send);
        if let Some(command) = command {
            if let Some(format) = command.time_format {
                self.queue_send(
                    Command::SendTime {
                        format,
                        ending: command.ending,
                    },
                    format.byte_len(command.ending),
                );
            } else {
                self.send_command(&command.content, command.hex, command.ending.text());
            }
        }
    }

    fn show_upgrade(&mut self) {
        if self.upgrade.is_none() {
            match UpgradeWindow::create(self.ui.window) {
                Ok(upgrade) => self.upgrade = Some(upgrade),
                Err(error) => {
                    self.status(&format!("无法打开 N6 升级窗口 · {error}"));
                    return;
                }
            }
        }
        self.update_controls();
        self.upgrade.as_ref().unwrap().show();
    }

    fn start_upgrade(&mut self) {
        let Some(request) = self.upgrade.as_ref().and_then(UpgradeWindow::take_request) else {
            return;
        };
        let result = if self.mode() != Mode::Serial {
            Err("N6 升级仅支持主窗口串口连接")
        } else if !self.connected {
            Err("请先在主窗口打开串口")
        } else if self.pending_send.is_some() || self.upgrading {
            Err("串口正在发送或升级，请稍候")
        } else if self
            .session
            .as_ref()
            .and_then(|s| s.commands.as_ref())
            .is_some_and(|commands| {
                commands
                    .try_send(Command::Upgrade(Box::new(request)))
                    .is_ok()
            })
        {
            Ok(())
        } else {
            Err("串口线程未接收升级请求，请重试")
        };
        if let Err(error) = result {
            if let Some(upgrade) = &self.upgrade {
                upgrade.event(&UpgradeEvent::Done(Err(error.into())));
            }
            self.status(error);
            return;
        }
        self.upgrading = true;
        self.update_controls();
        self.status(&format!("{} · N6 升级中", self.port_name));
        self.append_log(vec![LogEntry::note(format!(
            "开始 N6 升级 · {} · 115200 / 8N1 / YMODEM",
            self.port_name
        ))]);
    }

    fn toggle(&mut self) {
        if self.upgrading {
            return;
        }
        if let Some(session) = &mut self.session {
            session.commands.take();
            self.connected = false;
            self.update_controls();
            self.status("正在关闭连接…");
            return;
        }

        if self.mode() != Mode::Serial {
            let preferences = self.read_network_preferences();
            let settings = match preferences.settings() {
                Ok(settings) => settings,
                Err(error) => {
                    self.status(&error.to_string());
                    return;
                }
            };
            if unsafe { SetTimer(self.ui.window, POLL_TIMER, 30, None) } == 0 {
                self.status("未连接 · 无法启动网络状态检查");
                return;
            }
            self.port_name = settings.description();
            match network_io::connect(settings, self.preferences.buffer_size) {
                Ok(session) => {
                    self.session = Some(session);
                    self.network_preferences = preferences;
                    self.update_controls();
                    self.status(&format!("正在打开 {}…", self.port_name));
                }
                Err(error) => self.disconnected(&format!("启动网络线程失败，{error}")),
            }
            return;
        }

        let port = self.ui.text(Id::Port);
        if port.is_empty() {
            self.status("未连接 · 请选择串口");
            return;
        }
        let Ok(baud) = self.ui.text(Id::Baud).trim().parse::<u32>() else {
            self.status("未连接 · 请输入有效的整数波特率");
            return;
        };
        if baud == 0 {
            self.status("未连接 · 波特率必须大于 0");
            return;
        }
        let settings = Settings {
            port: port.clone(),
            baud,
            data_bits: [
                DataBits::Five,
                DataBits::Six,
                DataBits::Seven,
                DataBits::Eight,
            ][self.ui.selected(Id::DataBits)],
            parity: [Parity::None, Parity::Even, Parity::Odd][self.ui.selected(Id::Parity)],
            stop_bits: [StopBits::One, StopBits::Two][self.ui.selected(Id::StopBits)],
            flow: [
                FlowControl::None,
                FlowControl::Hardware,
                FlowControl::Software,
            ][self.ui.selected(Id::Flow)],
        };
        if unsafe { SetTimer(self.ui.window, POLL_TIMER, 30, None) } == 0 {
            self.status("未连接 · 无法启动串口状态检查");
            return;
        }
        match serial_io::connect(settings) {
            Ok(session) => {
                self.session = Some(session);
                self.port_name = port;
                self.update_controls();
                self.status(&format!("正在打开 {}…", self.port_name));
            }
            Err(error) => self.disconnected(&format!("启动串口线程失败，{error}")),
        }
    }

    fn send(&mut self) {
        let raw = self.ui.text(Id::SendText);
        let ending = crate::presets::Ending::OPTIONS[self.ui.selected(Id::Ending)].text();
        self.send_command(&raw, self.ui.checked(Id::HexSend), ending);
    }

    fn send_command(&mut self, raw: &str, hex: bool, ending: &str) {
        if !self.connected || self.pending_send.is_some() || self.upgrading {
            return;
        }
        let data = match codec::encode(raw, hex, self.encoding(), ending) {
            Ok(data) if !data.is_empty() => data,
            Ok(_) => {
                self.status(&format!("{} 已连接 · 请输入要发送的内容", self.port_name));
                return;
            }
            Err(error) => {
                self.status(&format!("{} 已连接 · {error}", self.port_name));
                return;
            }
        };
        let length = data.len();
        self.queue_send(Command::Send(data), length);
    }

    fn queue_send(&mut self, command: Command, length: usize) {
        if !self.connected || self.pending_send.is_some() || self.upgrading || self.buffer_busy() {
            return;
        }
        if let Some(commands) = self
            .session
            .as_ref()
            .and_then(|session| session.commands.as_ref())
        {
            if commands.try_send(command).is_ok() {
                self.pending_send = Some(length);
                self.update_controls();
                self.status(&format!(
                    "{} 已连接 · 正在发送 {length} 字节…",
                    self.port_name
                ));
            } else {
                let result = SendResult {
                    requested: length,
                    error: Some("通信线程未接收发送请求".into()),
                };
                let entry = LogEntry::sent(
                    Packet {
                        peer: None,
                        time: serial_io::local_time(),
                        bytes: Vec::new(),
                    },
                    result,
                );
                self.status(&format!("{} · {}", self.port_name, entry.send_summary()));
                self.append_log(vec![entry]);
            }
        }
    }

    fn reset_receive_format(&mut self) {
        let encoding = self.encoding();
        let hex = self.ui.checked(Id::HexDisplay);
        self.receive_format = ReceiveFormat::new(hex, encoding);
        let timestamps = self.ui.checked(Id::Timestamps);
        let mut display = String::new();
        for received in &self.history {
            received.render(
                &mut display,
                &mut self.receive_format,
                encoding,
                hex,
                timestamps,
            );
        }
        self.ui.show_log(&display, true);
    }

    fn clear_receive(&mut self) {
        self.history.clear();
        self.history_bytes = 0;
        self.rx = 0;
        self.tx = 0;
        self.update_counter();
        self.first_receive_in_session = true;
        self.reset_receive_format();
    }

    fn history_limit(&self) -> usize {
        self.preferences.buffer_size
    }

    fn history_retain(&self) -> usize {
        self.history_limit() * 3 / 4
    }

    fn history_record_limit(&self) -> usize {
        self.history_limit() * HISTORY_BASE_RECORDS / HISTORY_BASE_BYTES
    }

    fn change_buffer_size(&mut self) {
        let bytes = BUFFER_SIZES[self.ui.selected(Id::BufferSize)];
        self.preferences.buffer_size = bytes;
        self.ui.set_history_capacity(bytes);
        if self.history_bytes > bytes || self.history.len() > self.history_record_limit() {
            self.trim_history();
            self.reset_receive_format();
        }
        if self.mode() != Mode::Serial && self.session.is_some() {
            self.pending_buffer_size = Some(bytes);
            self.flush_buffer_update();
        }
        self.update_controls();
    }

    fn buffer_busy(&self) -> bool {
        self.pending_buffer_size.is_some() || self.buffer_update_in_flight.is_some()
    }

    fn flush_buffer_update(&mut self) {
        if self.buffer_update_in_flight.is_some() {
            return;
        }
        if let Some(bytes) = self.pending_buffer_size
            && let Some(commands) = self
                .session
                .as_ref()
                .and_then(|session| session.commands.as_ref())
            && commands.try_send(Command::SetReceiveBuffer(bytes)).is_ok()
        {
            self.pending_buffer_size = None;
            self.buffer_update_in_flight = Some(bytes);
        }
    }

    fn poll(&mut self) {
        self.flush_buffer_update();
        let mut batches = Vec::new();
        for _ in 0..32 {
            let Some(session) = &self.session else { break };
            match session.events.try_recv() {
                Ok(Event::Opened) => {
                    self.connected = true;
                    self.rx = 0;
                    self.tx = 0;
                    self.first_receive_in_session = true;
                    self.update_counter();
                    self.update_controls();
                    self.status(&format!(
                        "{} 已连接 · {} bps",
                        self.port_name,
                        self.ui.text(Id::Baud)
                    ));
                }
                Ok(Event::NetworkState {
                    ready,
                    description,
                    reset,
                }) => {
                    self.connected = ready
                        && self
                            .session
                            .as_ref()
                            .is_some_and(|session| session.commands.is_some());
                    if reset {
                        self.rx = 0;
                        self.tx = 0;
                        self.update_counter();
                    }
                    self.first_receive_in_session = true;
                    self.port_name = description;
                    self.update_controls();
                    self.status(&self.port_name);
                }
                Ok(Event::ReceiveBuffer {
                    requested,
                    actual,
                    update,
                }) => {
                    if update && self.buffer_update_in_flight == Some(requested) {
                        self.buffer_update_in_flight = None;
                        self.flush_buffer_update();
                        self.update_controls();
                    }
                    if requested == self.preferences.buffer_size {
                        match actual {
                            Ok(size) if size != requested => self.status(&format!(
                                "接收缓存申请 {} KB，系统实际设置 {} KB",
                                requested / 1024,
                                size / 1024
                            )),
                            Err(error) => self.status(&format!("网络接收缓存设置失败 · {error}")),
                            _ => (),
                        }
                    }
                }
                Ok(Event::Received(packets)) => {
                    for packet in packets {
                        self.rx += packet.bytes.len() as u64;
                        batches.push(LogEntry {
                            packet,
                            send_result: None,
                            annotation: false,
                            datagram: self.mode() == Mode::Udp,
                            omitted: 0,
                            starts_session: std::mem::take(&mut self.first_receive_in_session),
                        });
                    }
                }
                Ok(Event::Sent {
                    packet,
                    requested,
                    error,
                }) => {
                    self.pending_send = None;
                    self.tx += packet.bytes.len() as u64;
                    self.update_controls();
                    let entry = LogEntry::sent(packet, SendResult { requested, error });
                    self.status(&format!("{} · {}", self.port_name, entry.send_summary()));
                    batches.push(entry);
                }
                Ok(Event::Closed(error)) => {
                    self.disconnected(error.as_deref().unwrap_or("连接已关闭"));
                    break;
                }
                Ok(Event::Upgrade(event)) => {
                    if let Some(upgrade) = &self.upgrade {
                        upgrade.event(&event);
                    }
                    match event {
                        UpgradeEvent::Log(text) => batches.push(LogEntry::note(text)),
                        UpgradeEvent::Progress(value, text) => {
                            self.status(&format!("{} · {text}", self.port_name));
                            if value == 0 || value >= 99 {
                                batches.push(LogEntry::note(text));
                            }
                        }
                        UpgradeEvent::TransferCount { received, sent } => {
                            self.rx += received;
                            self.tx += sent;
                            self.update_counter();
                        }
                        UpgradeEvent::Done(result) => {
                            self.upgrading = false;
                            self.first_receive_in_session = true;
                            self.update_controls();
                            let message = match result {
                                Ok(()) => "升级完成，已恢复主窗口收发".to_string(),
                                Err(error) => format!("升级结束 · {error}"),
                            };
                            self.status(&format!("{} · {message}", self.port_name));
                            batches.push(LogEntry::note(message));
                        }
                        UpgradeEvent::Cancelable(_) => (),
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.disconnected("通信线程已退出");
                    break;
                }
            }
        }

        if self.session.is_none()
            && let Some(requested) = self.pending_send.take()
        {
            batches.push(LogEntry::sent(
                Packet {
                    peer: None,
                    time: serial_io::local_time(),
                    bytes: Vec::new(),
                },
                SendResult {
                    requested,
                    error: Some("连接已关闭，未收到写入结果".into()),
                },
            ));
        }
        if !batches.is_empty() {
            self.update_counter();
            self.append_log(batches);
        }
    }

    fn append_log(&mut self, batches: Vec<LogEntry>) {
        self.history_bytes += batches
            .iter()
            .map(|entry| entry.packet.bytes.len())
            .sum::<usize>();
        if self.history_bytes > self.history_limit()
            || self.history.len() + batches.len() > self.history_record_limit()
        {
            self.history.extend(batches);
            self.trim_history();
            self.reset_receive_format();
        } else {
            let timestamps = self.ui.checked(Id::Timestamps);
            let encoding = self.encoding();
            let hex = self.ui.checked(Id::HexDisplay);
            let mut display = String::new();
            for entry in &batches {
                entry.render(
                    &mut display,
                    &mut self.receive_format,
                    encoding,
                    hex,
                    timestamps,
                );
            }
            self.history.extend(batches);
            self.ui.show_log(&display, false);
        }
    }

    fn trim_history(&mut self) {
        let retain = self.history_retain();
        while self.history.len() > self.history_record_limit() * 3 / 4 {
            self.history_bytes -= self.history.pop_front().unwrap().packet.bytes.len();
        }
        while self.history_bytes > retain {
            let front = self.history.front_mut().unwrap();
            let excess = self.history_bytes - retain;
            if front.packet.bytes.len() > excess {
                front.packet.bytes.drain(..excess);
                front.omitted += excess;
                self.history_bytes = retain;
            } else {
                self.history_bytes -= front.packet.bytes.len();
                self.history.pop_front();
            }
        }
    }

    fn disconnected(&mut self, reason: &str) {
        self.session = None;
        self.connected = false;
        self.pending_buffer_size = None;
        self.buffer_update_in_flight = None;
        if self.upgrading {
            self.upgrading = false;
            if let Some(upgrade) = &self.upgrade {
                upgrade.event(&UpgradeEvent::Done(Err(reason.into())));
            }
            self.append_log(vec![LogEntry::note(format!("升级连接中断 · {reason}"))]);
        }
        self.update_controls();
        self.status(&format!("未连接 · {reason}"));
        unsafe { KillTimer(self.ui.window, POLL_TIMER) };
    }

    fn command(&mut self, id: usize, notification: u32) {
        if notification == 1 && (TOGGLE_EXTENSION_BASE..TOGGLE_EXTENSION_BASE + 9).contains(&id) {
            if let Some(&extension) = EXTENSIONS.get(id - TOGGLE_EXTENSION_BASE) {
                self.toggle_extension(extension);
            }
        } else if notification == BN_CLICKED {
            if let Some(extension) = id
                .checked_sub(MENU_EXTENSION_BASE)
                .and_then(|index| EXTENSIONS.get(index))
                .copied()
            {
                self.show_extension(extension);
                return;
            }
            match id {
                x if x == Id::Connect as usize => self.toggle(),
                x if x == Id::Refresh as usize => self.refresh_ports(),
                x if x == Id::Send as usize => self.send(),
                x if x == Id::ClearRx as usize => self.clear_receive(),
                x if x == Id::ClearTx as usize => self.ui.set_text(Id::SendText, ""),
                x if x == Id::HexDisplay as usize || x == Id::Timestamps as usize => {
                    self.reset_receive_format()
                }
                x if x == Id::HexSend as usize => self.update_controls(),
                x if x == Id::Follow as usize && self.ui.checked(Id::Follow) => {
                    self.ui.scroll_log()
                }
                _ => (),
            }
        } else if notification == CBN_SELCHANGE {
            match id {
                x if x == Id::BufferSize as usize => self.change_buffer_size(),
                x if x == Id::ConnectionMode as usize => {
                    if self.session.is_none() {
                        self.update_controls();
                        if self.mode() == Mode::Serial {
                            self.refresh_ports();
                        } else {
                            self.status("未连接 · 请设置网络地址和端口");
                        }
                    }
                }
                x if x == Id::Encoding as usize => self.reset_receive_format(),
                x if x == Id::LogFont as usize || x == Id::LogFontSize as usize => {
                    self.ui.apply_log_font()
                }
                _ => (),
            }
        }
    }
}

struct SendResult {
    requested: usize,
    error: Option<String>,
}

struct LogEntry {
    packet: Packet,
    send_result: Option<SendResult>,
    annotation: bool,
    datagram: bool,
    omitted: usize,
    starts_session: bool,
}

impl LogEntry {
    fn note(text: String) -> Self {
        Self {
            packet: Packet {
                peer: None,
                time: serial_io::local_time(),
                bytes: text.into_bytes(),
            },
            send_result: None,
            annotation: true,
            datagram: false,
            omitted: 0,
            starts_session: false,
        }
    }

    fn sent(packet: Packet, result: SendResult) -> Self {
        Self {
            packet,
            send_result: Some(result),
            annotation: false,
            datagram: false,
            omitted: 0,
            starts_session: false,
        }
    }

    fn send_summary(&self) -> String {
        let result = self.send_result.as_ref().unwrap();
        let written = self.packet.bytes.len() + self.omitted;
        match &result.error {
            None => format!("已写入 {written} 字节"),
            Some(error) => {
                let status = if written == 0 {
                    "写入未确认"
                } else {
                    "部分写入"
                };
                format!("{status} {written}/{} 字节 · {error}", result.requested)
            }
        }
    }

    fn render(
        &self,
        display: &mut String,
        receive_format: &mut ReceiveFormat,
        encoding: &'static Encoding,
        hex: bool,
        timestamps: bool,
    ) {
        use std::fmt::Write;
        if self.starts_session {
            *receive_format = ReceiveFormat::new(hex, encoding);
        }
        let data = if self.datagram && self.packet.bytes.is_empty() {
            "[空报文]".to_string()
        } else if self.annotation {
            String::from_utf8_lossy(&self.packet.bytes).into_owned()
        } else if self.send_result.is_some() || self.datagram {
            ReceiveFormat::new(hex, encoding).format_batch(&self.packet.bytes, true)
        } else {
            receive_format.format_batch(&self.packet.bytes, false)
        };
        if data.is_empty() && self.send_result.is_none() {
            return;
        }
        if timestamps {
            let time = self.packet.time;
            let _ = write!(
                display,
                "[{:02}:{:02}:{:02}.{:03}]",
                time.wHour, time.wMinute, time.wSecond, time.wMilliseconds
            );
        }
        if self.annotation {
            display.push_str("升级◆");
        } else if let Some(result) = &self.send_result {
            display.push_str("发→◇");
            if result.error.is_some() {
                let _ = write!(display, "[{}]", self.send_summary());
                if !data.is_empty() {
                    display.push(' ');
                }
            }
        } else {
            display.push_str("收←◆");
        }
        if let Some(peer) = self.packet.peer {
            let _ = write!(display, "[{peer}] ");
        }
        if self.omitted != 0 {
            let _ = write!(display, "[前 {} 字节已裁剪] ", self.omitted);
        }
        display.push_str(&data);
        if !data.ends_with("\r\n") {
            display.push_str("\r\n");
        }
        display.push_str("\r\n");
    }
}

pub fn run() -> io::Result<()> {
    unsafe {
        SetProcessDPIAware();
        let shortcuts = ExtensionShortcuts::new()?;
        let rich_edit = LoadLibraryW(w!("Msftedit.dll"));
        if rich_edit.is_null() {
            return Err(io::Error::last_os_error());
        }
        let dpi = GetDpiForSystem().max(96);
        let instance = GetModuleHandleW(null());
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hIcon: LoadIconW(null_mut(), IDI_APPLICATION),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: GetSysColorBrush(COLOR_3DFACE),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(io::Error::last_os_error());
        }
        let preferences = Preferences::load();
        let mut size = RECT {
            left: 0,
            top: 0,
            right: scale(preferences.width.max(MIN_CLIENT_WIDTH), dpi),
            bottom: scale(preferences.height.max(MIN_CLIENT_HEIGHT), dpi),
        };
        AdjustWindowRectExForDpi(&mut size, WINDOW_STYLE, 0, WINDOW_EX_STYLE, dpi);
        let mut work_area = RECT::default();
        let has_work_area =
            SystemParametersInfoW(SPI_GETWORKAREA, 0, (&raw mut work_area).cast(), 0) != 0;
        let (mut width, mut height) = (size.right - size.left, size.bottom - size.top);
        let (x, y) = if has_work_area {
            let available_width = work_area.right - work_area.left;
            let available_height = work_area.bottom - work_area.top;
            width = width.min(available_width);
            height = height.min(available_height);
            (
                work_area.left + (available_width - width) / 2,
                work_area.top + (available_height - height) / 2,
            )
        } else {
            (CW_USEDEFAULT, CW_USEDEFAULT)
        };
        let window_title = wide(concat!("串口助手 v", env!("CARGO_PKG_VERSION")));
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE,
            CLASS_NAME,
            window_title.as_ptr(),
            WINDOW_STYLE,
            x,
            y,
            width,
            height,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut ui = match Ui::new(hwnd, instance, dpi) {
            Ok(ui) => ui,
            Err(error) => {
                DestroyWindow(hwnd);
                return Err(error);
            }
        };
        ui.serial_width = preferences.serial_width;
        ui.lower_height = preferences.lower_height;
        ui.message(
            Id::Timestamps,
            BM_SETCHECK,
            usize::from(preferences.timestamps),
            0,
        );
        let rx = ui.handle(Id::Receive);
        let tx = ui.handle(Id::SendText);
        ui.layout();
        let show = if preferences.maximized {
            SW_SHOWMAXIMIZED
        } else {
            SW_SHOWNORMAL
        };
        let app = RefCell::new(App::new(ui, preferences, network_io::Preferences::load()));
        // The state lives until the window is destroyed and the message loop returns.
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &raw const app as isize);
        ShowWindow(hwnd, show);
        SetFocus(tx);
        app.borrow().ui.message(Id::Baud, CB_SETEDITSEL, 0, 0);
        let mut msg = MSG::default();
        loop {
            match GetMessageW(&mut msg, null_mut(), 0, 0) {
                -1 => {
                    let error = io::Error::last_os_error();
                    DestroyWindow(hwnd);
                    return Err(error);
                }
                0 => return Ok(()),
                _ => (),
            }
            if app.borrow().minimize_main_shortcut(&msg) {
                continue;
            }
            let belongs_to_app = app.borrow().dialog_for_message(msg.hwnd).is_some();
            if belongs_to_app && shortcuts.translate(hwnd, &msg) {
                continue;
            }
            if msg.message == WM_KEYDOWN && GetKeyState(VK_CONTROL as i32) < 0 {
                if msg.hwnd == tx && msg.wParam == VK_RETURN as usize {
                    app.borrow_mut().send();
                    continue;
                }
                if (msg.hwnd == rx || msg.hwnd == tx) && msg.wParam == b'A' as usize {
                    SendMessageW(msg.hwnd, EM_SETSEL, 0, -1);
                    continue;
                }
            }
            let dialog = app.borrow().dialog_for_message(msg.hwnd).unwrap_or(hwnd);
            if IsDialogMessageW(dialog, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

fn show_extensions(window: HWND) {
    unsafe {
        let button = GetDlgItem(window, Id::Extensions as i32);
        let mut bounds = RECT::default();
        if GetWindowRect(button, &mut bounds) == 0 {
            return;
        }
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        let ready = EXTENSIONS.iter().enumerate().all(|(index, &extension)| {
            let label = if index < 9 {
                format!("{}\tAlt+{}", extension.name(), index + 1)
            } else {
                extension.name().to_string()
            };
            AppendMenuW(
                menu,
                MF_STRING,
                MENU_EXTENSION_BASE + index,
                wide(&label).as_ptr(),
            ) != 0
        });
        let command = if ready {
            TrackPopupMenu(
                menu,
                TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                bounds.left,
                bounds.bottom,
                0,
                window,
                null(),
            )
        } else {
            0
        };
        DestroyMenu(menu);
        if command != 0 {
            PostMessageW(window, WM_COMMAND, command as usize, 0);
        }
    }
}

fn save_log(window: HWND, state: &RefCell<App>) {
    let Ok(app) = state.try_borrow() else {
        return;
    };
    let snapshot = app.ui.text(Id::Receive);
    if snapshot.is_empty() {
        app.status("当前没有可保存的收发记录");
        return;
    }
    drop(app);

    let now = serial_io::local_time();
    let filename = format!(
        "串口记录_{:04}{:02}{:02}_{:02}{:02}{:02}.txt",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond
    );
    let result = file_dialog::save_as(
        window,
        "保存收发记录",
        &filename,
        "文本文件 (*.txt)",
        "txt",
        snapshot.as_bytes(),
    );
    let message = match result {
        Ok(Some(path)) => format!("记录已保存 · {}", path.display()),
        Ok(None) => return,
        Err(error) => format!("保存记录失败 · {error}"),
    };
    if let Ok(app) = state.try_borrow() {
        app.status(&message);
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_COMMAND if wparam == Id::SaveLog as usize => {
            let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const RefCell<App>;
            if let Some(state) = unsafe { state.as_ref() } {
                save_log(hwnd, state);
            }
            return 0;
        }
        WM_COMMAND if wparam == Id::Extensions as usize => {
            show_extensions(hwnd);
            return 0;
        }
        WM_DRAWITEM if wparam == Id::Connect as usize => {
            let item = unsafe { &*(lparam as *const DRAWITEMSTRUCT) };
            draw_connect_button(item);
            return 1;
        }
        WM_DESTROY => {
            let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const RefCell<App>;
            if let Some(state) = unsafe { state.as_ref() }
                && let Ok(mut app) = state.try_borrow_mut()
            {
                app.control.take();
                app.upgrade.take();
            }
            unsafe {
                KillTimer(hwnd, POLL_TIMER);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                PostQuitMessage(0);
            }
            return 0;
        }
        WM_GETMINMAXINFO => {
            let dpi = unsafe { GetDpiForSystem() }.max(96);
            let info = unsafe { &mut *(lparam as *mut MINMAXINFO) };
            let mut bounds = RECT {
                left: 0,
                top: 0,
                right: scale(MIN_CLIENT_WIDTH, dpi),
                bottom: scale(MIN_CLIENT_HEIGHT, dpi),
            };
            unsafe { AdjustWindowRectExForDpi(&mut bounds, WINDOW_STYLE, 0, WINDOW_EX_STYLE, dpi) };
            info.ptMinTrackSize.x = bounds.right - bounds.left;
            info.ptMinTrackSize.y = bounds.bottom - bounds.top;
            return 0;
        }
        WM_CLOSE => {
            let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const RefCell<App>;
            if let Some(state) = unsafe { state.as_ref() } {
                let upgrade = state.borrow();
                if upgrade
                    .upgrade
                    .as_ref()
                    .is_some_and(|ui| !ui.request_close())
                {
                    return 0;
                }
            }
            if let Some(state) = unsafe { state.as_ref() }
                && let Ok(mut app) = state.try_borrow_mut()
                && let Err(error) = app.save_preferences()
            {
                let message = wide(&format!("无法保存串口助手设置。\r\n{error}"));
                unsafe { MessageBoxW(hwnd, message.as_ptr(), TITLE, MB_OK | MB_ICONWARNING) };
            }
        }
        WM_ERASEBKGND => {
            return 1;
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            unsafe { SetBkColor(wparam as HDC, WORKSPACE_COLOR) };
            unsafe { SetDCBrushColor(wparam as HDC, WORKSPACE_COLOR) };
            return unsafe { GetStockObject(DC_BRUSH) } as LRESULT;
        }
        WM_PAINT | WM_PRINTCLIENT => {
            let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const RefCell<App>;
            if let Some(state) = unsafe { state.as_ref() } {
                let mut paint = PAINTSTRUCT::default();
                let dc = if message == WM_PAINT {
                    unsafe { BeginPaint(hwnd, &mut paint) }
                } else {
                    wparam as HDC
                };
                let bounds = if message == WM_PAINT {
                    paint.rcPaint
                } else {
                    let mut rect = RECT::default();
                    unsafe { GetClientRect(hwnd, &mut rect) };
                    rect
                };
                fill_brush(dc, &bounds, WORKSPACE_COLOR, FillRect);
                if let Ok(app) = state.try_borrow() {
                    app.ui.paint_panes(dc);
                }
                if message == WM_PAINT {
                    unsafe { EndPaint(hwnd, &paint) };
                }
                return 0;
            }
        }
        WM_SETCURSOR if lparam & 0xffff == HTCLIENT as isize && wparam == hwnd as usize => {
            let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const RefCell<App>;
            if let Some(state) = unsafe { state.as_ref() }
                && let Ok(app) = state.try_borrow()
                && app.ui.divider_cursor()
            {
                return 1;
            }
        }
        WM_SIZE
        | WM_COMMAND
        | control::SEND_MESSAGE
        | upgrade::START_MESSAGE
        | WM_TIMER
        | WM_LBUTTONDOWN
        | WM_MOUSEMOVE
        | WM_LBUTTONUP
        | WM_CAPTURECHANGED
        | WM_CANCELMODE => {
            let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const RefCell<App>;
            if let Some(state) = unsafe { state.as_ref() } {
                // Changing a child control can send a synchronous notification back to the parent.
                if let Ok(mut app) = state.try_borrow_mut() {
                    match message {
                        WM_SIZE => app.resized(wparam),
                        WM_COMMAND => app.command(wparam & 0xffff, (wparam >> 16) as u32 & 0xffff),
                        control::SEND_MESSAGE => app.send_preset(),
                        upgrade::START_MESSAGE => app.start_upgrade(),
                        WM_TIMER if wparam == POLL_TIMER => app.poll(),
                        WM_LBUTTONDOWN => app.ui.start_drag(mouse_point(lparam)),
                        WM_MOUSEMOVE => app.ui.move_divider(mouse_point(lparam)),
                        WM_LBUTTONUP => {
                            app.ui.move_divider(mouse_point(lparam));
                            app.ui.finish_drag();
                        }
                        WM_CAPTURECHANGED => app.ui.drag = None,
                        WM_CANCELMODE => app.ui.finish_drag(),
                        _ => (),
                    }
                }
            }
            if message != WM_CANCELMODE {
                return 0;
            }
        }
        _ => (),
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn mouse_point(lparam: LPARAM) -> POINT {
    POINT {
        x: lparam as i16 as i32,
        y: (lparam >> 16) as i16 as i32,
    }
}

fn fill_brush(
    dc: HDC,
    bounds: &RECT,
    color: COLORREF,
    draw: unsafe extern "system" fn(HDC, *const RECT, HBRUSH) -> i32,
) {
    unsafe {
        let previous = SetDCBrushColor(dc, color);
        draw(dc, bounds, GetStockObject(DC_BRUSH));
        SetDCBrushColor(dc, previous);
    }
}

fn draw_connect_button(item: &DRAWITEMSTRUCT) {
    let disabled = item.itemState & ODS_DISABLED != 0;
    let pressed = item.itemState & ODS_SELECTED != 0;
    let (background, foreground) = match (disabled, pressed) {
        (true, _) => (COLOR_3DLIGHT, COLOR_GRAYTEXT),
        (false, true) => (COLOR_HOTLIGHT, COLOR_HIGHLIGHTTEXT),
        (false, false) => (COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT),
    };
    let mut bounds = item.rcItem;
    let mut caption = [0u16; 32];
    unsafe {
        let dc = item.hDC;
        let saved = SaveDC(dc);
        if saved == 0 {
            return;
        }
        let font = SendMessageW(item.hwndItem, WM_GETFONT, 0, 0) as HFONT;
        if !font.is_null() {
            SelectObject(dc, font);
        }
        let radius = scale(6, GetDpiForWindow(item.hwndItem));
        let shape = CreateRoundRectRgn(
            bounds.left,
            bounds.top,
            bounds.right,
            bounds.bottom,
            radius,
            radius,
        );
        if shape.is_null() {
            FillRect(dc, &bounds, GetSysColorBrush(background));
        } else {
            let corners = CreateRectRgnIndirect(&bounds);
            if !corners.is_null() {
                // Paint only the corner pixels with the parent color so the blue face
                // never gets erased before drawing the button again.
                CombineRgn(corners, corners, shape, RGN_DIFF);
                let previous = SetDCBrushColor(dc, WORKSPACE_COLOR);
                FillRgn(dc, corners, GetStockObject(DC_BRUSH));
                SetDCBrushColor(dc, previous);
                DeleteObject(corners);
            }
            FillRgn(dc, shape, GetSysColorBrush(background));
            DeleteObject(shape);
        }
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, GetSysColor(foreground));
        let length = GetWindowTextW(item.hwndItem, caption.as_mut_ptr(), caption.len() as i32);
        DrawTextW(
            dc,
            caption.as_mut_ptr(),
            length,
            &mut bounds,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        if item.itemState & ODS_FOCUS != 0 && item.itemState & ODS_NOFOCUSRECT == 0 {
            InflateRect(&mut bounds, -3, -3);
            DrawFocusRect(dc, &bounds);
        }
        RestoreDC(dc, saved);
    }
}

pub(crate) fn scale(value: i32, dpi: u32) -> i32 {
    (value * dpi as i32 + 48) / 96
}

pub(crate) fn make_font(name: &str, dpi: u32) -> HFONT {
    unsafe {
        CreateFontW(
            -scale(13, dpi),
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            DEFAULT_PITCH as u32,
            wide(name).as_ptr(),
        )
    }
}

pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

pub fn startup_error(error: &io::Error) {
    let text = wide(&format!("无法启动串口助手。\r\n{error}"));
    unsafe { MessageBoxW(null_mut(), text.as_ptr(), TITLE, MB_OK | MB_ICONERROR) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn native_alt_shortcuts_and_escape_preserve_windows() {
        unsafe {
            let instance = GetModuleHandleW(null());
            let rich_edit = LoadLibraryW(w!("Msftedit.dll"));
            let class_name = w!("RustSerialAssistantShortcutTest");
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: class_name,
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let host = CreateWindowExW(
                0,
                class_name,
                w!("Shortcut test"),
                WINDOW_STYLE,
                0,
                0,
                940,
                610,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            assert!(!host.is_null() && !rich_edit.is_null());
            let app = RefCell::new(App::new(
                Ui::new(host, instance, 96).unwrap(),
                Preferences::default(),
                network_io::Preferences::default(),
            ));
            SetWindowLongPtrW(host, GWLP_USERDATA, &raw const app as isize);
            let shortcuts = ExtensionShortcuts::new().unwrap();
            let mut previous = [0; 256];
            assert_ne!(GetKeyboardState(previous.as_mut_ptr()), 0);
            let key = |target, digit, repeat, alt| {
                let mut keyboard = [0; 256];
                keyboard[if alt { VK_MENU } else { VK_CONTROL } as usize] = 0x80;
                assert_ne!(SetKeyboardState(keyboard.as_ptr()), 0);
                let message = MSG {
                    hwnd: target,
                    message: if alt { WM_SYSKEYDOWN } else { WM_KEYDOWN },
                    wParam: digit as usize,
                    lParam: 1 | if alt { 1 << 29 } else { 0 } | if repeat { 1 << 30 } else { 0 },
                    ..Default::default()
                };
                shortcuts.translate(host, &message)
            };
            let main_input = app.borrow().ui.handle(Id::SendText);
            assert!(!key(main_input, b'1', false, false));
            assert!(key(main_input, b'3', false, true));
            assert!(app.borrow().control.is_none() && app.borrow().upgrade.is_none());
            assert!(key(main_input, b'1', false, true));
            let control = app.borrow().control.as_ref().unwrap().hwnd();
            let control_input = GetDlgItem(control, 102);
            SetWindowTextW(control_input, w!("preserve edited name"));
            assert!(IsWindowVisible(control) != 0 && IsIconic(control) == 0);
            assert!(key(control_input, b'1', false, true));
            assert_ne!(IsIconic(control), 0);
            assert!(key(main_input, b'1', true, true));
            assert_ne!(IsIconic(control), 0);
            assert!(key(main_input, b'1', false, true));
            assert_eq!(IsIconic(control), 0);
            assert_eq!(app.borrow().control.as_ref().unwrap().hwnd(), control);
            let mut text = [0; 64];
            let len = GetWindowTextW(control_input, text.as_mut_ptr(), text.len() as i32);
            assert_eq!(
                String::from_utf16_lossy(&text[..len as usize]),
                "preserve edited name"
            );
            assert!(key(control_input, b'2', false, true));
            let upgrade = app.borrow().upgrade.as_ref().unwrap().hwnd();
            let upgrade_input = GetDlgItem(upgrade, 106);
            SetWindowTextW(upgrade_input, w!("firmware.bin"));
            assert!(key(upgrade_input, b'2', false, true));
            assert_ne!(IsIconic(upgrade), 0);
            assert!(key(main_input, b'2', false, true));
            assert_eq!(IsIconic(upgrade), 0);
            let len = GetWindowTextW(upgrade_input, text.as_mut_ptr(), text.len() as i32);
            assert_eq!(
                String::from_utf16_lossy(&text[..len as usize]),
                "firmware.bin"
            );
            assert_ne!(SetKeyboardState([0; 256].as_ptr()), 0);
            let escape = MSG {
                hwnd: main_input,
                message: WM_KEYDOWN,
                wParam: VK_ESCAPE as usize,
                ..Default::default()
            };
            assert!(!app.borrow().minimize_main_shortcut(&escape));
            assert!(key(upgrade_input, b'1', false, true));
            assert!(key(main_input, b'2', false, true));
            assert_ne!(SetKeyboardState([0; 256].as_ptr()), 0);
            assert!(app.borrow().minimize_main_shortcut(&escape));
            assert_ne!(IsIconic(host), 0);
            assert_ne!(IsWindow(host), 0);
            ShowWindow(host, SW_RESTORE);
            SendMessageW(control, WM_CLOSE, 0, 0);
            SendMessageW(upgrade, WM_CLOSE, 0, 0);
            assert!(key(main_input, b'1', false, true));
            assert_eq!(app.borrow().control.as_ref().unwrap().hwnd(), control);
            SendMessageW(control, WM_CLOSE, 0, 0);
            assert_ne!(SetKeyboardState([0; 256].as_ptr()), 0);
            assert!(app.borrow().minimize_main_shortcut(&escape));
            assert_ne!(IsIconic(host), 0);
            assert_ne!(IsWindow(host), 0);
            SetKeyboardState(previous.as_ptr());
            DestroyWindow(host);
            UnregisterClassW(class_name, instance);
            FreeLibrary(rich_edit);
        }
    }

    #[test]
    fn native_controls_send_and_display_a_simulated_echo() {
        let (host, rich_edit, instance) = unsafe {
            let rich_edit = LoadLibraryW(w!("Msftedit.dll"));
            let instance = GetModuleHandleW(null());
            let host = CreateWindowExW(
                0,
                w!("STATIC"),
                w!(""),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                940,
                610,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            (host, rich_edit, instance)
        };
        assert!(!host.is_null() && !rich_edit.is_null());
        let mut app = App::new(
            Ui::new(host, instance, 96).unwrap(),
            Preferences::default(),
            network_io::Preferences::default(),
        );
        app.ui.layout();
        let (commands, command_rx) = mpsc::sync_channel(1);
        let (event_tx, events) = mpsc::sync_channel(32);
        app.session = Some(Connection {
            commands: Some(commands),
            events,
        });
        app.port_name = "回环".into();
        event_tx.send(Event::Opened).unwrap();
        app.poll();

        app.ui.set_text(Id::SendText, "串口");
        app.ui.message(Id::Ending, CB_SETCURSEL, 1, 0);
        app.send();
        let Command::Send(data) = command_rx.try_recv().unwrap() else {
            panic!("expected send");
        };
        assert_eq!(data, "串口\r\n".as_bytes());
        assert!(app.ui.text(Id::Receive).is_empty());
        let first = SYSTEMTIME {
            wHour: 9,
            wMinute: 8,
            wSecond: 7,
            wMilliseconds: 6,
            ..Default::default()
        };
        let second = SYSTEMTIME {
            wSecond: 8,
            ..first
        };
        event_tx
            .send(Event::Sent {
                packet: Packet {
                    peer: None,
                    time: first,
                    bytes: data.clone(),
                },
                requested: data.len(),
                error: None,
            })
            .unwrap();
        event_tx
            .send(Event::Received(vec![Packet {
                peer: None,
                time: first,
                bytes: data[..2].to_vec(),
            }]))
            .unwrap();
        app.poll();
        event_tx
            .send(Event::Received(vec![Packet {
                peer: None,
                time: second,
                bytes: data[2..].to_vec(),
            }]))
            .unwrap();
        app.poll();
        let log = app.ui.text(Id::Receive);
        assert_eq!(log.replace("\r\n", "\n"), "发→◇串口\n\n收←◆串口\n\n");
        assert_eq!(app.ui.text(Id::Counter), "RX 8 字节     TX 8 字节");

        app.ui
            .message(Id::Timestamps, BM_SETCHECK, BST_CHECKED as usize, 0);
        app.command(Id::Timestamps as usize, BN_CLICKED);
        assert!(app.ui.text(Id::Receive).contains("[09:08:08.006]收←◆串口"));

        app.ui
            .message(Id::HexDisplay, BM_SETCHECK, BST_CHECKED as usize, 0);
        app.reset_receive_format();
        let timestamped = app.ui.text(Id::Receive);
        assert!(timestamped.starts_with("[09:08:07.006]发→◇E4 B8 B2 E5 8F A3 0D 0A"));
        assert!(timestamped.contains("[09:08:07.006]收←◆E4 B8"));
        assert!(timestamped.contains("[09:08:08.006]收←◆B2 E5 8F A3 0D 0A"));
        assert_eq!(app.ui.text(Id::Counter), "RX 8 字节     TX 8 字节");
        app.ui
            .message(Id::Timestamps, BM_SETCHECK, BST_UNCHECKED as usize, 0);
        app.command(Id::Timestamps as usize, BN_CLICKED);
        assert!(app.ui.text(Id::Receive).starts_with("发→◇"));

        let before = app.ui.text(Id::Receive);
        app.ui.message(Id::LogFont, CB_SETCURSEL, 1, 0);
        app.ui.message(Id::LogFontSize, CB_SETCURSEL, 6, 0);
        app.command(Id::LogFontSize as usize, CBN_SELCHANGE);
        assert_eq!(app.ui.text(Id::Receive), before);
        let mut format = CharFormat {
            size: size_of::<CharFormat>() as u32,
            ..Default::default()
        };
        // EM_GETCHARFORMAT with SCF_SELECTION at the first character.
        app.ui.message(Id::Receive, EM_SETSEL, 0, 1);
        app.ui
            .message(Id::Receive, WM_USER + 58, 1, (&raw mut format) as LPARAM);
        assert_eq!(format.height, 18 * 20);
        assert!(String::from_utf16_lossy(&format.face).starts_with("DengXian"));

        for written in [vec![0xAA], Vec::new()] {
            event_tx
                .send(Event::Sent {
                    packet: Packet {
                        peer: None,
                        time: second,
                        bytes: written,
                    },
                    requested: 2,
                    error: Some("模拟超时".into()),
                })
                .unwrap();
        }
        app.poll();
        let log = app.ui.text(Id::Receive);
        assert!(log.contains("发→◇[部分写入 1/2 字节 · 模拟超时] AA"));
        assert!(log.contains("发→◇[写入未确认 0/2 字节 · 模拟超时]"));
        assert_eq!(app.ui.text(Id::Counter), "RX 8 字节     TX 9 字节");
        event_tx
            .send(Event::Received(vec![Packet {
                peer: None,
                time: second,
                bytes: vec![0x55; app.history_limit()],
            }]))
            .unwrap();
        app.poll();
        assert_eq!(app.history_bytes, app.history_retain());
        assert!(app.ui.text(Id::Receive).trim_end().ends_with("55 55"));
        app.clear_receive();
        assert!(app.ui.text(Id::Receive).is_empty());
        assert_eq!((app.rx, app.tx), (0, 0));

        event_tx
            .send(Event::Received(vec![Packet {
                peer: None,
                time: first,
                bytes: b"old".to_vec(),
            }]))
            .unwrap();
        event_tx
            .send(Event::Closed(Some("模拟断开".into())))
            .unwrap();
        app.poll();
        assert!(app.session.is_none());
        assert!(!app.connected);

        let (event_tx, events) = mpsc::sync_channel(4);
        app.session = Some(Connection {
            commands: None,
            events,
        });
        event_tx.send(Event::Opened).unwrap();
        let before = app.ui.text(Id::Receive);
        assert!(before.contains("收←◆6F 6C 64"));
        app.poll();
        assert_eq!(app.ui.text(Id::Receive), before);
        assert_eq!((app.rx, app.tx), (0, 0));
        event_tx
            .send(Event::Received(vec![Packet {
                peer: None,
                time: second,
                bytes: b"new".to_vec(),
            }]))
            .unwrap();
        app.poll();
        let after = app.ui.text(Id::Receive);
        assert!(after.starts_with(&before));
        assert!(after.contains("收←◆6E 65 77"));

        app.ui.show_log(&"一条记录\r\n\r\n".repeat(200), true);
        let first_visible = |ui: &Ui| ui.message(Id::Receive, EM_GETFIRSTVISIBLELINE, 0, 0);
        assert!(first_visible(&app.ui) > 0);
        app.ui
            .message(Id::Follow, BM_SETCHECK, BST_UNCHECKED as usize, 0);
        app.ui.message(Id::Receive, WM_VSCROLL, SB_TOP as usize, 0);
        app.ui.show_log("新记录\r\n", false);
        assert_eq!(first_visible(&app.ui), 0);
        app.ui
            .message(Id::Follow, BM_SETCHECK, BST_CHECKED as usize, 0);
        app.command(Id::Follow as usize, BN_CLICKED);
        assert!(first_visible(&app.ui) > 0);
        let previous = first_visible(&app.ui);
        app.ui.show_log(&"后续记录\r\n".repeat(15), false);
        assert!(first_visible(&app.ui) > previous);

        let (commands, command_rx) = mpsc::sync_channel(1);
        app.session.as_mut().unwrap().commands = Some(commands);
        let mut presets = crate::presets::Presets::default();
        presets.profiles[0].commands = vec![
            crate::presets::Preset {
                name: "文本".into(),
                time_format: None,
                content: "AT?".into(),
                hex: false,
                ending: crate::presets::Ending::CrLf,
            },
            crate::presets::Preset {
                name: "HEX".into(),
                time_format: None,
                content: "A1 B2 00".into(),
                hex: true,
                ending: crate::presets::Ending::None,
            },
        ];
        let control = ControlWindow::with_presets(host, 96, presets).unwrap();
        let side = control.hwnd();
        app.control = Some(control);
        app.update_controls();
        for (id, expected) in [(200, b"AT?\r\n".as_slice()), (201, b"\xa1\xb2\0")] {
            let button = unsafe { GetDlgItem(side, id) };
            assert_ne!(unsafe { IsWindowEnabled(button) }, 0);
            unsafe { SendMessageW(button, BM_CLICK, 0, 0) };
            app.send_preset();
            let Command::Send(actual) = command_rx.try_recv().unwrap() else {
                panic!("expected send");
            };
            assert_eq!(actual, expected);
            assert_eq!(unsafe { IsWindowEnabled(button) }, 0);
            event_tx
                .send(Event::Sent {
                    packet: Packet {
                        peer: None,
                        time: second,
                        bytes: expected.to_vec(),
                    },
                    requested: expected.len(),
                    error: None,
                })
                .unwrap();
            app.poll();
        }
        assert_eq!(app.ui.text(Id::SendText), "串口");
        assert!(app.ui.text(Id::Receive).contains("发→◇A1 B2 00"));
        app.reset_receive_format();
        app.show_upgrade();
        let upgrade_hwnd = app.upgrade.as_ref().unwrap().hwnd();
        for removed in [100, 101, 111] {
            assert!(unsafe { GetDlgItem(upgrade_hwnd, removed) }.is_null());
        }
        let enabled = |id| unsafe { IsWindowEnabled(GetDlgItem(upgrade_hwnd, id)) != 0 };
        assert!(enabled(108));
        assert!(!enabled(104));
        assert!(!enabled(110));
        let fixture = std::env::temp_dir().join(format!(
            "serial-assistant-upgrade-test-{}.bin",
            std::process::id()
        ));
        std::fs::write(&fixture, crate::upgrade_protocol::tests::image_bytes(true)).unwrap();
        unsafe { SetDlgItemTextW(upgrade_hwnd, 106, wide(&fixture.to_string_lossy()).as_ptr()) };
        for success in [true, false] {
            unsafe { SendMessageW(GetDlgItem(upgrade_hwnd, 108), BM_CLICK, 0, 0) };
            app.start_upgrade();
            let Command::Upgrade(request) = command_rx.try_recv().unwrap() else {
                panic!("expected shared upgrade");
            };
            assert!(app.upgrading && app.connected);
            assert!(!enabled(108) && enabled(110));
            assert_eq!(unsafe { IsWindowEnabled(app.ui.handle(Id::Send)) }, 0);
            assert_eq!(unsafe { IsWindowEnabled(app.ui.handle(Id::Connect)) }, 0);
            assert_eq!(unsafe { IsWindowEnabled(GetDlgItem(side, 200)) }, 0);
            app.send();
            app.toggle();
            assert!(app.session.as_ref().unwrap().commands.is_some());
            assert!(command_rx.try_recv().is_err());
            event_tx
                .send(Event::Upgrade(UpgradeEvent::Log("板端日志保持中文".into())))
                .unwrap();
            event_tx
                .send(Event::Upgrade(UpgradeEvent::Progress(
                    99,
                    "等待安装".into(),
                )))
                .unwrap();
            app.poll();
            assert!(app.ui.text(Id::Receive).contains("升级◆板端日志保持中文"));
            assert!(app.ui.text(Id::Receive).contains("升级◆等待安装"));
            if success {
                event_tx
                    .send(Event::Upgrade(UpgradeEvent::Cancelable(false)))
                    .unwrap();
                app.poll();
                assert!(!enabled(110));
                assert!(!app.upgrade.as_ref().unwrap().request_close());
                assert!(!request.cancel.load(std::sync::atomic::Ordering::Relaxed));
            } else {
                assert!(!app.upgrade.as_ref().unwrap().request_close());
                assert!(request.cancel.load(std::sync::atomic::Ordering::Relaxed));
            }
            let (rx, tx) = (app.rx, app.tx);
            event_tx
                .send(Event::Upgrade(UpgradeEvent::TransferCount {
                    received: 123,
                    sent: 456,
                }))
                .unwrap();
            event_tx
                .send(Event::Upgrade(UpgradeEvent::Done(if success {
                    Ok(())
                } else {
                    Err("已停止发送".into())
                })))
                .unwrap();
            app.poll();
            assert_eq!((app.rx, app.tx), (rx + 123, tx + 456));
            assert!(!app.upgrading && app.connected);
            assert!(enabled(108) && !enabled(110));
            assert!(app.upgrade.as_ref().unwrap().request_close());
            assert_ne!(unsafe { IsWindowEnabled(app.ui.handle(Id::Send)) }, 0);
            assert_ne!(unsafe { IsWindowEnabled(GetDlgItem(side, 200)) }, 0);
            let before = app.ui.text(Id::Receive);
            app.reset_receive_format();
            assert_eq!(app.ui.text(Id::Receive), before);
        }
        std::fs::remove_file(fixture).unwrap();
        let before = app.ui.text(Id::Receive);
        app.disconnected("测试断开");
        assert!(!enabled(108));
        assert_eq!(app.ui.text(Id::Receive), before);
        app.upgrade.take();
        app.control.take();
        unsafe {
            DestroyWindow(host);
            FreeLibrary(rich_edit);
        }
    }
}
