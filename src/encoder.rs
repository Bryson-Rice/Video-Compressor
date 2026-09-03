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
    FileDone(PathBuf),
    FileError { name: String, error: String },
    AllDone,
    Cancelled,
}


// Active ffmpeg child stored here too so cancel() can kill it immediately instead of waiting for it to notice the flag
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

// Scans one level deep only - not dealing with recursive folder walks for this
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


fn find_tool(name: &str) -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let win = dir.join(format!("{}.exe", name));
            if win.exists() {
                return win;
            }
            let plain = dir.join(name);
            if plain.exists() {
                return plain;
            }
        }
    }
    PathBuf::from(name)
}

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
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
}


// Follows each OS's normal spot for this instead of dumping into the app folder or temp dir:
//   Windows -> %LOCALAPPDATA%\VideoCompressor\logs
//   macOS   -> ~/Library/Logs/VideoCompressor
//   Linux   -> ~/.local/state/video-compressor/logs
// Falls back to the temp dir if the relevant env var isn't set, so a missing var never blocks an encode
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

// pub so the UI can wire up an "open error log" button instead of making the user hunt for the path
pub fn error_log_path() -> PathBuf {
    error_log_dir().join("error.txt")
}

// Appends rather than overwrites so a retry's outcome doesn't erase the original failure, usually the more useful one. Best-effort - a failed write (no permissions, etc.) never fails the encode itself
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

// Without this ffmpeg would flash a console window on Windows, since this is a GUI app spawning a console program - no-op elsewhere
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
                "Could not run ffprobe ({}): {}. Place ffprobe.exe next to the app or install FFmpeg.",
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

    let (width, height) = probe
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"))
        .map(|s| (s.width.unwrap_or(0), s.height.unwrap_or(0)))
        .unwrap_or((0, 0));

    Ok(VideoInfo {
        duration_secs: duration,
        size_mb: size_bytes as f64 / 1_048_576.0,
        width,
        height,
    })
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
                "Failed to start ffmpeg ({}): {}. Place ffmpeg.exe next to the app or install FFmpeg.",
                ffmpeg.display(), e
            )
        })?;

    // Must take stdout/stderr before the child moves into the mutex below
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // Stored here so the UI thread can kill it via cancel() - see the polling loop below for why we don't just call wait()
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

    // ffmpeg's real diagnostics go to stderr, not stdout - captured here so a failure can be logged with actual detail instead of just "exited with an error"
    let stderr_thread = std::thread::spawn(move || {
        let mut captured = String::new();
        for line in BufReader::new(stderr).lines().flatten() {
            captured.push_str(&line);
            captured.push('\n');
        }
        captured
    });

    // Polls with try_wait() instead of a blocking wait() - a blocking wait() would hold the mutex the whole run and cancel() could never lock it to kill the process, so polling frees the mutex between checks and Cancel takes effect within one interval
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
            Ok(out) => {
                let _ = tx.send(EncoderMessage::FileDone(out));
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


// Falls back to the source folder if the custom output dir can't be created (e.g. a USB drive that's since been unplugged) - the encode should still land somewhere sensible
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


fn compress_video_inner(
    input: &Path,
    opts: &CompressOptions,
    cancel: &CancelToken,
    tx: &Sender<EncoderMessage>,
) -> Result<PathBuf, String> {
    let ffmpeg = ffmpeg_path();

    let info = get_video_info(input)?;

    let start_secs = opts.trim_start.unwrap_or(0.0);
    let end_secs = opts.trim_end.unwrap_or(info.duration_secs);
    let effective_duration = (end_secs - start_secs).max(1.0);

    const AUDIO_KBPS: u32 = 128;
    let video_kbps = target_video_kbps(opts.target_mb, effective_duration, AUDIO_KBPS, opts.mute);

    let stem = input.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let ext = input.extension().unwrap_or_default().to_string_lossy().to_string();
    let ext = if ext.is_empty() { "mp4".to_string() } else { ext };
    let output = resolve_output_dir(input, opts.output_dir.as_deref())
        .join(format!("{}_{:.0}mb.{}", stem, opts.target_mb, ext));

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

    let audio_args: Vec<String> = if opts.mute {
        vec!["-an".into()]
    } else {
        vec![
            "-c:a".into(), "aac".into(),
            "-b:a".into(), format!("{}k", AUDIO_KBPS),
        ]
    };

    let passlog = std::env::temp_dir().join(format!(
        "vc_pass_{}_{}",
        stem,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));
    let passlog_str = passlog.to_string_lossy().to_string();

    let result = if opts.two_pass {
        run_two_pass(
            &ffmpeg, &input_args, &audio_args, &output, &passlog_str,
            video_kbps, effective_duration, cancel, tx,
        )
    } else {
        run_single_pass(
            &ffmpeg, &input_args, &audio_args, &output,
            video_kbps, effective_duration, cancel, tx,
        )
    };

    let _ = std::fs::remove_file(format!("{}-0.log", passlog_str));
    let _ = std::fs::remove_file(format!("{}-0.log.mbtree", passlog_str));

    if let Err(e) = &result {
        if e == "cancelled" {
            // Cancel kills ffmpeg mid-write, so whatever's in `output` is an incomplete/corrupt file - clean it up rather than leaving it in the output folder
            let _ = std::fs::remove_file(&output);
        }
    }

    result.map(|_| output)
}

fn run_two_pass(
    ffmpeg: &Path,
    input_args: &[String],
    audio_args: &[String],
    output: &Path,
    passlog: &str,
    video_kbps: u32,
    duration: f64,
    cancel: &CancelToken,
    tx: &Sender<EncoderMessage>,
) -> Result<(), String> {
    let pass1_out: Vec<String> = vec![
        "-c:v".into(), "libx264".into(),
        "-b:v".into(), format!("{}k", video_kbps),
        "-pass".into(), "1".into(),
        "-passlogfile".into(), passlog.to_string(),
        "-an".into(),
        "-f".into(), "null".into(),
        if cfg!(windows) { "NUL".into() } else { "/dev/null".into() },
    ];

    let tx1 = tx.clone();
    run_ffmpeg(ffmpeg, input_args, &pass1_out, duration, cancel, move |p| {
        let _ = tx1.send(EncoderMessage::Pass1Progress(p));
    })?;
    let _ = tx.send(EncoderMessage::Pass1Progress(1.0));

    let mut pass2_out: Vec<String> = vec![
        "-c:v".into(), "libx264".into(),
        "-b:v".into(), format!("{}k", video_kbps),
        "-pass".into(), "2".into(),
        "-passlogfile".into(), passlog.to_string(),
    ];
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
    video_kbps: u32,
    duration: f64,
    cancel: &CancelToken,
    tx: &Sender<EncoderMessage>,
) -> Result<(), String> {
    let mut out_args: Vec<String> = vec![
        "-c:v".into(), "libx264".into(),
        "-b:v".into(), format!("{}k", video_kbps),
    ];
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
) -> Result<PathBuf, String> {
    let target_bytes = (target_mb * 1_048_576.0) as usize;

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

    Ok(output)
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
    // Raw pixel bytes stand in for encoded size while searching for a scale factor - actually encoding JPEG at every step would be far too slow, so real encoding only happens once at the end
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