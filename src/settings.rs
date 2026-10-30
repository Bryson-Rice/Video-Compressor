//   macOS -> ~/Library/Application Support/VideoCompressor/settings.json
//   Linux -> ~/.config/video-compressor/settings.json

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_CUSTOM_MB_VIDEO: f64 = 50.0;
pub const DEFAULT_CUSTOM_MB_IMAGE: f64 = 5.0;

#[derive(Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "default_custom_mb_video")]
    pub custom_mb_video: f64,
    #[serde(default = "default_custom_mb_image")]
    pub custom_mb_image: f64,
    #[serde(default)]
    pub mute: bool,
    #[serde(default = "default_two_pass")]
    pub two_pass: bool,
    #[serde(default)]
    pub output_dir: Option<PathBuf>,
}

fn default_custom_mb_video() -> f64 { DEFAULT_CUSTOM_MB_VIDEO }
fn default_custom_mb_image() -> f64 { DEFAULT_CUSTOM_MB_IMAGE }
fn default_two_pass() -> bool { true }

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            custom_mb_video: DEFAULT_CUSTOM_MB_VIDEO,
            custom_mb_image: DEFAULT_CUSTOM_MB_IMAGE,
            mute: false,
            two_pass: true,
            output_dir: None,
        }
    }
}

fn settings_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join("Library").join("Application Support").join("VideoCompressor");
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".config").join("video-compressor");
        }
    }
    std::env::temp_dir().join("VideoCompressor")
}

fn settings_path() -> PathBuf {
    settings_dir().join("settings.json")
}

// Falls back to defaults on first run or if the file's missing/corrupt - never a reason to stop the app from starting
pub fn load_settings() -> AppSettings {
    match std::fs::read_to_string(settings_path()) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => AppSettings::default(),
    }
}

// Best-effort, same as the Windows registry path - a failed write (permissions, read-only fs, etc.) just means settings don't persist this session
pub fn save_settings(s: &AppSettings) {
    let dir = settings_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(settings_path(), json);
    }
}