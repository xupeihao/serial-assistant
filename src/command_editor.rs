use crate::presets::{Ending, Preset, Presets, TimeFormat};
use crate::serial_io;
use crate::window::wide;
use std::io;
use std::ptr::null;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::{
    COLOR_3DFACE, COLOR_GRAYTEXT, GetSysColor, GetSysColorBrush, HDC, SetBkMode, SetTextColor,
    TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::EM_SETLIMITTEXT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::w;

const SELECT_LABEL: i32 = 110;
const SELECT: i32 = 111;
const NAME: i32 = 112;
const MODE: i32 = 113;
const ENDING: i32 = 114;
const CONTENT: i32 = 115;
const STATUS: i32 = 116;
const DELETE: usize = 117;
const TIME_FORMAT: i32 = 118;
const TIME_LABEL: i32 = 119;
const TIME_PREVIEW: i32 = 120;

pub struct EditResult {
    pub presets: Presets,
    pub index: Option<usize>,
}

struct Editor {
    presets: Presets,
    index: Option<usize>,
}

pub fn edit(owner: HWND, presets: Presets, index: Option<usize>) -> io::Result<Option<EditResult>> {
    let mut editor = Editor { presets, index };
    let result = unsafe {
        DialogBoxParamW(
            GetModuleHandleW(null()),
            101usize as *const u16,
            owner,
            Some(dialog_proc),
            (&raw mut editor) as LPARAM,
        )
    };
    match result {
        -1 => Err(io::Error::last_os_error()),
        1 => Ok(Some(EditResult {
            presets: editor.presets,
            index: editor.index,
        })),
        _ => Ok(None),
    }
}

fn set(dialog: HWND, id: i32, value: &str) {
    unsafe { SetDlgItemTextW(dialog, id, wide(value).as_ptr()) };
}

fn text(dialog: HWND, id: i32) -> String {
    unsafe {
        let handle = GetDlgItem(dialog, id);
        let mut buffer = vec![0; GetWindowTextLengthW(handle) as usize + 1];
        let count = GetWindowTextW(handle, buffer.as_mut_ptr(), buffer.len() as i32);
        String::from_utf16_lossy(&buffer[..count as usize])
    }
}

fn select(dialog: HWND, id: i32) -> usize {
    unsafe { SendDlgItemMessageW(dialog, id, CB_GETCURSEL, 0, 0).max(0) as usize }
}

fn fill<'a>(dialog: HWND, id: i32, choices: impl IntoIterator<Item = &'a str>) {
    for choice in choices {
        unsafe {
            SendDlgItemMessageW(dialog, id, CB_ADDSTRING, 0, wide(choice).as_ptr() as LPARAM)
        };
    }
}

fn update_mode(dialog: HWND) {
    let mode = select(dialog, MODE);
    let time = mode == 2;
    unsafe {
        EnableWindow(GetDlgItem(dialog, ENDING), i32::from(mode != 1));
        ShowWindow(
            GetDlgItem(dialog, CONTENT),
            if time { SW_HIDE } else { SW_SHOW },
        );
        for id in [TIME_FORMAT, TIME_LABEL, TIME_PREVIEW] {
            ShowWindow(GetDlgItem(dialog, id), if time { SW_SHOW } else { SW_HIDE });
        }
    };
    update_preview(dialog);
}

fn update_preview(dialog: HWND) {
    if select(dialog, MODE) == 2 {
        let format = TimeFormat::OPTIONS[select(dialog, TIME_FORMAT)];
        set(
            dialog,
            TIME_PREVIEW,
            &format.render(serial_io::local_time()),
        );
    }
}

fn read_preset(dialog: HWND) -> Preset {
    let hex = select(dialog, MODE) == 1;
    let time = select(dialog, MODE) == 2;
    Preset {
        name: text(dialog, NAME).trim().to_owned(),
        content: if time {
            String::new()
        } else {
            text(dialog, CONTENT)
        },
        hex,
        time_format: time.then(|| TimeFormat::OPTIONS[select(dialog, TIME_FORMAT)]),
        ending: if hex {
            Ending::None
        } else {
            Ending::OPTIONS[select(dialog, ENDING)]
        },
    }
}

fn check_input(dialog: HWND) {
    let preset = read_preset(dialog);
    let error = preset.validate_content().err();
    let show_error = !preset.content.is_empty() || preset.ending != Ending::None;
    set(
        dialog,
        STATUS,
        if let Some(error) = error.filter(|_| show_error) {
            error
        } else if preset.time_format.is_some() {
            "点击快捷按钮时，自动发送电脑当前本地时间"
        } else {
            ""
        },
    );
    let valid = error.is_none() && !preset.name.is_empty();
    unsafe { EnableWindow(GetDlgItem(dialog, IDOK), i32::from(valid)) };
}

impl Editor {
    fn init(&self, dialog: HWND) {
        fill(dialog, MODE, ["文本", "HEX", "电脑时间"]);
        fill(dialog, ENDING, Ending::LABELS);
        fill(dialog, TIME_FORMAT, TimeFormat::LABELS);
        let profile = &self.presets.profiles[self.presets.selected];
        fill(
            dialog,
            SELECT,
            profile.commands.iter().map(|command| command.name.as_str()),
        );
        unsafe {
            SendDlgItemMessageW(dialog, NAME, EM_SETLIMITTEXT, 48, 0);
            SendDlgItemMessageW(dialog, CONTENT, EM_SETLIMITTEXT, 65536, 0);
            if let Some(index) = self.index {
                SetWindowTextW(dialog, w!("编辑指令"));
                SendDlgItemMessageW(dialog, SELECT, CB_SETCURSEL, index, 0);
            } else {
                for id in [SELECT_LABEL, SELECT, DELETE as i32] {
                    ShowWindow(GetDlgItem(dialog, id), SW_HIDE);
                }
            }
        }
        self.load(dialog);
        unsafe { SetTimer(dialog, 1, 500, None) };
    }

    fn load(&self, dialog: HWND) {
        let profile = &self.presets.profiles[self.presets.selected];
        let preset = self.index.and_then(|i| profile.commands.get(i));
        set(dialog, NAME, preset.map_or("", |p| p.name.as_str()));
        set(dialog, CONTENT, preset.map_or("", |p| p.content.as_str()));
        set(dialog, STATUS, "");
        let ending = preset
            .and_then(|p| {
                Ending::OPTIONS
                    .iter()
                    .position(|&option| option == p.ending)
            })
            .unwrap_or(0);
        unsafe {
            SendDlgItemMessageW(
                dialog,
                MODE,
                CB_SETCURSEL,
                if preset.is_some_and(|p| p.time_format.is_some()) {
                    2
                } else {
                    usize::from(preset.is_some_and(|p| p.hex))
                },
                0,
            );
            SendDlgItemMessageW(dialog, ENDING, CB_SETCURSEL, ending, 0);
            let format = preset
                .and_then(|p| p.time_format)
                .and_then(|format| {
                    TimeFormat::OPTIONS
                        .iter()
                        .position(|&value| value == format)
                })
                .unwrap_or(0);
            SendDlgItemMessageW(dialog, TIME_FORMAT, CB_SETCURSEL, format, 0);
        }
        update_mode(dialog);
        check_input(dialog);
    }

    fn save(&mut self, dialog: HWND) {
        let preset = read_preset(dialog);
        if let Err(issue) = preset.validate() {
            set(dialog, STATUS, issue);
            return;
        }
        let mut next = self.presets.clone();
        let commands = &mut next.profiles[next.selected].commands;
        let index = self.index.unwrap_or(commands.len());
        if let Some(old) = commands.get_mut(index) {
            *old = preset;
        } else {
            commands.push(preset);
        }
        self.finish(dialog, next, Some(index));
    }

    fn delete(&mut self, dialog: HWND) {
        let Some(index) = self.index else {
            return;
        };
        let mut next = self.presets.clone();
        let commands = &mut next.profiles[next.selected].commands;
        let prompt = wide(&format!("删除指令「{}」？", commands[index].name));
        if unsafe {
            MessageBoxW(
                dialog,
                prompt.as_ptr(),
                w!("编辑指令"),
                MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
            )
        } == IDYES
        {
            commands.remove(index);
            self.finish(dialog, next, None);
        }
    }

    fn finish(&mut self, dialog: HWND, presets: Presets, index: Option<usize>) {
        if let Err(error) = presets.save() {
            set(dialog, STATUS, &format!("保存失败 · {error}"));
            return;
        }
        self.presets = presets;
        self.index = index;
        unsafe { EndDialog(dialog, 1) };
    }
}

unsafe extern "system" fn dialog_proc(
    dialog: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    if msg == WM_CTLCOLORSTATIC && lparam as HWND == unsafe { GetDlgItem(dialog, STATUS) } {
        unsafe {
            let info = read_preset(dialog).time_format.is_some();
            SetTextColor(
                wparam as HDC,
                if info {
                    GetSysColor(COLOR_GRAYTEXT)
                } else {
                    0x0021_21b5
                },
            );
            SetBkMode(wparam as HDC, TRANSPARENT as i32);
            return GetSysColorBrush(COLOR_3DFACE) as isize;
        }
    }
    if msg == WM_INITDIALOG {
        let editor = unsafe { &*(lparam as *const Editor) };
        editor.init(dialog);
        unsafe {
            SetWindowLongPtrW(dialog, GWLP_USERDATA, lparam);
            SetFocus(GetDlgItem(dialog, NAME));
        }
        return 0;
    }
    if msg == WM_TIMER && wparam == 1 {
        update_preview(dialog);
        return 1;
    }
    if msg == WM_COMMAND {
        let id = wparam & 0xffff;
        let notice = (wparam >> 16) as u32 & 0xffff;
        if ((id == NAME as usize || id == CONTENT as usize) && notice == EN_CHANGE)
            || ((id == MODE as usize || id == ENDING as usize || id == TIME_FORMAT as usize)
                && notice == CBN_SELCHANGE)
        {
            if id == MODE as usize && notice == CBN_SELCHANGE {
                if select(dialog, MODE) == 2 {
                    if text(dialog, NAME).trim().is_empty() {
                        set(dialog, NAME, "RTC对时");
                    }
                    if select(dialog, ENDING) == 0 {
                        unsafe { SendDlgItemMessageW(dialog, ENDING, CB_SETCURSEL, 1, 0) };
                    }
                }
                update_mode(dialog);
            } else if id == TIME_FORMAT as usize {
                update_preview(dialog);
            }
            check_input(dialog);
            return 1;
        }
        if id == IDCANCEL as usize {
            unsafe { EndDialog(dialog, 0) };
            return 1;
        }
        if id != IDOK as usize
            && id != DELETE
            && !(id == SELECT as usize && notice == CBN_SELCHANGE)
        {
            return 0;
        }
        let editor = unsafe { GetWindowLongPtrW(dialog, GWLP_USERDATA) } as *mut Editor;
        if let Some(editor) = unsafe { editor.as_mut() } {
            match id {
                x if x == IDOK as usize => editor.save(dialog),
                DELETE => editor.delete(dialog),
                x if x == SELECT as usize && notice == CBN_SELCHANGE => {
                    editor.index = Some(select(dialog, SELECT));
                    editor.load(dialog);
                }
                _ => return 0,
            }
            return 1;
        }
    }
    0
}
