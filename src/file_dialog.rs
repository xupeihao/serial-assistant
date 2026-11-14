use crate::preferences::save_settings_file;
use crate::window::wide;
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Controls::Dialogs::*;

pub fn save_as(
    owner: HWND,
    title: &str,
    filename: &str,
    description: &str,
    extension: &str,
    contents: &[u8],
) -> io::Result<Option<PathBuf>> {
    let path = choose_path(owner, title, filename, description, extension, true)?;
    if let Some(path) = &path {
        save_settings_file(path, contents)?;
    }
    Ok(path)
}

pub fn open_json(owner: HWND) -> io::Result<Option<PathBuf>> {
    choose_path(
        owner,
        "导入控制配置",
        "",
        "JSON 文件 (*.json)",
        "json",
        false,
    )
}

fn choose_path(
    owner: HWND,
    title: &str,
    filename: &str,
    description: &str,
    extension: &str,
    save: bool,
) -> io::Result<Option<PathBuf>> {
    let mut path = vec![0; 32768];
    for (slot, unit) in path.iter_mut().zip(filename.encode_utf16()) {
        *slot = unit;
    }
    let title = wide(title);
    let filter = wide(&format!("{description}\0*.{extension}\0"));
    let extension = wide(extension);
    let mut options = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrTitle: title.as_ptr(),
        lpstrFilter: filter.as_ptr(),
        nFilterIndex: 1,
        lpstrFile: path.as_mut_ptr(),
        nMaxFile: path.len() as u32,
        lpstrDefExt: extension.as_ptr(),
        Flags: OFN_EXPLORER
            | OFN_NOCHANGEDIR
            | OFN_PATHMUSTEXIST
            | if save {
                OFN_OVERWRITEPROMPT
            } else {
                OFN_FILEMUSTEXIST
            },
        ..Default::default()
    };
    let selected = unsafe {
        if save {
            GetSaveFileNameW(&mut options)
        } else {
            GetOpenFileNameW(&mut options)
        }
    };
    if selected == 0 {
        let error = unsafe { CommDlgExtendedError() };
        return if error == 0 {
            Ok(None)
        } else {
            Err(io::Error::other(format!("文件对话框错误 0x{error:X}")))
        };
    }
    let length = path
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(path.len());
    let path = PathBuf::from(OsString::from_wide(&path[..length]));
    Ok(Some(path))
}
