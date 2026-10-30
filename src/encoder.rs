use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use serde::Deserialize;


#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub duration_secs: f64,
    pub size_mb: f64,
    pub width: u32,
    pub height: u32,
    pub codec: String,
    pub audio_tracks: usize,
    pub color_transfer: Option<String>,
    pub hdr: Option<HdrStaticMeta>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HdrStaticMeta {
    pub master_display: Option<String>,
    pub max_cll: Option<(u32, u32)>,
}

#[derive(Debug, Clone)]
pub struct ImageInfo {
    pub size_mb: f64,
    pub width: u32,
    pub height: u32,
    pub format: String,
}

#[derive(Debug, Clone)]
pub enum FileInfo {
    Video(VideoInfo),
    Image(ImageInfo),
}

pub enum EncoderMessage {
    FileStart { index: usize, total: usize, name: String },
    Pass1Progress(f32),
    Pass2Progress(f32),
    SingleProgress(f32),
    FallbackUsed { name: String, reason: String, retry: String },
    // Kept apart from FileError so a skip never counts as a failure in the summary
    Skipped { name: String, reason: String },
    // Sent after the encode succeeded, so it can only inform and never fail the file
    OutputNote { name: String, text: String },
    FileDone(PathBuf),
    FileError { name: String, error: String },
    AllDone,
    Cancelled,
}


// The active FFmpeg child is stored here too, so cancel() can kill it
// immediately rather than waiting for it to notice the flag
#[derive(Clone, Default)]
pub struct CancelToken {
    pub flag: Arc<AtomicBool>,
    pub child: Arc<Mutex<Option<Child>>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.child.lock() {
            if let Some(child) = guard.as_mut() {
                let _ = child.kill();
            }
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}


// An enum rather than an Err so a skip isn't logged or counted as a failure
enum Outcome {
    Done(PathBuf),
    Skipped(String),
}

fn extension_lower(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
}

pub fn is_image_ext(path: &Path) -> bool {
    matches!(
        extension_lower(path).as_deref(),
        Some("jpg") | Some("jpeg") | Some("png") | Some("bmp") | Some("gif")
    )
}

pub fn is_video_ext(path: &Path) -> bool {
    matches!(
        extension_lower(path).as_deref(),
        Some("mp4")
            | Some("mkv")
            | Some("avi")
            | Some("mov")
            | Some("wmv")
            | Some("flv")
            | Some("webm")
            | Some("m4v")
            | Some("ts")
            | Some("mts")
    )
}

pub fn is_supported(path: &Path) -> bool {
    is_image_ext(path) || is_video_ext(path)
}

// Folders are scanned one level deep (non-recursive) because I dont want to deal with multiple levels of folders
pub fn collect_files(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        if is_supported(path) {
            vec![path.to_path_buf()]
        } else {
            vec![]
        }
    } else if path.is_dir() {
        let mut files: Vec<PathBuf> = std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && is_supported(p))
            .collect();
        files.sort();
        files
    } else {
        vec![]
    }
}


pub fn get_file_info(path: &Path) -> Result<FileInfo, String> {
    if is_image_ext(path) {
        get_image_info(path).map(FileInfo::Image)
    } else {
        get_video_info(path).map(FileInfo::Video)
    }
}

fn get_image_info(path: &Path) -> Result<ImageInfo, String> {
    let img = image::open(path).map_err(|e| format!("Could not open image: {}", e))?;
    let size_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let format = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("unknown")
        .to_uppercase();
    Ok(ImageInfo {
        size_mb: size_bytes as f64 / 1_048_576.0,
        width: img.width(),
        height: img.height(),
        format,
    })
}


// A bin/ folder is checked as well as the exe's own folder so a bundled
// layout works without copying the tools next to the executable
fn find_tool(name: &str) -> PathBuf {
    // Per platform because one bundle can hold both builds: Linux must never pick
    // up ffmpeg.exe, and Windows must never pick up ffmpeg.bin
    let file_names: Vec<String> = if cfg!(windows) {
        vec![format!("{}.exe", name)]
    } else {
        vec![format!("{}.bin", name), name.to_string()]
    };

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for base in [dir.to_path_buf(), dir.join("bin")] {
                for file_name in &file_names {
                    let candidate = base.join(file_name);
                    if candidate.is_file() {
                        ensure_executable(&candidate);
                        return candidate;
                    }
                }
            }
        }
    }
    PathBuf::from(name)
}

// Zip extraction, git checkouts and some file managers drop the executable bit, and
// spawning then fails with "Permission denied" even though the file is right there
#[cfg(unix)]
fn ensure_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o111 == 0 {
            let mut perms = meta.permissions();
            perms.set_mode(mode | 0o111);
            // Can fail in a read-only install location; spawning then reports the real error
            let _ = std::fs::set_permissions(path, perms);
        }
    }
}

#[cfg(not(unix))]
fn ensure_executable(_path: &Path) {}

fn ffmpeg_path() -> PathBuf {
    find_tool("ffmpeg")
}
fn ffprobe_path() -> PathBuf {
    find_tool("ffprobe")
}


#[derive(Deserialize)]
struct FfprobeOutput {
    format: FfprobeFormat,
    #[serde(default)]
    streams: Vec<FfprobeStream>,
}

#[derive(Deserialize)]
struct FfprobeFormat {
    #[serde(default)]
    duration: Option<String>,
    #[serde(default)]
    size: Option<String>,
}

#[derive(Deserialize, Default)]
struct FfprobeStream {
    codec_type: Option<String>,
    #[serde(default)]
    codec_name: Option<String>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    color_transfer: Option<String>,
    #[serde(default)]
    disposition: Option<FfprobeDisposition>,
    #[serde(default)]
    side_data_list: Vec<serde_json::Value>,
}

#[derive(Deserialize, Default)]
struct FfprobeDisposition {
    #[serde(default)]
    attached_pic: u8,
}


// Where FFmpeg failure logs get written 
//   Windows -> %LOCALAPPDATA%\VideoCompressor\logs
//   macOS   -> ~/Library/Logs/VideoCompressor
//   Linux   -> ~/.local/state/video-compressor/logs
// Falls back to the system temp dir if the above fail

fn error_log_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Ok(local_appdata) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(local_appdata).join("VideoCompressor").join("logs");
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join("Library").join("Logs").join("VideoCompressor");
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join(".local").join("state").join("video-compressor").join("logs");
        }
    }
    std::env::temp_dir().join("VideoCompressor").join("logs")
}

pub fn error_log_path() -> PathBuf {
    error_log_dir().join("error.txt")
}


// Append error rather than overwrite - I doubt this will actually take any real amount of storage but something to be aware of
fn write_error_log(ffmpeg: &Path, args: &[String], exit_code: Option<i32>, stderr: &str) -> Option<PathBuf> {
    let dir = error_log_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("error.txt");

    let unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let command_line = std::iter::once(ffmpeg.display().to_string())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");

    let exit_desc = exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "unknown (terminated by signal)".to_string());

    let entry = format!(
        "===== {unix_secs} (unix time) =====\n\
         Exit code: {exit_desc}\n\
         Command: {command_line}\n\
         --- ffmpeg output ---\n\
         {stderr}\n\n"
    );

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    file.write_all(entry.as_bytes()).ok()?;

    Some(path)
}

fn no_window_command(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

fn get_video_info(path: &Path) -> Result<VideoInfo, String> {
    let ffprobe = ffprobe_path();
    let output = no_window_command(&ffprobe)
        .args([
            "-v", "quiet",
            "-print_format", "json",
            "-show_format",
            "-show_streams",
            path.to_str().ok_or("Invalid file path")?,
        ])
        .output()
        .map_err(|e| {
            format!(
                "Could not run ffprobe ({}): {}. Place ffprobe next to the app (or in a bin folder there) or install FFmpeg.",
                ffprobe.display(), e
            )
        })?;

    if !output.status.success() {
        return Err("ffprobe failed - is this a valid video file?".to_string());
    }

    let probe: FfprobeOutput = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Could not parse ffprobe output: {}", e))?;

    let duration = probe
        .format
        .duration
        .as_deref()
        .and_then(|d| d.parse::<f64>().ok())
        .ok_or("Could not read video duration")?;

    if duration <= 0.0 {
        return Err("Video appears to have zero duration".to_string());
    }

    let size_bytes = probe
        .format
        .size
        .as_deref()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    // First real video stream - skips embedded cover art, which ffprobe also reports as video
    let is_video = |s: &&FfprobeStream| s.codec_type.as_deref() == Some("video");
    let vstream = probe
        .streams
        .iter()
        .filter(is_video)
        .find(|s| s.disposition.as_ref().map_or(true, |d| d.attached_pic == 0))
        .or_else(|| probe.streams.iter().find(is_video));

    let (width, height) = vstream
        .map(|s| (s.width.unwrap_or(0), s.height.unwrap_or(0)))
        .unwrap_or((0, 0));

    let codec = vstream
        .and_then(|s| s.codec_name.clone())
        .unwrap_or_default();

    let color_transfer = vstream
        .and_then(|s| s.color_transfer.clone())
        .filter(|t| t != "unknown" && !t.is_empty());

    let hdr = vstream.and_then(|s| parse_hdr_meta(&s.side_data_list));

    let audio_tracks = probe
        .streams
        .iter()
        .filter(|s| s.codec_type.as_deref() == Some("audio"))
        .count();

    Ok(VideoInfo {
        duration_secs: duration,
        size_mb: size_bytes as f64 / 1_048_576.0,
        width,
        height,
        codec,
        audio_tracks,
        color_transfer,
        hdr,
    })
}


// ffprobe prints rationals as "34000/50000" and plain numbers as JSON numbers
fn json_frac(v: &serde_json::Value, key: &str) -> Option<f64> {
    let x = v.get(key)?;
    if let Some(s) = x.as_str() {
        match s.split_once('/') {
            Some((n, d)) => {
                let n: f64 = n.trim().parse().ok()?;
                let d: f64 = d.trim().parse().ok()?;
                if d == 0.0 { None } else { Some(n / d) }
            }
            None => s.trim().parse().ok(),
        }
    } else {
        x.as_f64()
    }
}

// ffprobe reports these as fractions, but x264/x265 only accept integers in fixed
// units (chromaticity in 0.00002 steps, luminance in 0.0001 cd/m2 steps)
fn parse_hdr_meta(side_data: &[serde_json::Value]) -> Option<HdrStaticMeta> {
    let mut meta = HdrStaticMeta::default();

    for sd in side_data {
        let kind = sd.get("side_data_type").and_then(|v| v.as_str()).unwrap_or("");

        if kind.starts_with("Mastering display") {
            let c = |k: &str| json_frac(sd, k).map(|v| (v * 50_000.0).round() as i64);
            let l = |k: &str| json_frac(sd, k).map(|v| (v * 10_000.0).round() as i64);
            if let (
                Some(rx), Some(ry), Some(gx), Some(gy), Some(bx), Some(by),
                Some(wx), Some(wy), Some(max_l), Some(min_l),
            ) = (
                c("red_x"), c("red_y"), c("green_x"), c("green_y"), c("blue_x"), c("blue_y"),
                c("white_point_x"), c("white_point_y"), l("max_luminance"), l("min_luminance"),
            ) {
                meta.master_display = Some(format!(
                    "G({gx},{gy})B({bx},{by})R({rx},{ry})WP({wx},{wy})L({max_l},{min_l})"
                ));
            }
        } else if kind.starts_with("Content light") {
            if let (Some(a), Some(b)) = (json_frac(sd, "max_content"), json_frac(sd, "max_average")) {
                meta.max_cll = Some((a.round() as u32, b.round() as u32));
            }
        }
    }

    if meta.master_display.is_some() || meta.max_cll.is_some() {
        Some(meta)
    } else {
        None
    }
}

// HDR10 recorders usually put the metadata in the bitstream (per-frame SEI)
// rather than the container, so it only shows up on decoded frames
fn probe_first_frame_hdr(path: &Path) -> Option<HdrStaticMeta> {
    let out = no_window_command(&ffprobe_path())
        .args([
            "-v", "quiet",
            "-print_format", "json",
            "-select_streams", "V:0",
            "-read_intervals", "%+#1",
            "-show_frames",
            "-show_entries", "frame=side_data_list",
            path.to_str()?,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let list = v.get("frames")?.as_array()?.first()?.get("side_data_list")?.as_array()?;
    parse_hdr_meta(list)
}

// Recorders often write the metadata only into the bitstream, so the frame probe is
// the fallback when the container has none. Only HDR10 (PQ) has static metadata;
// HLG and SDR never do, so probing them would be wasted work
fn read_hdr_meta(info: &VideoInfo, path: &Path) -> Option<HdrStaticMeta> {
    if info.color_transfer.as_deref() != Some("smpte2084") {
        return None;
    }
    info.hdr.clone().or_else(|| probe_first_frame_hdr(path))
}


// 97% because when trying to do 100% the resulting file is often slightly above what the user wanted
fn target_video_kbps(target_mb: f64, duration_secs: f64, audio_kbps: u32, mute: bool) -> u32 {
    let effective = target_mb * 0.97;
    let target_bits = effective * 8.0 * 1_048_576.0;
    let audio_bits = if mute {
        0.0
    } else {
        audio_kbps as f64 * 1_000.0 * duration_secs
    };
    let video_bits = (target_bits - audio_bits).max(10_000.0);
    (video_bits / duration_secs / 1_000.0) as u32
}


fn run_ffmpeg(
    ffmpeg: &Path,
    input_args: &[String],
    output_args: &[String],
    duration: f64,
    cancel: &CancelToken,
    on_progress: impl Fn(f32) + Send + 'static,
) -> Result<(), String> {
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }

    let mut args: Vec<String> = vec![
        "-y".into(),
        "-progress".into(), "pipe:1".into(),
        // "error" rather than "quiet": quiet also silences real errors, which leaves the
        // error log's ffmpeg output section empty
        "-v".into(), "error".into(),
        "-nostats".into(),
    ];
    args.extend_from_slice(input_args);
    args.extend_from_slice(output_args);

    let mut child = no_window_command(ffmpeg)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            format!(
                "Failed to start ffmpeg ({}): {}. Place ffmpeg next to the app (or in a bin folder there) or install FFmpeg.",
                ffmpeg.display(), e
            )
        })?;

    // Must take stdout/stderr before the child moves into the mutex below
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // Stored here so the UI thread can kill it via cancel() - see the
    // polling loop below for why we don't just hold this and call wait()
    if let Ok(mut guard) = cancel.child.lock() {
        *guard = Some(child);
    }

    let progress_thread = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().flatten() {
            if let Some(ts) = line.strip_prefix("out_time=") {
                if let Some(secs) = parse_hhmmss(ts.trim()) {
                    if duration > 0.0 {
                        on_progress((secs / duration).clamp(0.0, 1.0) as f32);
                    }
                }
            }
        }
    });

    // ffmpeg's actual diagnostics (errors, codec complaints, etc.) all go to
    // stderr - captured here so a failure can be logged with real detail
    // instead of just "exited with an error"
    let stderr_thread = std::thread::spawn(move || {
        let mut captured = String::new();
        for line in BufReader::new(stderr).lines().flatten() {
            captured.push_str(&line);
            captured.push('\n');
        }
        captured
    });

    // Poll with try_wait() rather than a single blocking wait(): a blocking
    // wait() would hold the mutex the whole run, and cancel() would never
    // get a chance to lock it and kill the process. Polling keeps the mutex
    // free between checks so a Cancel click takes effect within one interval
    let status: std::io::Result<std::process::ExitStatus> = loop {
        let polled = {
            let mut guard = cancel.child.lock().unwrap();
            match guard.as_mut() {
                Some(c) => c.try_wait(),
                None => break Err(std::io::Error::other("child handle missing")),
            }
        };
        match polled {
            Ok(Some(s)) => break Ok(s),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => break Err(e),
        }
    };
    let _ = progress_thread.join();
    let captured_stderr = stderr_thread.join().unwrap_or_default();

    if let Ok(mut guard) = cancel.child.lock() {
        *guard = None;
    }

    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }

    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => {
            let log_path = write_error_log(ffmpeg, &args, s.code(), &captured_stderr);
            Err(match log_path {
                Some(p) => format!(
                    "FFmpeg exited with an error. Details saved to {}",
                    p.display()
                ),
                None => "FFmpeg exited with an error. The file may be unsupported or corrupt.".into(),
            })
        }
        Err(e) => Err(format!("FFmpeg process error: {}", e)),
    }
}

fn parse_hhmmss(s: &str) -> Option<f64> {
    if s.starts_with('-') {
        return Some(0.0);
    }
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h = parts[0].parse::<f64>().ok()?;
    let m = parts[1].parse::<f64>().ok()?;
    let sec = parts[2].parse::<f64>().ok()?;
    Some(h * 3600.0 + m * 60.0 + sec)
}


#[derive(Debug, Clone)]
pub struct CompressOptions {
    pub target_mb: f64,
    pub mute: bool,
    pub two_pass: bool,
    pub trim_start: Option<f64>,
    pub trim_end: Option<f64>,
    pub output_dir: Option<PathBuf>,
}

pub fn run_batch(
    files: Vec<PathBuf>,
    opts: CompressOptions,
    cancel: CancelToken,
    tx: Sender<EncoderMessage>,
) {
    let total = files.len();

    for (index, path) in files.into_iter().enumerate() {
        if cancel.is_cancelled() {
            let _ = tx.send(EncoderMessage::Cancelled);
            return;
        }

        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        let _ = tx.send(EncoderMessage::FileStart {
            index,
            total,
            name: name.clone(),
        });

        let result = if is_image_ext(&path) {
            compress_image_inner(&path, opts.target_mb, opts.output_dir.as_deref(), &cancel, &tx)
        } else {
            compress_video_inner(&path, &opts, &cancel, &tx)
        };

        if cancel.is_cancelled() {
            let _ = tx.send(EncoderMessage::Cancelled);
            return;
        }

        match result {
            Ok(Outcome::Done(out)) => {
                let _ = tx.send(EncoderMessage::FileDone(out));
            }
            Ok(Outcome::Skipped(reason)) => {
                let _ = tx.send(EncoderMessage::Skipped { name, reason });
            }
            Err(e) if e == "cancelled" => {
                let _ = tx.send(EncoderMessage::Cancelled);
                return;
            }
            Err(e) => {
                let _ = tx.send(EncoderMessage::FileError { name, error: e });
            }
        }
    }

    let _ = tx.send(EncoderMessage::AllDone);
}


// Falls back to the source folder if a custom output folder can't be
// created (e.g. it was on USB that's since been unplugged),
// so the encode still succeeds somewhere sensible
fn resolve_output_dir(input: &Path, output_dir: Option<&Path>) -> PathBuf {
    let source_dir = input.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();

    match output_dir {
        Some(dir) => {
            if std::fs::create_dir_all(dir).is_ok() {
                dir.to_path_buf()
            } else {
                source_dir
            }
        }
        None => source_dir,
    }
}


// Matched to the source so the app only shrinks the file instead of also changing what
// kind of file it is. Re-encoding a 10-bit HEVC HDR recording as H.264 High 10 is what
// produced files that browsers and Discord can't play
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VideoCodec {
    H264,
    Hevc,
    Vp9,
    Av1,
}

impl VideoCodec {
    fn pick(source_codec: &str, ext: &str) -> Self {
        let matched = match source_codec {
            "h264" => Some(VideoCodec::H264),
            "hevc" => Some(VideoCodec::Hevc),
            "vp9" => Some(VideoCodec::Vp9),
            "av1" => Some(VideoCodec::Av1),
            _ => None,
        };
        match (matched, ext) {
            // webm can only hold VP8/VP9/AV1
            (Some(VideoCodec::H264 | VideoCodec::Hevc) | None, "webm") => VideoCodec::Vp9,
            (Some(c), _) => c,
            // No encoder to match (mpeg4, wmv, mpeg2, ...): H.264 hits a size target
            // reliably and plays almost everywhere
            (None, _) => VideoCodec::H264,
        }
    }

    fn encoder(self) -> &'static str {
        match self {
            VideoCodec::H264 => "libx264",
            VideoCodec::Hevc => "libx265",
            VideoCodec::Vp9 => "libvpx-vp9",
            VideoCodec::Av1 => "libsvtav1",
        }
    }

    // SVT-AV1 has no two-pass mode in ffmpeg, so it always runs single-pass
    fn supports_two_pass(self) -> bool {
        self != VideoCodec::Av1
    }
}

fn audio_encoder_for(ext: &str) -> &'static str {
    match ext {
        "webm" => "libopus", // webm can't hold AAC
        _ => "aac",
    }
}

// x265-params is ':'-separated, so a Windows path like C:\Temp\x needs its
// colon escaped (and backslashes flipped) to survive parsing
fn x265_path(p: &str) -> String {
    p.replace('\\', "/").replace(':', "\\:")
}

struct VideoPlan {
    codec: VideoCodec,
    ext: String,
    kbps: u32,
    hdr: Option<HdrStaticMeta>,
}

impl VideoPlan {
    fn encode_args(&self, pass: Option<(u8, &str)>, mbtree_off: bool) -> Vec<String> {
        let mut a: Vec<String> = vec![
            // V (not v) skips embedded cover art, which would otherwise count as a video stream
            "-map".into(), "0:V:0".into(),
            // Keep the source's frame timing as-is. The default would duplicate frames to
            // fill a gap before the video starts (common when audio starts first), which
            // fabricates a frozen first frame AND makes pass 2 see more frames than pass 1
            "-fps_mode".into(), "passthrough".into(),
            "-c:v".into(), self.codec.encoder().into(),
            "-b:v".into(), format!("{}k", self.kbps),
        ];
        let mut params: Vec<String> = Vec::new();

        match self.codec {
            VideoCodec::H264 => {
                if mbtree_off {
                    params.push("mbtree=0".into());
                }
                // A plain re-encode drops HDR10 mastering metadata, so pass it through explicitly
                if let Some(h) = &self.hdr {
                    if let Some(md) = &h.master_display {
                        params.push(format!("mastering-display={}", md));
                    }
                    if let Some((cll, fall)) = h.max_cll {
                        params.push(format!("cll={},{}", cll, fall));
                    }
                }
                if let Some((n, log)) = pass {
                    a.extend(["-pass".into(), n.to_string(), "-passlogfile".into(), log.to_string()]);
                }
            }
            VideoCodec::Hevc => {
                params.push("log-level=error".into());
                if let Some(h) = &self.hdr {
                    if let Some(md) = &h.master_display {
                        params.push(format!("master-display={}", md));
                    }
                    if let Some((cll, fall)) = h.max_cll {
                        params.push(format!("max-cll={},{}", cll, fall));
                    }
                }
                // ffmpeg's -pass is silently ignored by libx265, so two-pass goes through x265 itself
                if let Some((n, log)) = pass {
                    params.push(format!("pass={}", n));
                    params.push(format!("stats={}", x265_path(log)));
                }
                // Apple players and some browsers only accept HEVC in mp4/mov tagged hvc1
                if matches!(self.ext.as_str(), "mp4" | "m4v" | "mov") {
                    a.extend(["-tag:v".into(), "hvc1".into()]);
                }
            }
            VideoCodec::Vp9 => {
                // Without these libvpx-vp9 is extremely slow
                a.extend(["-row-mt".into(), "1".into(), "-cpu-used".into(), "2".into()]);
                if let Some((n, log)) = pass {
                    a.extend(["-pass".into(), n.to_string(), "-passlogfile".into(), log.to_string()]);
                }
            }
            VideoCodec::Av1 => {}
        }

        if !params.is_empty() {
            let flag = if self.codec == VideoCodec::H264 { "-x264-params" } else { "-x265-params" };
            a.push(flag.into());
            a.push(params.join(":"));
        }
        a
    }
}

// Each codec leaves different side files (.mbtree, .cutree, ...), so they're matched by
// prefix rather than named individually, which would leak files when a codec adds one
fn remove_pass_files(passlog: &Path) {
    let (Some(dir), Some(prefix)) = (
        passlog.parent(),
        passlog.file_name().and_then(|n| n.to_str()),
    ) else {
        return;
    };
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(prefix) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

fn compress_video_inner(
    input: &Path,
    opts: &CompressOptions,
    cancel: &CancelToken,
    tx: &Sender<EncoderMessage>,
) -> Result<Outcome, String> {
    let ffmpeg = ffmpeg_path();

    let info = get_video_info(input)?;

    let start_secs = opts.trim_start.unwrap_or(0.0);
    let end_secs = opts.trim_end.unwrap_or(info.duration_secs);
    let effective_duration = (end_secs - start_secs).max(1.0);

    // Encoding to a target at or above the source size can't shrink anything, it only
    // costs quality. With a trim the comparison is against the trimmed portion, since
    // that's all that would be written
    let trimmed = opts.trim_start.is_some() || opts.trim_end.is_some();
    let range_secs = (end_secs - start_secs).clamp(0.0, info.duration_secs);
    let range_mb = info.size_mb * range_secs / info.duration_secs;
    if info.size_mb > 0.0 && opts.target_mb >= range_mb {
        return Ok(Outcome::Skipped(format!(
            "target size ({} MB) is larger than the {} ({:.1} MB)",
            opts.target_mb,
            if trimmed { "selected range" } else { "source" },
            range_mb
        )));
    }

    // Every track is kept, so each one needs its own share of the size budget
    const AUDIO_KBPS: u32 = 128;
    let audio_budget_kbps = AUDIO_KBPS * info.audio_tracks as u32;
    let video_kbps = target_video_kbps(opts.target_mb, effective_duration, audio_budget_kbps, opts.mute);

    let stem = input.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let ext = input.extension().unwrap_or_default().to_string_lossy().to_string();
    let ext = if ext.is_empty() { "mp4".to_string() } else { ext };
    let ext_lower = ext.to_lowercase();
    // The app compresses, it doesn't convert, so the source's container is kept
    let output = resolve_output_dir(input, opts.output_dir.as_deref())
        .join(format!("{}_{:.0}mb.{}", stem, opts.target_mb, ext));

    let codec = VideoCodec::pick(&info.codec, &ext_lower);
    let src_hdr = read_hdr_meta(&info, input);
    let plan = VideoPlan {
        codec,
        ext: ext_lower.clone(),
        kbps: video_kbps,
        hdr: src_hdr.clone(),
    };

    let mut input_args: Vec<String> = Vec::new();
    if let Some(start) = opts.trim_start {
        input_args.push("-ss".into());
        input_args.push(format!("{}", start));
    }
    input_args.push("-i".into());
    input_args.push(input.to_str().unwrap_or("").to_string());
    if opts.trim_end.is_some() {
        input_args.push("-t".into());
        input_args.push(format!("{}", end_secs - start_secs));
    }

    // ffmpeg's default mapping silently keeps only one audio track, so map them all.
    // The `?` stops videos with no audio from erroring
    let mut audio_args: Vec<String> = if opts.mute {
        vec!["-an".into()]
    } else {
        vec![
            "-map".into(), "0:a?".into(),
            "-c:a".into(), audio_encoder_for(&ext_lower).into(),
            "-b:a".into(), format!("{}k", AUDIO_KBPS),
        ]
    };
    // mkv can carry any subtitle format as-is; other containers can't take a plain copy
    if ext_lower == "mkv" {
        audio_args.extend(["-map".into(), "0:s?".into(), "-c:s".into(), "copy".into()]);
    }

    let passlog = std::env::temp_dir().join(format!(
        "vc_pass_{}_{}",
        stem,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));
    let passlog_str = passlog.to_string_lossy().to_string();
    let name = input.file_name().unwrap_or_default().to_string_lossy().to_string();

    let result = if opts.two_pass && codec.supports_two_pass() {
        run_two_pass(
            &ffmpeg, &input_args, &audio_args, &output, &passlog_str,
            &plan, effective_duration, cancel, tx, &name,
        )
    } else {
        run_single_pass(
            &ffmpeg, &input_args, &audio_args, &output,
            &plan, effective_duration, cancel, tx,
        )
    };

    remove_pass_files(&passlog);

    if let Err(e) = &result {
        if e == "cancelled" {
            // Cancel now kills ffmpeg immediately, mid-write, so whatever it
            // managed to flush to `output` is an incomplete/corrupt file -
            // clean it up rather than leaving it sitting in the output folder
            let _ = std::fs::remove_file(&output);
        }
    }

    if result.is_ok() {
        // Notes only. The encode already succeeded, so nothing here may block, prompt or fail it
        let expected_secs = (end_secs - start_secs).max(0.0);
        for text in verify_output(&info, &src_hdr, codec, &output, opts, expected_secs) {
            append_log_note(&name, &text);
            let _ = tx.send(EncoderMessage::OutputNote { name: name.clone(), text });
        }
    }

    result.map(|_| Outcome::Done(output))
}

// Unhandled edge cases tend to show up as silent differences from the source rather than
// as errors, so this reports them instead of waiting for a user to notice
fn verify_output(
    src: &VideoInfo,
    src_hdr: &Option<HdrStaticMeta>,
    codec: VideoCodec,
    output: &Path,
    opts: &CompressOptions,
    expected_secs: f64,
) -> Vec<String> {
    let mut notes = Vec::new();
    let out = match get_video_info(output) {
        Ok(i) => i,
        Err(_) => return notes,
    };

    if out.size_mb > opts.target_mb {
        notes.push(format!(
            "output is {:.2} MB, which is over the {:.0} MB target",
            out.size_mb, opts.target_mb
        ));
    }
    if !opts.mute && out.audio_tracks != src.audio_tracks {
        notes.push(format!(
            "output has {} audio track(s), source had {}",
            out.audio_tracks, src.audio_tracks
        ));
    }
    if (out.width, out.height) != (src.width, src.height) {
        notes.push(format!(
            "resolution changed from {}x{} to {}x{}",
            src.width, src.height, out.width, out.height
        ));
    }
    if (out.duration_secs - expected_secs).abs() > 1.0 {
        notes.push(format!(
            "duration is {:.1}s, expected about {:.1}s",
            out.duration_secs, expected_secs
        ));
    }
    if src.color_transfer.is_some() && out.color_transfer != src.color_transfer {
        notes.push(format!(
            "color transfer changed from {} to {}",
            src.color_transfer.as_deref().unwrap_or("?"),
            out.color_transfer.as_deref().unwrap_or("none")
        ));
    }
    // Only x264/x265 can be handed HDR metadata, so only check where we could have preserved it
    if src_hdr.is_some()
        && matches!(codec, VideoCodec::H264 | VideoCodec::Hevc)
        && read_hdr_meta(&out, output).is_none()
    {
        notes.push("HDR mastering metadata was not carried over".to_string());
    }
    notes
}

fn append_log_note(file: &str, text: &str) {
    let dir = error_log_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("error.txt"))
    {
        let _ = writeln!(f, "===== {unix_secs} (unix time) =====\nNote on {file}: {text}\n");
    }
}

// Two-pass rate control can fail on some sources. x264 fails with
// "Incomplete MB-tree stats file" - pass 1 and pass 2 each decode the
// source independently, and if they land on different frame counts (VFR
// timestamps, odd B-frame reordering, etc.) pass 2 runs past the end of
// pass 1's per-frame stats. It's a source-file quirk, not an app bug, and
// it's not reliably detectable up front - so on any two-pass failure this
// retries once. For x264 the retry switches MB-tree off, which removes the
// cross-pass dependency at the cost of slightly less efficient rate control;
// other codecs have no equivalent switch, so they retry as single-pass
fn run_two_pass(
    ffmpeg: &Path,
    input_args: &[String],
    audio_args: &[String],
    output: &Path,
    passlog: &str,
    plan: &VideoPlan,
    duration: f64,
    cancel: &CancelToken,
    tx: &Sender<EncoderMessage>,
    file_name: &str,
) -> Result<(), String> {
    match run_two_pass_attempt(
        ffmpeg, input_args, audio_args, output, passlog, plan, duration, cancel, tx, false,
    ) {
        Ok(()) => Ok(()),
        Err(e) if e == "cancelled" => Err(e),
        Err(first_err) => {
            let is_x264 = plan.codec == VideoCodec::H264;
            let retry_desc = if is_x264 { "without MB-tree analysis" } else { "as a single-pass encode" };

            let _ = tx.send(EncoderMessage::FallbackUsed {
                name: file_name.to_string(),
                reason: first_err.clone(),
                retry: retry_desc.to_string(),
            });

            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }

            let retry = if is_x264 {
                run_two_pass_attempt(
                    ffmpeg, input_args, audio_args, output, passlog, plan, duration, cancel, tx, true,
                )
            } else {
                run_single_pass(ffmpeg, input_args, audio_args, output, plan, duration, cancel, tx)
            };

            retry.map_err(|retry_err| {
                if retry_err == "cancelled" {
                    retry_err
                } else {
                    format!(
                        "{} (retry {} also failed: {})",
                        first_err, retry_desc, retry_err
                    )
                }
            })
        }
    }
}

fn run_two_pass_attempt(
    ffmpeg: &Path,
    input_args: &[String],
    audio_args: &[String],
    output: &Path,
    passlog: &str,
    plan: &VideoPlan,
    duration: f64,
    cancel: &CancelToken,
    tx: &Sender<EncoderMessage>,
    disable_mbtree: bool,
) -> Result<(), String> {
    let mut pass1_out = plan.encode_args(Some((1, passlog)), disable_mbtree);
    pass1_out.extend_from_slice(&[
        "-an".into(),
        "-f".into(), "null".into(),
        if cfg!(windows) { "NUL".into() } else { "/dev/null".into() },
    ]);

    let tx1 = tx.clone();
    run_ffmpeg(ffmpeg, input_args, &pass1_out, duration, cancel, move |p| {
        let _ = tx1.send(EncoderMessage::Pass1Progress(p));
    })?;
    let _ = tx.send(EncoderMessage::Pass1Progress(1.0));

    let mut pass2_out = plan.encode_args(Some((2, passlog)), disable_mbtree);
    pass2_out.extend_from_slice(audio_args);
    pass2_out.push(output.to_str().unwrap_or("output.mp4").to_string());

    let tx2 = tx.clone();
    run_ffmpeg(ffmpeg, input_args, &pass2_out, duration, cancel, move |p| {
        let _ = tx2.send(EncoderMessage::Pass2Progress(p));
    })?;
    let _ = tx.send(EncoderMessage::Pass2Progress(1.0));

    Ok(())
}

fn run_single_pass(
    ffmpeg: &Path,
    input_args: &[String],
    audio_args: &[String],
    output: &Path,
    plan: &VideoPlan,
    duration: f64,
    cancel: &CancelToken,
    tx: &Sender<EncoderMessage>,
) -> Result<(), String> {
    let mut out_args = plan.encode_args(None, false);
    out_args.extend_from_slice(audio_args);
    out_args.push(output.to_str().unwrap_or("output.mp4").to_string());

    let tx = tx.clone();
    run_ffmpeg(ffmpeg, input_args, &out_args, duration, cancel, move |p| {
        let _ = tx.send(EncoderMessage::SingleProgress(p));
    })
}


fn compress_image_inner(
    input: &Path,
    target_mb: f64,
    output_dir: Option<&Path>,
    cancel: &CancelToken,
    tx: &Sender<EncoderMessage>,
) -> Result<Outcome, String> {
    let target_bytes = (target_mb * 1_048_576.0) as usize;

    // Already at or under the target - re-encoding would only degrade it (or grow it)
    let source_bytes = std::fs::metadata(input).map(|m| m.len()).unwrap_or(0) as usize;
    if source_bytes > 0 && target_bytes >= source_bytes {
        return Ok(Outcome::Skipped(format!(
            "target size ({} MB) is larger than the source ({:.2} MB)",
            target_mb,
            source_bytes as f64 / 1_048_576.0
        )));
    }

    let img = image::open(input).map_err(|e| format!("Could not open image: {}", e))?;

    let stem = input.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let output = resolve_output_dir(input, output_dir)
        .join(format!("{}_{:.0}mb.jpg", stem, target_mb));

    let _ = tx.send(EncoderMessage::SingleProgress(0.1));

    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }

    let size_at_q1 = encode_jpeg_bytes(&img, 1).len();

    let (final_img, best_quality) = if size_at_q1 <= target_bytes {
        let quality = binary_search_quality(&img, target_bytes, cancel, tx)?;
        (img, quality)
    } else {
        let _ = tx.send(EncoderMessage::SingleProgress(0.3));
        let scaled = scale_to_fit(&img, target_bytes);
        let _ = tx.send(EncoderMessage::SingleProgress(0.8));
        (scaled, 1u8)
    };

    let _ = tx.send(EncoderMessage::SingleProgress(0.95));

    let bytes = encode_jpeg_bytes(&final_img, best_quality);
    std::fs::write(&output, &bytes).map_err(|e| format!("Could not write output: {}", e))?;

    Ok(Outcome::Done(output))
}

fn binary_search_quality(
    img: &image::DynamicImage,
    target_bytes: usize,
    cancel: &CancelToken,
    tx: &Sender<EncoderMessage>,
) -> Result<u8, String> {
    const LO_BOUND: u8 = 1;
    const HI_BOUND: u8 = 95;
    // Max iterations a binary search over this range can take (ceil(log2(range)), ≈7)
    const MAX_ITERS: f32 = (u8::BITS - (HI_BOUND - LO_BOUND).leading_zeros()) as f32;

    let mut lo: u8 = LO_BOUND;
    let mut hi: u8 = HI_BOUND;
    let mut best: u8 = LO_BOUND;
    let mut iter = 0u8;

    while lo <= hi {
        if cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        let mid = lo + (hi - lo) / 2;
        iter += 1;
        let progress = 0.1 + (iter as f32 / MAX_ITERS) * 0.8;
        let _ = tx.send(EncoderMessage::SingleProgress(progress));

        let size = encode_jpeg_bytes(img, mid).len();
        if size <= target_bytes {
            best = mid;
            if mid == 95 {
                break;
            }
            lo = mid + 1;
        } else {
            if mid == 0 {
                break;
            }
            hi = mid - 1;
        }
    }

    Ok(best)
}

fn encode_jpeg_bytes(img: &image::DynamicImage, quality: u8) -> Vec<u8> {
    let mut buf = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    let _ = img.write_with_encoder(encoder);
    buf
}

fn scale_to_fit(img: &image::DynamicImage, target_bytes: usize) -> image::DynamicImage {
    // Raw pixel bytes as a cheap stand-in for encoded size while searching for
    // a scale factor - real JPEG encoding only happens once, at the end
    let channels = img.color().channel_count() as u32;
    let raw_size_at = |w: u32, h: u32| (w * h * channels) as usize;

    let mut scale = 0.75f32;
    let mut best_w = img.width();
    let mut best_h = img.height();

    for _ in 0..20 {
        let w = (img.width() as f32 * scale) as u32;
        let h = (img.height() as f32 * scale) as u32;
        if w == 0 || h == 0 {
            break;
        }
        best_w = w;
        best_h = h;
        // Assumes JPEG at quality 1 compresses to ~15% of raw size
        if raw_size_at(w, h) / 7 <= target_bytes {
            break;
        }
        scale *= 0.75;
    }

    img.resize(best_w, best_h, image::imageops::FilterType::Lanczos3)
}