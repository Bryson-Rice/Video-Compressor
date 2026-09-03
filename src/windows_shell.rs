use std::path::{Path, PathBuf};
use winreg::enums::*;
use winreg::RegKey;

const PROG_ID: &str = "VideoCompressor.File";
const FRIENDLY_TYPE_NAME: &str = "Video Compressor File";
const VERB_FRIENDLY_NAME: &str = "Compress with Video Compressor";

// Every extension this app can handle, kept in sync with main.rs and encoder.rs by hand - this is shit and should be one shared list
const ALL_EXTS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "ts", "mts",
    "jpg", "jpeg", "png", "bmp", "gif",
];

// Called after any registry change so Explorer picks up the new associations immediately, no logoff/reboot needed

#[link(name = "shell32")]
extern "system" {
    fn SHChangeNotify(
        wEventId: i32,
        uFlags: u32,
        dwItem1: *const core::ffi::c_void,
        dwItem2: *const core::ffi::c_void,
    );
}

const SHCNE_ASSOCCHANGED: i32 = 0x0800_0000;
const SHCNF_IDLIST: u32 = 0x0000;


#[allow(dead_code)]
pub fn is_registered() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(format!("Software\\Classes\\{}", PROG_ID))
        .is_ok()
}

// Writes a ProgID with an icon/open command, plus an OpenWithProgids entry per extension - shows up in "Open With" without becoming the forced default handler. All under HKCU, so no elevation prompt
pub fn register(exe_path: &Path) -> std::io::Result<()> {
    let exe_str = exe_path.to_string_lossy();
    let exe_quoted = format!("\"{}\"", exe_str);

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    let (prog_key, _) = hkcu
        .create_subkey(format!("Software\\Classes\\{}", PROG_ID))?;
    prog_key.set_value("", &FRIENDLY_TYPE_NAME)?;

    let (icon_key, _) = prog_key.create_subkey("DefaultIcon")?;
    icon_key.set_value("", &format!("{},0", exe_str))?;

    let (open_key, _) = prog_key.create_subkey("shell\\open")?;
    open_key.set_value("", &VERB_FRIENDLY_NAME)?;

    let (cmd_key, _) = open_key.create_subkey("command")?;
    cmd_key.set_value("", &format!("{} \"%1\"", exe_quoted))?;

    for ext in ALL_EXTS {
        let key_path = format!("Software\\Classes\\.{}\\OpenWithProgids", ext);
        let (ext_key, _) = hkcu.create_subkey(&key_path)?;
        ext_key.set_value(PROG_ID, &"")?;
    }

    notify_shell();
    Ok(())
}

// This was used in testing but should be removed at this point
#[allow(dead_code)]
pub fn unregister() -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    for ext in ALL_EXTS {
        let key_path = format!(
            "Software\\Classes\\.{}\\OpenWithProgids",
            ext
        );
        // Missing key/value just means there's nothing to remove
        if let Ok(ext_key) =
            hkcu.open_subkey_with_flags(&key_path, KEY_SET_VALUE)
        {
            let _ = ext_key.delete_value(PROG_ID);
        }
    }

    let _ = hkcu.delete_subkey_all(format!("Software\\Classes\\{}", PROG_ID));

    notify_shell();
    Ok(())
}


fn notify_shell() {
    // SAFETY: null item pointers are required by SHCNE_ASSOCCHANGED
    unsafe {
        SHChangeNotify(
            SHCNE_ASSOCCHANGED,
            SHCNF_IDLIST,
            std::ptr::null(),
            std::ptr::null(),
        );
    }
}

// Stored as REG_SZ/REG_DWORD under HKCU\Software\VideoCompressor\Settings so they're human-readable in regedit if you ever need to poke at them

const SETTINGS_KEY: &str = "Software\\VideoCompressor\\Settings";
const VAL_CUSTOM_MB_VIDEO: &str = "CustomMbVideo";
const VAL_CUSTOM_MB_IMAGE: &str = "CustomMbImage";
const VAL_MUTE: &str = "Mute";
const VAL_TWO_PASS: &str = "TwoPass";
const VAL_OUTPUT_DIR: &str = "OutputDir";

pub const DEFAULT_CUSTOM_MB_VIDEO: f64 = 50.0;
pub const DEFAULT_CUSTOM_MB_IMAGE: f64 = 5.0;

pub struct AppSettings {
    pub custom_mb_video: f64,
    pub custom_mb_image: f64,
    pub mute: bool,
    pub two_pass: bool,
    pub output_dir: Option<PathBuf>,
}

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

pub fn load_settings() -> AppSettings {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = match hkcu.open_subkey(SETTINGS_KEY) {
        Ok(k) => k,
        Err(_) => return AppSettings::default(),
    };

    let custom_mb_video = key
        .get_value::<String, _>(VAL_CUSTOM_MB_VIDEO)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|&v| v >= 0.1)
        .unwrap_or(DEFAULT_CUSTOM_MB_VIDEO);

    let custom_mb_image = key
        .get_value::<String, _>(VAL_CUSTOM_MB_IMAGE)
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|&v| v >= 0.1)
        .unwrap_or(DEFAULT_CUSTOM_MB_IMAGE);

    let mute = key
        .get_value::<u32, _>(VAL_MUTE)
        .ok()
        .map(|v| v != 0)
        .unwrap_or(false);

    let two_pass = key
        .get_value::<u32, _>(VAL_TWO_PASS)
        .ok()
        .map(|v| v != 0)
        .unwrap_or(true);

    // Empty string (or missing value) means "no custom folder set" - falls back to writing output next to each source file
    let output_dir = key
        .get_value::<String, _>(VAL_OUTPUT_DIR)
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);

    AppSettings { custom_mb_video, custom_mb_image, mute, two_pass, output_dir }
}

// Called on every encode start. Silently ignores errors - if the write fails, settings just don't stick this session, not worth surfacing
pub fn save_settings(s: &AppSettings) {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok((key, _)) = hkcu.create_subkey(SETTINGS_KEY) {
        let _ = key.set_value(VAL_CUSTOM_MB_VIDEO, &s.custom_mb_video.to_string());
        let _ = key.set_value(VAL_CUSTOM_MB_IMAGE, &s.custom_mb_image.to_string());
        let _ = key.set_value(VAL_MUTE, &(s.mute as u32));
        let _ = key.set_value(VAL_TWO_PASS, &(s.two_pass as u32));
        let output_dir_str = s
            .output_dir
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let _ = key.set_value(VAL_OUTPUT_DIR, &output_dir_str);
    }
}