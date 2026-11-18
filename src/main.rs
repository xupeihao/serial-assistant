#![cfg_attr(not(test), windows_subsystem = "windows")]

mod codec;
mod command_editor;
mod control;
mod file_dialog;
mod preferences;
mod presets;
mod serial_io;
mod upgrade;
mod upgrade_protocol;
mod window;
mod window_layout;

fn main() {
    if let Err(error) = window::run() {
        window::startup_error(&error);
        std::process::exit(1);
    }
}
