use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[cfg(target_os = "windows")]
mod blobs {
    pub static FFMPEG: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/bin/ffmpeg.exe"));
    pub static FFPROBE: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/bin/ffprobe.exe"));
    pub const SUFFIX: &str = ".exe";
}

#[cfg(target_os = "linux")]
mod blobs {
    pub static FFMPEG: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/bin/ffmpeg.bin"));
    pub static FFPROBE: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/bin/ffprobe.bin"));
    pub const SUFFIX: &str = "";
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod blobs {
    pub static FFMPEG: &[u8] = &[];
    pub static FFPROBE: &[u8] = &[];
    pub const SUFFIX: &str = "";
}

struct Tools {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
}

static TOOLS: OnceLock<Option<Tools>> = OnceLock::new();

pub fn prepare() {
    let _ = TOOLS.get_or_init(init);
}

pub fn path(name: &str) -> Option<PathBuf> {
    let tools = TOOLS.get_or_init(init).as_ref()?;
    match name {
        "ffmpeg" => Some(tools.ffmpeg.clone()),
        "ffprobe" => Some(tools.ffprobe.clone()),
        _ => None,
    }
}

fn init() -> Option<Tools> {
    if blobs::FFMPEG.is_empty() || blobs::FFPROBE.is_empty() {
        return None;
    }
    let fallback = std::env::temp_dir().join("VideoCompressor").join("tools");
    for dir in [cache_root(), fallback] {
        let ffmpeg = extract(&dir, &format!("ffmpeg{}", blobs::SUFFIX), blobs::FFMPEG);
        let ffprobe = extract(&dir, &format!("ffprobe{}", blobs::SUFFIX), blobs::FFPROBE);
        if let (Ok(ffmpeg), Ok(ffprobe)) = (ffmpeg, ffprobe) {
            return Some(Tools { ffmpeg, ffprobe });
        }
    }
    None
}

fn cache_root() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Ok(p) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(p).join("VideoCompressor").join("tools");
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(p) = std::env::var("XDG_CACHE_HOME") {
            if !p.is_empty() {
                return PathBuf::from(p).join("video-compressor").join("tools");
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join(".cache").join("video-compressor").join("tools");
        }
    }
    std::env::temp_dir().join("VideoCompressor").join("tools")
}

fn has_len(path: &Path, len: usize) -> bool {
    fs::metadata(path).map(|m| m.len() == len as u64).unwrap_or(false)
}

fn extract(dir: &Path, name: &str, data: &[u8]) -> std::io::Result<PathBuf> {
    let dest = dir.join(name);
    if has_len(&dest, data.len()) {
        make_executable(&dest);
        return Ok(dest);
    }

    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{}.{}.tmp", name, std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(data)?;
    }
    make_executable(&tmp);

    match fs::rename(&tmp, &dest) {
        Ok(()) => Ok(dest),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            if has_len(&dest, data.len()) { Ok(dest) } else { Err(e) }
        }
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}