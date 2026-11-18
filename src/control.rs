use crate::command_editor;
use crate::file_dialog;
use crate::presets::{Preset, Presets, Profile};
use crate::window::{make_font, scale, wide};
use crate::window_layout::WindowLayout;
use std::cell::RefCell;
use std::io;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemServices::{SS_CENTERIMAGE, SS_ENDELLIPSIS};
use windows_sys::Win32::UI::Controls::EM_SETLIMITTEXT;
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

pub const SEND_MESSAGE: u32 = WM_APP + 1;
const CLASS: windows_sys::core::PCWSTR = w!("RustSerialAssistantControl");
const TITLE: windows_sys::core::PCWSTR = w!("拓展 · 控制");
const STYLE: u32 = WS_CAPTION | WS_SYSMENU | WS_THICKFRAME | WS_CLIPCHILDREN | WS_MINIMIZEBOX;
const EX_STYLE: u32 = WS_EX_CONTROLPARENT | WS_EX_COMPOSITED;
const GRID_TOP: i32 = 124;
const ROW_HEIGHT: i32 = 40;
const FOOTER_HEIGHT: i32 = 84;
const VISIBLE_BUTTONS: usize = 20;
const FIRST_SEND: usize = 200;

#[derive(Clone, Copy)]
#[repr(usize)]
enum Id {
    ProfileLabel = 100,
    Profile,
    ProfileName,
    ProfileAdd,
    ProfileRename,
    ProfileDelete,
    GridLabel,
    GridEmpty,
    PagePrev,
    PageNumber,
    PageNext,
    EditCommand,
    AddCommand,
    Status,
    SaveProfile,
    ExportProfiles,
    ImportProfiles,
}

impl Id {
    const COUNT: usize = Self::ImportProfiles as usize - Self::ProfileLabel as usize + 1;

    fn index(self) -> usize {
        self as usize - Self::ProfileLabel as usize
    }
}

pub struct ControlWindow {
    state: Box<RefCell<Control>>,
}

impl ControlWindow {
    pub fn create(owner: HWND, dpi: u32) -> io::Result<Self> {
        Self::with_presets(owner, dpi, Presets::load()?)
    }

    pub(crate) fn with_presets(owner: HWND, dpi: u32, presets: Presets) -> io::Result<Self> {
        unsafe {
            let instance = GetModuleHandleW(null());
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: GetSysColorBrush(COLOR_WINDOW),
                lpszClassName: CLASS,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                return Err(io::Error::last_os_error());
            }
            let mut bounds = window_bounds(414, 610, dpi);
            let width = bounds.right - bounds.left;
            let height = bounds.bottom - bounds.top;
            GetWindowRect(owner, &mut bounds);
            let mut monitor = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            let has_work_area = GetMonitorInfoW(
                MonitorFromWindow(owner, MONITOR_DEFAULTTONEAREST),
                &mut monitor,
            ) != 0;
            let (x, y, height) = if has_work_area {
                let work = monitor.rcWork;
                let height = height.min(work.bottom - work.top);
                let preferred_x = if bounds.right + width <= work.right {
                    bounds.right
                } else {
                    bounds.right - width
                };
                (
                    preferred_x.clamp(work.left, (work.right - width).max(work.left)),
                    bounds
                        .top
                        .clamp(work.top, (work.bottom - height).max(work.top)),
                    height,
                )
            } else {
                (bounds.right, bounds.top, height)
            };
            let hwnd = CreateWindowExW(
                EX_STYLE,
                CLASS,
                TITLE,
                STYLE,
                x,
                y,
                width,
                height,
                owner,
                null_mut(),
                instance,
                null(),
            );
            if hwnd.is_null() {
                return Err(io::Error::last_os_error());
            }
            let control = match Control::new(hwnd, owner, dpi, instance, presets) {
                Ok(control) => control,
                Err(error) => {
                    DestroyWindow(hwnd);
                    return Err(error);
                }
            };
            let state = Box::new(RefCell::new(control));
            SetWindowLongPtrW(
                hwnd,
                GWLP_USERDATA,
                (&*state as *const RefCell<Control>) as isize,
            );
            Ok(Self { state })
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.state.borrow().hwnd
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

    pub fn set_connection(&self, connected: bool, busy: bool) {
        let mut state = self.state.borrow_mut();
        state.connected = connected;
        state.busy = busy;
        state.refresh_buttons();
    }

    pub fn take_send(&self) -> Option<Preset> {
        self.state.borrow_mut().pending.take()
    }

    pub fn status(&self, message: &str) {
        self.state.borrow().set(Id::Status, message);
    }
}

impl Drop for ControlWindow {
    fn drop(&mut self) {
        let hwnd = self.hwnd();
        unsafe {
            if IsWindow(hwnd) != 0
                && GetWindowLongPtrW(hwnd, GWLP_USERDATA)
                    == (&*self.state as *const RefCell<Control>) as isize
            {
                DestroyWindow(hwnd);
            }
        }
    }
}

struct Control {
    hwnd: HWND,
    owner: HWND,
    dpi: u32,
    font: HFONT,
    controls: [HWND; Id::COUNT],
    buttons: [HWND; VISIBLE_BUTTONS],
    presets: Presets,
    page: usize,
    page_size: usize,
    pending: Option<Preset>,
    connected: bool,
    busy: bool,
    child_layout: WindowLayout,
}

impl Control {
    fn new(
        hwnd: HWND,
        owner: HWND,
        dpi: u32,
        instance: HINSTANCE,
        presets: Presets,
    ) -> io::Result<Self> {
        use Id::*;
        let mut ui = Self {
            hwnd,
            owner,
            dpi,
            font: make_font("Microsoft YaHei UI", dpi),
            controls: [null_mut(); Id::COUNT],
            buttons: [null_mut(); VISIBLE_BUTTONS],
            presets,
            page: 0,
            page_size: 2,
            pending: None,
            connected: false,
            busy: false,
            child_layout: WindowLayout::default(),
        };
        let label = (w!("STATIC"), SS_CENTERIMAGE | SS_ENDELLIPSIS);
        let button = (w!("BUTTON"), WS_TABSTOP | BS_PUSHBUTTON as u32);
        let list = (
            w!("COMBOBOX"),
            WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
        );
        let edit = (w!("EDIT"), WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32);
        let specs = [
            (ProfileLabel, label, "配置"),
            (Profile, list, ""),
            (ProfileName, edit, ""),
            (ProfileAdd, button, "新建"),
            (ProfileRename, button, "重命名"),
            (ProfileDelete, button, "删除"),
            (SaveProfile, button, "保存配置"),
            (ExportProfiles, button, "导出配置"),
            (ImportProfiles, button, "导入配置"),
            (GridLabel, label, "快捷发送"),
            (GridEmpty, label, "还没有指令，点击右下角新增"),
            (PagePrev, button, "上一页"),
            (PageNumber, label, ""),
            (PageNext, button, "下一页"),
            (EditCommand, button, "编辑指令"),
            (AddCommand, button, "＋ 新增指令"),
            (Status, label, ""),
        ];
        for (id, kind, text) in specs {
            ui.controls[id.index()] = ui.create_child(instance, id as usize, kind, text, true)?;
        }
        for index in 0..VISIBLE_BUTTONS {
            ui.buttons[index] = ui.create_child(instance, FIRST_SEND + index, button, "", false)?;
        }
        ui.message(ProfileName, EM_SETLIMITTEXT, 48, 0);
        ui.refresh_profile();
        ui.layout();
        ui.set(Status, "打开主窗口的串口后可快捷发送");
        Ok(ui)
    }

    fn create_child(
        &self,
        instance: HINSTANCE,
        id: usize,
        kind: (windows_sys::core::PCWSTR, u32),
        text: &str,
        visible: bool,
    ) -> io::Result<HWND> {
        unsafe {
            let style = WS_CHILD | WS_CLIPSIBLINGS | kind.1 | if visible { WS_VISIBLE } else { 0 };
            let hwnd = CreateWindowExW(
                0,
                kind.0,
                wide(text).as_ptr(),
                style,
                0,
                0,
                10,
                10,
                self.hwnd,
                id as HMENU,
                instance,
                null(),
            );
            if hwnd.is_null() {
                return Err(io::Error::last_os_error());
            }
            SendMessageW(hwnd, WM_SETFONT, self.font as usize, 0);
            Ok(hwnd)
        }
    }

    fn handle(&self, id: Id) -> HWND {
        self.controls[id.index()]
    }

    fn message(&self, id: Id, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe { SendMessageW(self.handle(id), msg, wparam, lparam) }
    }

    fn set(&self, id: Id, value: &str) {
        unsafe { SetWindowTextW(self.handle(id), wide(value).as_ptr()) };
    }

    fn text(&self, id: Id) -> String {
        let hwnd = self.handle(id);
        unsafe {
            let size = GetWindowTextLengthW(hwnd) + 1;
            let mut text = vec![0; size as usize];
            let length = GetWindowTextW(hwnd, text.as_mut_ptr(), size);
            String::from_utf16_lossy(&text[..length as usize])
        }
    }

    fn selected(&self, id: Id) -> usize {
        self.message(id, CB_GETCURSEL, 0, 0).max(0) as usize
    }

    fn fill<'a>(&self, id: Id, items: impl IntoIterator<Item = &'a str>, selected: usize) {
        self.message(id, CB_RESETCONTENT, 0, 0);
        for item in items {
            self.message(id, CB_ADDSTRING, 0, wide(item).as_ptr() as LPARAM);
        }
        self.message(id, CB_SETCURSEL, selected, 0);
    }

    fn profile(&self) -> &Profile {
        &self.presets.profiles[self.presets.selected]
    }

    fn layout(&mut self) {
        let mut rect = RECT::default();
        unsafe { GetClientRect(self.hwnd, &mut rect) };
        let width = rect.right * 96 / self.dpi as i32;
        let height = rect.bottom * 96 / self.dpi as i32;
        let page_y = height - FOOTER_HEIGHT;
        let page_size =
            (((page_y - GRID_TOP - 6) / ROW_HEIGHT).max(1) as usize * 2).min(VISIBLE_BUTTONS);
        let input_width = width - 32;
        let controls = [
            (Id::ProfileLabel, 16, 12, 46, 27),
            (Id::Profile, 64, 12, width - 80, 230),
            (Id::ProfileName, 16, 44, width - 231, 26),
            (Id::ProfileAdd, width - 207, 42, 57, 28),
            (Id::ProfileRename, width - 144, 42, 66, 28),
            (Id::ProfileDelete, width - 72, 42, 56, 28),
            (Id::SaveProfile, 16, 78, 94, 27),
            (Id::ExportProfiles, 118, 78, 94, 27),
            (Id::ImportProfiles, 220, 78, 94, 27),
            (Id::GridLabel, 16, 108, input_width, 20),
            (Id::GridEmpty, 16, GRID_TOP + 6, input_width, 31),
            (Id::PagePrev, 16, page_y, 68, 26),
            (Id::PageNumber, 94, page_y, 98, 26),
            (Id::PageNext, 194, page_y, 68, 26),
            (Id::EditCommand, width - 212, height - 47, 84, 30),
            (Id::AddCommand, width - 120, height - 47, 104, 30),
            (Id::Status, 16, height - 46, width - 236, 28),
        ]
        .map(|(id, x, y, w, h)| (self.handle(id), x, y, w, h));
        let button_width = (width - 40) / 2;
        let buttons = self.buttons.iter().enumerate().map(|(index, &button)| {
            (
                button,
                16 + (index % 2) as i32 * (button_width + 8),
                GRID_TOP + 6 + (index / 2) as i32 * ROW_HEIGHT,
                button_width,
                33,
            )
        });
        self.child_layout
            .apply(self.hwnd, self.dpi, controls.into_iter().chain(buttons));
        if self.page_size != page_size {
            let first = self.page * self.page_size;
            self.page_size = page_size;
            self.page = first / page_size;
            self.refresh_buttons();
        }
    }

    fn refresh_buttons(&mut self) {
        let len = self.profile().commands.len();
        let pages = len.div_ceil(self.page_size).max(1);
        self.page = self.page.min(pages - 1);
        for (index, &button) in self.buttons.iter().enumerate() {
            let command = self
                .profile()
                .commands
                .get(self.page * self.page_size + index);
            unsafe {
                if index < self.page_size
                    && let Some(command) = command
                {
                    // Native buttons interpret ampersand as a shortcut marker.
                    SetWindowTextW(button, wide(&command.name.replace('&', "&&")).as_ptr());
                    EnableWindow(
                        button,
                        i32::from(self.connected && !self.busy && self.pending.is_none()),
                    );
                    ShowWindow(button, SW_SHOWNA);
                } else {
                    ShowWindow(button, SW_HIDE);
                }
            }
        }
        self.set(
            Id::PageNumber,
            &format!("{} / {} 页 · {} 条", self.page + 1, pages, len),
        );
        unsafe {
            ShowWindow(
                self.handle(Id::GridEmpty),
                if len == 0 { SW_SHOWNA } else { SW_HIDE },
            );
            EnableWindow(self.handle(Id::PagePrev), i32::from(self.page > 0));
            EnableWindow(self.handle(Id::PageNext), i32::from(self.page + 1 < pages));
            EnableWindow(self.handle(Id::EditCommand), i32::from(len != 0));
        }
    }

    fn refresh_profile(&mut self) {
        self.fill(
            Id::Profile,
            self.presets
                .profiles
                .iter()
                .map(|profile| profile.name.as_str()),
            self.presets.selected,
        );
        self.set(Id::ProfileName, &self.profile().name);
        self.refresh_buttons();
    }

    fn commit(&mut self, presets: Presets) -> bool {
        match presets.save() {
            Ok(()) => {
                self.presets = presets;
                true
            }
            Err(error) => {
                self.set(Id::Status, &format!("保存失败 · {error}"));
                false
            }
        }
    }

    fn switch_profile(&mut self) {
        let mut next = self.presets.clone();
        next.selected = self.selected(Id::Profile);
        if self.commit(next) {
            self.page = 0;
            self.refresh_profile();
            self.set(Id::Status, &format!("已切换到 {}", self.profile().name));
        } else {
            self.message(Id::Profile, CB_SETCURSEL, self.presets.selected, 0);
        }
    }

    fn edit_profile(&mut self, action: usize) {
        let mut next = self.presets.clone();
        if action == Id::ProfileDelete as usize {
            if next.profiles.len() == 1 {
                self.set(Id::Status, "请至少保留一套配置");
                return;
            }
            next.profiles.remove(next.selected);
            next.selected = next.selected.min(next.profiles.len() - 1);
        } else {
            let new = action == Id::ProfileAdd as usize;
            let input = self.text(Id::ProfileName);
            let mut name = input.trim().to_owned();
            if new && (name.is_empty() || name == self.profile().name) {
                name = (1..)
                    .map(|n| format!("配置 {n}"))
                    .find(|name| !next.profiles.iter().any(|p| p.name == *name))
                    .unwrap();
            }
            if name.is_empty()
                || next
                    .profiles
                    .iter()
                    .enumerate()
                    .any(|(index, p)| (new || index != next.selected) && p.name == name)
            {
                self.set(Id::Status, "请输入未使用的配置名称");
                return;
            }
            if new {
                next.selected = next.profiles.len();
                next.profiles.push(Profile {
                    name,
                    commands: Vec::new(),
                });
            } else {
                next.profiles[next.selected].name = name;
            }
        }
        if self.commit(next) {
            self.page = 0;
            self.refresh_profile();
            self.set(Id::Status, "配置已保存");
        }
    }

    fn delete_prompt(&self, id: usize) -> Option<String> {
        if id == Id::ProfileDelete as usize && self.presets.profiles.len() > 1 {
            Some(format!("删除配置「{}」及其所有指令？", self.profile().name))
        } else {
            None
        }
    }

    fn command(&mut self, id: usize, notification: u32) {
        match notification {
            CBN_SELCHANGE if id == Id::Profile as usize => self.switch_profile(),
            BN_CLICKED => match id {
                x if x == Id::SaveProfile as usize => {
                    let result = self.presets.save();
                    let message = match result {
                        Ok(()) => "所有已保存的配置和指令已写入本机".into(),
                        Err(error) => format!("保存配置失败 · {error}"),
                    };
                    self.set(Id::Status, &message);
                }
                x if [Id::ProfileAdd, Id::ProfileRename, Id::ProfileDelete]
                    .into_iter()
                    .any(|id| id as usize == x) =>
                {
                    self.edit_profile(x)
                }
                x if x == Id::PagePrev as usize => {
                    self.page = self.page.saturating_sub(1);
                    self.refresh_buttons();
                }
                x if x == Id::PageNext as usize => {
                    self.page += 1;
                    self.refresh_buttons();
                }
                x if (FIRST_SEND..FIRST_SEND + self.page_size).contains(&x) => {
                    if self.connected
                        && !self.busy
                        && self.pending.is_none()
                        && let Some(command) = self
                            .profile()
                            .commands
                            .get(self.page * self.page_size + x - FIRST_SEND)
                    {
                        self.pending = Some(command.clone());
                        if unsafe { PostMessageW(self.owner, SEND_MESSAGE, 0, 0) } == 0 {
                            self.pending = None;
                            self.set(Id::Status, "未能提交发送请求");
                        }
                    }
                }
                _ => (),
            },
            _ => (),
        }
    }
}

fn export_profiles(window: HWND, state: &RefCell<Control>) {
    let Ok(ui) = state.try_borrow() else {
        return;
    };
    let snapshot = ui.presets.to_json();
    drop(ui);
    let result = snapshot.and_then(|bytes| {
        file_dialog::save_as(
            window,
            "导出全部控制配置",
            "串口控制配置.json",
            "JSON 文件 (*.json)",
            "json",
            &bytes,
        )
    });
    let message = match result {
        Ok(Some(path)) => format!("配置已导出 · {}", path.display()),
        Ok(None) => return,
        Err(error) => format!("导出配置失败 · {error}"),
    };
    if let Ok(ui) = state.try_borrow() {
        ui.set(Id::Status, &message);
    }
}

fn import_profiles(window: HWND, state: &RefCell<Control>) {
    let result = file_dialog::open_json(window)
        .and_then(|path| path.map(|path| Presets::read(&path)).transpose());
    let mut ui = state.borrow_mut();
    let result = result.and_then(|incoming| {
        let Some(incoming) = incoming else {
            return Ok(None);
        };
        let mut next = ui.presets.clone();
        let added = next.import(incoming)?;
        next.save()?;
        ui.presets = next;
        ui.page = 0;
        ui.refresh_profile();
        Ok(Some(added))
    });
    match result {
        Ok(Some(0)) => ui.set(Id::Status, "配置已存在"),
        Ok(Some(count)) => ui.set(Id::Status, &format!("已导入 {count} 套配置")),
        Ok(None) => (),
        Err(error) => ui.set(Id::Status, &format!("导入失败 · {error}")),
    }
}

fn edit_command(window: HWND, state: &RefCell<Control>, new: bool) {
    let ui = state.borrow();
    let index = if new {
        None
    } else {
        Some(ui.page * ui.page_size)
    };
    if index.is_some_and(|index| index >= ui.profile().commands.len()) {
        return;
    }
    let presets = ui.presets.clone();
    drop(ui);
    let result = command_editor::edit(window, presets, index);
    let mut ui = state.borrow_mut();
    match result {
        Ok(Some(edit)) => {
            ui.presets = edit.presets;
            if let Some(index) = edit.index {
                ui.page = index / ui.page_size;
            }
            ui.refresh_buttons();
            ui.set(
                Id::Status,
                if edit.index.is_some() {
                    "指令已保存"
                } else {
                    "指令已删除"
                },
            );
        }
        Ok(None) => (),
        Err(error) => ui.set(Id::Status, &format!("无法打开指令编辑器 · {error}")),
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        unsafe { DeleteObject(self.font) };
    }
}

fn window_bounds(width: i32, height: i32, dpi: u32) -> RECT {
    let mut bounds = RECT {
        left: 0,
        top: 0,
        right: scale(width, dpi),
        bottom: scale(height, dpi),
    };
    unsafe { AdjustWindowRectExForDpi(&mut bounds, STYLE, 0, EX_STYLE, dpi) };
    bounds
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            unsafe {
                let dc = BeginPaint(hwnd, &mut paint);
                FillRect(dc, &paint.rcPaint, GetSysColorBrush(COLOR_WINDOW));
                EndPaint(hwnd, &paint);
            }
            return 0;
        }
        WM_CLOSE => {
            unsafe { ShowWindow(hwnd, SW_HIDE) };
            return 0;
        }
        WM_COMMAND if wparam == IDCANCEL as usize => {
            unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
            return 0;
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            unsafe { SetBkColor(wparam as HDC, GetSysColor(COLOR_WINDOW)) };
            return unsafe { GetSysColorBrush(COLOR_WINDOW) } as LRESULT;
        }
        WM_GETMINMAXINFO => {
            let bounds = window_bounds(380, 469, unsafe { GetDpiForWindow(hwnd) }.max(96));
            let info = unsafe { &mut *(lparam as *mut MINMAXINFO) };
            info.ptMinTrackSize = POINT {
                x: bounds.right - bounds.left,
                y: bounds.bottom - bounds.top,
            };
            return 0;
        }
        WM_SIZE | WM_COMMAND => {
            let state =
                unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const RefCell<Control>;
            if let Some(state) = unsafe { state.as_ref() } {
                if msg == WM_COMMAND {
                    match wparam {
                        x if x == Id::ExportProfiles as usize => {
                            export_profiles(hwnd, state);
                            return 0;
                        }
                        x if x == Id::ImportProfiles as usize => {
                            import_profiles(hwnd, state);
                            return 0;
                        }
                        x if x == Id::AddCommand as usize || x == Id::EditCommand as usize => {
                            edit_command(hwnd, state, x == Id::AddCommand as usize);
                            return 0;
                        }
                        _ => (),
                    }
                }
                let id = wparam & 0xffff;
                let notification = (wparam >> 16) as u32 & 0xffff;
                let prompt = if msg == WM_COMMAND && notification == BN_CLICKED {
                    state.try_borrow().ok().and_then(|ui| ui.delete_prompt(id))
                } else {
                    None
                };
                if let Some(prompt) = prompt
                    && unsafe {
                        MessageBoxW(
                            hwnd,
                            wide(&prompt).as_ptr(),
                            TITLE,
                            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
                        )
                    } != IDYES
                {
                    return 0;
                }
                if let Ok(mut ui) = state.try_borrow_mut() {
                    if msg == WM_SIZE && wparam != SIZE_MINIMIZED as usize {
                        ui.layout();
                    } else if msg == WM_COMMAND {
                        ui.command(id, notification);
                    }
                }
            }
            return 0;
        }
        WM_SHOWWINDOW if wparam != 0 => {
            let state =
                unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const RefCell<Control>;
            if let Some(state) = unsafe { state.as_ref() }
                && let Ok(ui) = state.try_borrow()
            {
                unsafe { SetFocus(ui.handle(Id::Profile)) };
            }
        }
        _ => (),
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
