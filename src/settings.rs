//! Keeps the folders, format and file name pattern between runs, in %APPDATA%\DVD Audio Grabber\settings.txt.
//! Problems reading or saving are ignored, and the defaults are used instead.

use std::fs;
use std::path::PathBuf;

use crate::dvd::Format;

pub struct Settings {
    pub folder: String,
    pub out_dir: String,
    pub format: Format,
    pub name: String,
}

fn file() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(base).join("DVD Audio Grabber").join("settings.txt"))
}

/// The saved settings, with `defaults` for anything missing or unreadable.
pub fn load(defaults: Settings) -> Settings {
    let Some(text) = file().and_then(|path| fs::read_to_string(path).ok()) else {
        return defaults;
    };
    let mut settings = defaults;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "folder" => settings.folder = value.to_string(),
            "out_dir" => settings.out_dir = value.to_string(),
            "format" => settings.format = Format::from_key(value).unwrap_or(settings.format),
            "name" => settings.name = value.to_string(),
            _ => {}
        }
    }
    settings
}

pub fn save(settings: &Settings) {
    let Some(path) = file() else {
        return;
    };
    if let Some(dir) = path.parent() {
        if fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let text = format!(
        "folder={}\nout_dir={}\nformat={}\nname={}\n",
        settings.folder,
        settings.out_dir,
        settings.format.key(),
        settings.name
    );
    fs::write(path, text).ok();
}
