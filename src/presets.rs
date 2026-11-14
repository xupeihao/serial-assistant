use crate::codec;
use crate::preferences::{save_settings_file, settings_directory};
use encoding_rs::UTF_8;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub enum Ending {
    #[default]
    None,
    CrLf,
    Lf,
    Cr,
}

impl Ending {
    pub const OPTIONS: [Self; 4] = [Self::None, Self::CrLf, Self::Lf, Self::Cr];
    pub const LABELS: [&str; 4] = ["无换行", "CRLF", "LF", "CR"];

    pub fn text(self) -> &'static str {
        match self {
            Self::None => "",
            Self::CrLf => "\r\n",
            Self::Lf => "\n",
            Self::Cr => "\r",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Preset {
    pub name: String,
    pub content: String,
    pub hex: bool,
    pub ending: Ending,
}

impl Preset {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.name.trim().is_empty() || self.name.contains(['\0', '\r', '\n']) {
            return Err("请输入有效的按钮名称");
        }
        self.validate_content()
    }

    pub fn validate_content(&self) -> Result<(), &'static str> {
        if self.content.contains('\0') {
            return Err("指令含有 NUL 字符，发送 00 字节请使用 HEX 模式");
        }
        let bytes = codec::encode(&self.content, self.hex, UTF_8, self.ending.text())?;
        if bytes.is_empty() {
            return Err("请输入要发送的指令");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Profile {
    pub name: String,
    pub commands: Vec<Preset>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Presets {
    pub selected: usize,
    pub profiles: Vec<Profile>,
}

impl Default for Presets {
    fn default() -> Self {
        Self {
            selected: 0,
            profiles: vec![Profile {
                name: "配置 1".into(),
                commands: Vec::new(),
            }],
        }
    }
}

impl Presets {
    fn path() -> io::Result<PathBuf> {
        settings_directory()
            .map(|dir| dir.join("control.json"))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA"))
    }

    pub fn load() -> io::Result<Self> {
        match Self::read(&Self::path()?) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            result => result,
        }
    }

    pub fn read(path: &Path) -> io::Result<Self> {
        let bytes = fs::read(path)?;
        let mut saved: Self =
            serde_json::from_slice(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes))?;
        if saved.profiles.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "配置列表为空"));
        }
        saved.selected = saved.selected.min(saved.profiles.len() - 1);
        Ok(saved)
    }

    pub fn import(&mut self, incoming: Self) -> io::Result<usize> {
        for profile in &incoming.profiles {
            if profile.name.trim().is_empty() || profile.name.contains(['\0', '\r', '\n']) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "导入文件含有无效的配置名称",
                ));
            }
            for command in &profile.commands {
                command.validate().map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("「{}」的指令「{}」：{error}", profile.name, command.name),
                    )
                })?;
            }
        }
        // The initial empty placeholder need not appear alongside a migrated setup.
        if *self == Self::default() {
            self.profiles.clear();
        }
        let mut added = 0;
        for (index, mut profile) in incoming.profiles.into_iter().enumerate() {
            let mut matched = None;
            for suffix in 0.. {
                let name = match suffix {
                    0 => profile.name.clone(),
                    1 => format!("{}（导入）", profile.name),
                    _ => format!("{}（导入 {suffix}）", profile.name),
                };
                if let Some(existing) = self.profiles.iter().position(|item| item.name == name) {
                    if self.profiles[existing].commands == profile.commands {
                        matched = Some(existing);
                        break;
                    }
                } else {
                    profile.name = name;
                    break;
                }
            }
            let selected = matched.unwrap_or(self.profiles.len());
            if matched.is_none() {
                self.profiles.push(profile);
                added += 1;
            }
            if index == incoming.selected {
                self.selected = selected;
            }
        }
        Ok(added)
    }

    pub fn save(&self) -> io::Result<()> {
        save_settings_file(&Self::path()?, &self.to_json()?)
    }

    pub fn to_json(&self) -> io::Result<Vec<u8>> {
        Ok(serde_json::to_vec_pretty(self)?)
    }
}
