use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub fn settings_directory() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("SerialAssistant"))
}

pub fn save_settings_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

pub const LOG_FONT_COUNT: usize = 4;
pub const LOG_FONT_SIZES: [i32; 9] = [9, 10, 11, 12, 14, 16, 18, 20, 24];
const DEFAULT_LOG_FONT: usize = 0;
const DEFAULT_LOG_FONT_SIZE: usize = 3;

pub struct Preferences {
    pub width: i32,
    pub height: i32,
    pub serial_width: i32,
    pub lower_height: i32,
    pub maximized: bool,
    pub timestamps: bool,
    pub log_font: usize,
    pub log_font_size: usize,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            width: 940,
            height: 610,
            serial_width: 330,
            lower_height: 210,
            maximized: false,
            timestamps: false,
            log_font: DEFAULT_LOG_FONT,
            log_font_size: DEFAULT_LOG_FONT_SIZE,
        }
    }
}

impl Preferences {
    pub fn load() -> Self {
        Self::path()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    pub fn save(&self) -> io::Result<()> {
        let Some(path) = Self::path() else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA"));
        };
        self.save_to(&path)
    }

    fn path() -> Option<PathBuf> {
        settings_directory().map(|dir| dir.join("settings.txt"))
    }

    fn save_to(&self, path: &Path) -> io::Result<()> {
        let contents = format!(
            "2 {} {} {} {} {} {} {} {}\n",
            self.width,
            self.height,
            self.serial_width,
            self.lower_height,
            self.maximized,
            self.timestamps,
            self.log_font,
            self.log_font_size,
        );
        save_settings_file(path, contents.as_bytes())
    }

    fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split_ascii_whitespace();
        let version = parts.next()?;
        if version != "1" && version != "2" {
            return None;
        }
        let mut dimension = || {
            parts
                .next()?
                .parse::<i32>()
                .ok()
                .filter(|n| (1..=32767).contains(n))
        };
        let width = dimension()?;
        let height = dimension()?;
        let serial_width = dimension()?;
        let lower_height = dimension()?;
        let maximized = parts.next()?.parse().ok()?;
        let timestamps = parts.next()?.parse().ok()?;
        let mut font_option = |count, default| {
            parts
                .next()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|&index| index < count)
                .unwrap_or(default)
        };
        let (log_font, log_font_size) = if version == "2" {
            (
                font_option(LOG_FONT_COUNT, DEFAULT_LOG_FONT),
                font_option(LOG_FONT_SIZES.len(), DEFAULT_LOG_FONT_SIZE),
            )
        } else {
            (DEFAULT_LOG_FONT, DEFAULT_LOG_FONT_SIZE)
        };
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            width,
            height,
            serial_width,
            lower_height,
            maximized,
            timestamps,
            log_font,
            log_font_size,
        })
    }
}
