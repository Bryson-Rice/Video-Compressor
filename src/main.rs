#![windows_subsystem = "windows"]

// TODO: make one thing handle which file types or supported rather than manually updating 3 different locations

use eframe::egui::{self, Color32, RichText, Sense, Stroke, Vec2};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

mod encoder;
use encoder::{
    CancelToken, CompressOptions, EncoderMessage, FileInfo,
};

#[cfg(target_os = "windows")]
mod windows_shell;

fn main() -> eframe::Result<()> {
    // Runs unconditionally on every launch (not gated on is_registered())
    // so a partial/stale registration from an older build self-heals
    // instead of getting stuck forever. Cheap HKCU-only writes
    #[cfg(target_os = "windows")]
    {
        if let Ok(exe) = std::env::current_exe() {
            let _ = windows_shell::register(&exe);
        }
    }

    // "Open with" / drop-on-exe pass the file path as argv[1]; grab it
    // before the event loop starts so it can be pre-loaded
    let initial_path: Option<std::path::PathBuf> =
        std::env::args_os().nth(1).map(Into::into);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Video Compressor")
            .with_inner_size([580.0, 680.0])
            .with_min_inner_size([500.0, 540.0])
            .with_drag_and_drop(true)
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        "Video Compressor",
        options,
        Box::new(move |cc| {
            let mut visuals = egui::Visuals::dark();
            visuals.override_text_color = Some(Color32::from_gray(220));
            visuals.window_rounding = 8.0.into();
            visuals.widgets.noninteractive.rounding = 6.0.into();
            visuals.widgets.inactive.rounding = 6.0.into();
            visuals.widgets.hovered.rounding = 6.0.into();
            visuals.widgets.active.rounding = 6.0.into();
            cc.egui_ctx.set_visuals(visuals);

            let mut style = (*cc.egui_ctx.style()).clone();
            style.spacing.item_spacing = egui::vec2(8.0, 6.0);
            style.spacing.button_padding = egui::vec2(12.0, 6.0);
            cc.egui_ctx.set_style(style);

            let mut app = App::default();

            // Registry settings only exist on Windows (loaded in App::default());
            // eframe's cross-platform storage covers these two so custom MB
            // targets also persist on macOS/Linux
            if let Some(storage) = cc.storage {
                if let Some(s) = storage.get_string("custom_mb_video") {
                    if let Ok(v) = s.parse::<f64>() {
                        let v = v.clamp(0.1, 2000.0);
                        app.custom_mb_video = v;
                        app.custom_mb_video_str = v.to_string();
                    }
                }
                if let Some(s) = storage.get_string("custom_mb_image") {
                    if let Ok(v) = s.parse::<f64>() {
                        let v = v.clamp(0.1, 2000.0);
                        app.custom_mb_image = v;
                        app.custom_mb_image_str = v.to_string();
                    }
                }
            }

            if let Some(path) = initial_path {
                app.load_path(path);
            }
            Ok(Box::new(app))
        }),
    )
}


#[derive(Debug, Clone, PartialEq, Default)]
enum TargetSize {
    #[default]
    MB8,
    MB25,
    MB50,
    MB100,
    Custom,
}

impl TargetSize {
    fn label(&self) -> &'static str {
        match self {
            Self::MB8 => "8 MB",
            Self::MB25 => "25 MB",
            Self::MB50 => "50 MB",
            Self::MB100 => "100 MB",
            Self::Custom => "Custom",
        }
    }
    fn value(&self, custom: f64) -> f64 {
        match self {
            Self::MB8 => 8.0,
            Self::MB25 => 25.0,
            Self::MB50 => 50.0,
            Self::MB100 => 100.0,
            Self::Custom => custom,
        }
    }
}


const VIDEO_EXTS: &[&str] = &["mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "ts", "mts"];
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "bmp", "gif"];


#[derive(Debug, Clone, PartialEq, Default)]
enum AppState {
    #[default]
    Idle,
    Encoding,
    Done,
    Cancelled,
}

struct App {
    // Input - a single file, or a folder scanned for supported files
    queued_files: Vec<PathBuf>,
    single_info: Option<FileInfo>,      // only set when exactly one file loaded
    source_label: String,

    // Settings
    target_size: TargetSize,
    custom_mb_video: f64,  // tracked separately from image so switching file types doesn't fuck up either
    custom_mb_video_str: String,
    custom_mb_image: f64,
    custom_mb_image_str: String,
    mute: bool,
    two_pass: bool,
    trim_start_str: String,
    trim_end_str: String,

    // Encoding state
    state: AppState,
    cancel_token: CancelToken,
    rx: Option<Receiver<EncoderMessage>>,

    // Progress
    pass1_progress: f32,
    pass2_progress: f32,
    file_progress: f32,           // combined 0..1 estimate across both passes
    current_file_index: usize,
    total_files: usize,
    current_file_name: String,
    current_file_is_image: bool,  // drives whether the two-pass UI applies

    // Results
    completed: Vec<PathBuf>,
    errors: Vec<String>,
    error: Option<String>,        // load-time error, distinct from per-file encode errors

    // `None` = write output next to the source file 
    output_dir: Option<PathBuf>,
}

impl App {
    // (custom_mb_video, custom_mb_image, mute, two_pass, output_dir)
    // Falls back to defaults on non-Windows builds or first run
    fn load_saved_settings() -> (f64, f64, bool, bool, Option<PathBuf>) {
        #[cfg(target_os = "windows")]
        {
            let s = windows_shell::load_settings();
            (s.custom_mb_video, s.custom_mb_image, s.mute, s.two_pass, s.output_dir)
        }
        #[cfg(not(target_os = "windows"))]
        { (50.0, 5.0, false, true, None) }
    }

    // No-op on non-Windows builds - nowhere durable to put settings there yet
    fn persist_settings(&self) {
        #[cfg(target_os = "windows")]
        windows_shell::save_settings(&windows_shell::AppSettings {
            custom_mb_video: self.custom_mb_video,
            custom_mb_image: self.custom_mb_image,
            mute: self.mute,
            two_pass: self.two_pass,
            output_dir: self.output_dir.clone(),
        });
    }
}

impl Default for App {
    fn default() -> Self {
        let (custom_mb_video, custom_mb_image, mute, two_pass, output_dir) =
            Self::load_saved_settings();

        Self {
            queued_files: Vec::new(),
            single_info: None,
            source_label: String::new(),
            target_size: TargetSize::MB8,
            custom_mb_video,
            custom_mb_video_str: mb_to_str(custom_mb_video),
            custom_mb_image,
            custom_mb_image_str: mb_to_str(custom_mb_image),
            mute,
            two_pass,
            trim_start_str: String::new(),
            trim_end_str: String::new(),
            state: AppState::Idle,
            cancel_token: CancelToken::new(),
            rx: None,
            pass1_progress: 0.0,
            pass2_progress: 0.0,
            file_progress: 0.0,
            current_file_index: 0,
            total_files: 0,
            current_file_name: String::new(),
            current_file_is_image: false,
            completed: Vec::new(),
            errors: Vec::new(),
            error: None,
            output_dir,
        }
    }
}


impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_encoder(ctx);
        self.handle_drops(ctx);

        // Must be declared before CentralPanel so it pins to the viewport
        // bottom regardless of the central panel's scroll position
        egui::TopBottomPanel::bottom("output_footer")
            .show_separator_line(true)
            .show(ctx, |ui| {
                self.render_output_folder(ui);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.render(ui);
            });
        });

        if self.state == AppState::Encoding {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

}


impl App {
    fn poll_encoder(&mut self, ctx: &egui::Context) {
        let mut terminal = None;

        if let Some(rx) = &self.rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    EncoderMessage::FileStart { index, total, name } => {
                        self.current_file_is_image =
                            encoder::is_image_ext(std::path::Path::new(&name));
                        self.current_file_index = index;
                        self.total_files = total;
                        self.current_file_name = name;
                        self.pass1_progress = 0.0;
                        self.pass2_progress = 0.0;
                        self.file_progress = 0.0;
                    }
                    EncoderMessage::Pass1Progress(p) => {
                        self.pass1_progress = p;
                        self.file_progress = p * 0.45; // pass 1 
                    }
                    EncoderMessage::Pass2Progress(p) => {
                        self.pass2_progress = p;
                        self.file_progress = 0.45 + p * 0.55; // pass 2
                    }
                    EncoderMessage::SingleProgress(p) => {
                        self.file_progress = p;
                        self.pass2_progress = p;
                    }
                    EncoderMessage::FileDone(path) => {
                        self.completed.push(path);
                        self.file_progress = 1.0;
                    }
                    EncoderMessage::FileError { name, error } => {
                        self.errors.push(format!("{}: {}", name, error));
                    }
                    EncoderMessage::AllDone => {
                        terminal = Some(AppState::Done);
                    }
                    EncoderMessage::Cancelled => {
                        terminal = Some(AppState::Cancelled);
                    }
                }
                ctx.request_repaint();
            }
        }

        if let Some(new_state) = terminal {
            self.state = new_state;
            self.rx = None;
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect()
        });
        match dropped.len() {
            0 => {}
            1 => self.load_path(dropped.into_iter().next().unwrap()),
            _ => {
                self.error = Some(
                    "Please drop one file or one folder at a time.".to_string(),
                );
            }
        }
    }

    fn load_path(&mut self, path: PathBuf) {
        self.error = None;
        self.single_info = None;
        self.queued_files.clear();

        let files = encoder::collect_files(&path);

        if files.is_empty() {
            self.error = Some(
                if path.is_dir() {
                    "No supported video or image files found in that folder.".to_string()
                } else {
                    "Unsupported file type. Supported: MP4, MKV, AVI, MOV, WebM, JPG, PNG, BMP, GIF.".to_string()
                },
            );
            return;
        }

        self.source_label = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        // Detailed info is only worth fetching for a single file - the batch view doesn't need it
        if files.len() == 1 {
            match encoder::get_file_info(&files[0]) {
                Ok(info) => self.single_info = Some(info),
                Err(e) => {
                    self.error = Some(e);
                    return;
                }
            }
        }

        self.queued_files = files;
    }

    fn open_file_dialog(&mut self) {
        let all_exts: Vec<&str> = VIDEO_EXTS.iter().chain(IMAGE_EXTS.iter()).copied().collect();
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Videos & Images", &all_exts)
            .add_filter("Video files", VIDEO_EXTS)
            .add_filter("Image files", IMAGE_EXTS)
            .pick_file()
        {
            self.load_path(path);
        }
    }

    fn open_folder_dialog(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            self.load_path(path);
        }
    }

    fn start_encoding(&mut self) {
        if self.queued_files.is_empty() {
            return;
        }

        let is_image_only = self.queued_files.iter().all(|p| encoder::is_image_ext(p));
        let custom_mb = if is_image_only { self.custom_mb_image } else { self.custom_mb_video };

        let files = self.queued_files.clone();
        let opts = CompressOptions {
            target_mb: self.target_size.value(custom_mb),
            mute: self.mute,
            two_pass: self.two_pass,
            trim_start: parse_timecode(&self.trim_start_str),
            trim_end: parse_timecode(&self.trim_end_str),
            output_dir: self.output_dir.clone(),
        };

        let cancel = CancelToken::new();
        self.cancel_token = cancel.clone();

        self.persist_settings();

        let (tx, rx) = std::sync::mpsc::channel();
        self.rx = Some(rx);
        self.state = AppState::Encoding;
        self.completed.clear();
        self.errors.clear();
        self.pass1_progress = 0.0;
        self.pass2_progress = 0.0;
        self.file_progress = 0.0;
        self.current_file_index = 0;
        self.total_files = files.len();
        self.current_file_name = String::new();
        self.current_file_is_image = false;

        std::thread::spawn(move || {
            encoder::run_batch(files, opts, cancel, tx);
        });
    }
}


impl App {
    fn render(&mut self, ui: &mut egui::Ui) {
        ui.add_space(16.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("Video Compressor").size(26.0).strong());
        });
        ui.add_space(14.0);
        ui.separator();
        ui.add_space(14.0);

        match self.state {
            AppState::Encoding => self.render_encoding_screen(ui),
            _ => {
                self.render_main_screen(ui);
                // Results show below the drop zone rather than replacing it,
                // so loading a new file doesn't require dismissing anything
                if matches!(self.state, AppState::Done | AppState::Cancelled) {
                    self.render_results_panel(ui);
                }
            }
        }    }


    fn render_main_screen(&mut self, ui: &mut egui::Ui) {
        let is_hovering = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());

        let desired = Vec2::new(ui.available_width().min(520.0), 150.0);
        ui.vertical_centered(|ui| {
            let (rect, resp) = ui.allocate_exact_size(desired, Sense::click());

            let bg = if is_hovering {
                Color32::from_rgba_unmultiplied(80, 130, 220, 30)
            } else if resp.hovered() {
                Color32::from_gray(40)
            } else {
                Color32::from_gray(28)
            };
            let border = if is_hovering {
                Color32::from_rgb(100, 160, 255)
            } else {
                Color32::from_gray(70)
            };

            ui.painter().rect(rect, 10.0, bg, Stroke::new(2.0, border));

            ui.allocate_ui_at_rect(rect, |ui| {
                ui.set_min_size(rect.size());
                ui.vertical_centered(|ui| {
                    ui.add_space(20.0);

                    if self.queued_files.is_empty() {
                        ui.label(RichText::new("Drag & drop a file or folder here").size(14.0));
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(
                                "Videos: MP4 MKV AVI MOV WebM ...    Images: JPG PNG BMP GIF",
                            )
                            .color(Color32::from_gray(130))
                            .size(11.0),
                        );
                    } else if self.queued_files.len() == 1 {
                        ui.label(
                            RichText::new("OK")
                                .size(18.0)
                                .color(Color32::from_rgb(100, 220, 100)),
                        );
                        ui.label(
                            RichText::new(&self.source_label)
                                .strong()
                                .size(14.0),
                        );
                        if let Some(info) = &self.single_info {
                            ui.label(
                                RichText::new(file_info_summary(info))
                                    .color(Color32::from_gray(160))
                                    .size(12.0),
                            );
                        }
                    } else {
                        ui.label(
                            RichText::new("OK")
                                .size(18.0)
                                .color(Color32::from_rgb(100, 220, 100)),
                        );
                        ui.label(RichText::new(&self.source_label).strong().size(14.0));
                        ui.label(
                            RichText::new(format!(
                                "{} files queued",
                                self.queued_files.len()
                            ))
                            .color(Color32::from_gray(160))
                            .size(12.0),
                        );
                        ui.label(
                            RichText::new("Each file will be compressed to the target size individually.")
                                .color(Color32::from_gray(130))
                                .size(11.0),
                        );
                    }
                });
            });

            if resp.clicked() {
                self.open_file_dialog();
            }
        });

        ui.add_space(6.0);
        ui.vertical_centered(|ui| {
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new("Browse file...").min_size(Vec2::new(130.0, 28.0)))
                    .clicked()
                {
                    self.open_file_dialog();
                }
                if ui
                    .add(egui::Button::new("Browse folder...").min_size(Vec2::new(130.0, 28.0)))
                    .clicked()
                {
                    self.open_folder_dialog();
                }
            });
        });

        if let Some(err) = &self.error.clone() {
            ui.add_space(6.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new(format!("Error: {}", err))
                        .color(Color32::from_rgb(240, 100, 80))
                        .size(12.0),
                );
            });
        }

        if !self.queued_files.is_empty() {
            ui.add_space(20.0);
            ui.separator();
            ui.add_space(14.0);
            self.render_settings(ui);
        }
    }

    fn render_settings(&mut self, ui: &mut egui::Ui) {
        let is_image_only = self.queued_files.iter().all(|p| encoder::is_image_ext(p));
        let is_batch = self.queued_files.len() > 1;

        ui.label(RichText::new("Target Size").strong().size(13.0));
        ui.add_space(6.0);

        ui.horizontal_wrapped(|ui| {
            for size in [
                TargetSize::MB8,
                TargetSize::MB25,
                TargetSize::MB50,
                TargetSize::MB100,
                TargetSize::Custom,
            ] {
                let selected = self.target_size == size;
                let fill = if selected {
                    Color32::from_rgb(50, 120, 200)
                } else {
                    Color32::from_gray(50)
                };
                let text_color = if selected { Color32::WHITE } else { Color32::from_gray(200) };
                if ui
                    .add(
                        egui::Button::new(RichText::new(size.label()).color(text_color))
                            .fill(fill)
                            .min_size(Vec2::new(70.0, 30.0)),
                    )
                    .clicked()
                {
                    self.target_size = size;
                }
            }
        });

        if self.target_size == TargetSize::Custom {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("Size (MB):");
                if is_image_only {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.custom_mb_image_str)
                            .desired_width(80.0)
                            .hint_text("e.g. 5"),
                    );
                    if r.changed() {
                        if let Ok(v) = self.custom_mb_image_str.parse::<f64>() {
                            self.custom_mb_image = v.clamp(0.1, 2000.0);
                        }
                    }
                } else {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.custom_mb_video_str)
                            .desired_width(80.0)
                            .hint_text("e.g. 50"),
                    );
                    if r.changed() {
                        if let Ok(v) = self.custom_mb_video_str.parse::<f64>() {
                            self.custom_mb_video = v.clamp(0.1, 2000.0);
                        }
                    }
                }
            });
        }

        // Video options (hidden for image-only queues)
        if !is_image_only {
            ui.add_space(16.0);
            egui::CollapsingHeader::new(RichText::new("Options").size(13.0))
                .default_open(false)
                .show(ui, |ui| {
                    ui.add_space(4.0);
                    ui.checkbox(&mut self.mute, "Remove all audio (mute)");
                    ui.checkbox(
                        &mut self.two_pass,
                        "Two-pass encoding  (better quality, ~2x slower)",
                    );
                    if !is_batch {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new("Trim (optional)")
                                .color(Color32::from_gray(150))
                                .size(11.0),
                        );
                        ui.add_space(4.0);
                        egui::Grid::new("trim_grid")
                            .num_columns(2)
                            .spacing([12.0, 4.0])
                            .show(ui, |ui| {
                                ui.label("Start (HH:MM:SS)");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.trim_start_str)
                                        .desired_width(130.0)
                                        .hint_text("00:00:00"),
                                );
                                ui.end_row();
                                ui.label("End   (HH:MM:SS)");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.trim_end_str)
                                        .desired_width(130.0)
                                        .hint_text("blank = full"),
                                );
                                ui.end_row();
                            });
                    }
                });
        }

        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            let label = if is_batch {
                format!("Compress {} Files!", self.queued_files.len())
            } else {
                "Compress".to_string()
            };
            if ui
                .add(
                    egui::Button::new(RichText::new(&label).size(17.0).color(Color32::WHITE))
                        .fill(Color32::from_rgb(34, 139, 87))
                        .min_size(Vec2::new(200.0, 44.0)),
                )
                .clicked()
            {
                self.start_encoding();
            }
        });
    }

    fn render_output_folder(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            let label = match &self.output_dir {
                Some(dir) => format!("Output folder: {}", dir.display()),
                None => "Output folder: same as source file".to_string(),
            };
            ui.label(
                RichText::new(label)
                    .color(Color32::from_gray(150))
                    .size(11.0),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("Change...").color(Color32::WHITE),
                        )
                        .fill(Color32::from_rgb(50, 100, 180))
                        .min_size(Vec2::new(90.0, 24.0)),
                    )
                    .clicked()
                {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        self.output_dir = Some(path);
                        self.persist_settings();
                    }
                }

                ui.add_space(6.0);

                // lets the user get back to same as source
                let reset_enabled = self.output_dir.is_some();
                if ui
                    .add_enabled(
                        reset_enabled,
                        egui::Button::new(
                            RichText::new("Reset").color(Color32::from_gray(200)),
                        )
                        .fill(Color32::from_gray(50))
                        .min_size(Vec2::new(70.0, 24.0)),
                    )
                    .clicked()
                {
                    self.output_dir = None;
                    self.persist_settings();
                }

                ui.add_space(6.0);

                let open_enabled = self.output_dir.is_some();
                if ui
                    .add_enabled(
                        open_enabled,
                        egui::Button::new(
                            RichText::new("Open").color(Color32::from_gray(200)),
                        )
                        .fill(Color32::from_gray(50))
                        .min_size(Vec2::new(70.0, 24.0)),
                    )
                    .clicked()
                {
                    if let Some(dir) = &self.output_dir {
                        open_in_explorer(dir);
                    }
                }
            });
        });

        ui.add_space(6.0);
    }


    fn render_encoding_screen(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(30.0);
            ui.label(RichText::new("Compressing...").size(20.0).strong());
            ui.add_space(6.0);
            ui.label(
                RichText::new(&self.current_file_name)
                    .color(Color32::from_gray(160))
                    .size(12.0),
            );
            ui.add_space(24.0);

            let bar_width = (ui.available_width() - 40.0).min(460.0);

            if self.total_files > 1 {
                let done = self.completed.len() + self.errors.len();
                let overall = done as f32 / self.total_files as f32;

                ui.label(
                    RichText::new(format!(
                        "Overall: file {} of {}  ({} done, {} errors)",
                        self.current_file_index + 1,
                        self.total_files,
                        self.completed.len(),
                        self.errors.len(),
                    ))
                    .size(12.0)
                    .color(Color32::from_gray(190)),
                );
                ui.add(
                    egui::ProgressBar::new(overall)
                        .desired_width(bar_width)
                        .fill(Color32::from_rgb(80, 80, 180)),
                );
                ui.add_space(16.0);
            }

            let is_two_pass = self.two_pass && !self.current_file_is_image;

            if is_two_pass {
                ui.label(
                    RichText::new(format!("Pass 1 - {:.0}%", self.pass1_progress * 100.0))
                        .size(12.0)
                        .color(Color32::from_gray(180)),
                );
                ui.add(
                    egui::ProgressBar::new(self.pass1_progress)
                        .desired_width(bar_width)
                        .animate(false)
                        .fill(Color32::from_rgb(60, 110, 180)),
                );
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!("Pass 2 - {:.0}%", self.pass2_progress * 100.0))
                        .size(12.0)
                        .color(Color32::from_gray(180)),
                );
                ui.add(
                    egui::ProgressBar::new(self.pass2_progress)
                        .desired_width(bar_width)
                        .animate(false)
                        .fill(Color32::from_rgb(34, 139, 87)),
                );
            } else {
                ui.label(
                    RichText::new(format!("Current file - {:.0}%", self.file_progress * 100.0))
                        .size(12.0)
                        .color(Color32::from_gray(180)),
                );
                ui.add(
                    egui::ProgressBar::new(self.file_progress)
                        .desired_width(bar_width)
                        .animate(false)
                        .fill(Color32::from_rgb(34, 139, 87)),
                );
            }

            ui.add_space(24.0);

            if ui
                .add(
                    egui::Button::new(RichText::new("Cancel").color(Color32::WHITE))
                        .fill(Color32::from_rgb(180, 60, 50))
                        .min_size(Vec2::new(120.0, 34.0)),
                )
                .clicked()
            {
                self.cancel_token.cancel();
            }
        });
    }

    fn render_results_panel(&mut self, ui: &mut egui::Ui) {
        let was_cancelled = self.state == AppState::Cancelled;

        ui.add_space(16.0);
        ui.separator();
        ui.add_space(12.0);

        ui.horizontal(|ui| {
            let header = if was_cancelled { "Last Compression  (cancelled)" } else { "Last Compression" };
            ui.label(RichText::new(header).strong().size(13.0));

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("Clear").color(Color32::from_gray(180)))
                            .fill(Color32::from_gray(45))
                            .min_size(Vec2::new(60.0, 22.0)),
                    )
                    .clicked()
                {
                    self.completed.clear();
                    self.errors.clear();
                    self.state = AppState::Idle;
                }
            });
        });

        ui.add_space(8.0);

        egui::Frame::none()
            .fill(Color32::from_gray(28))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(12.0))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());

                ui.label(
                    RichText::new(format!(
                        "{} compressed,  {} failed",
                        self.completed.len(),
                        self.errors.len(),
                    ))
                    .strong()
                    .size(13.0),
                );

                if !self.completed.is_empty() {
                    ui.add_space(8.0);
                    for out in &self.completed {
                        if let Ok(meta) = std::fs::metadata(out) {
                            let size_mb = meta.len() as f64 / 1_048_576.0;
                            ui.label(
                                RichText::new(format!(
                                    "  {}  ->  {:.2} MB",
                                    out.file_name().unwrap_or_default().to_string_lossy(),
                                    size_mb
                                ))
                                .color(Color32::from_rgb(120, 210, 130))
                                .size(12.0),
                            );
                        }
                    }
                }

                if !self.errors.is_empty() {
                    ui.add_space(6.0);
                    for err in &self.errors {
                        ui.label(
                            RichText::new(format!("  Failed: {}", err))
                                .color(Color32::from_rgb(230, 100, 80))
                                .size(12.0),
                        );
                    }
                }
            });

        if let Some(first) = self.completed.first() {
            if let Some(parent) = first.parent() {
                let parent = parent.to_path_buf();
                ui.add_space(8.0);
                if ui
                    .add(
                        egui::Button::new("Open Output Folder")
                            .min_size(Vec2::new(180.0, 30.0)),
                    )
                    .clicked()
                {
                    open_in_explorer(&parent);
                }
            }
        }

        ui.add_space(10.0);
    }
}


// Opens the given folder in the OS's default file explorer
// if the command fails to spawn, this silently does nothing
fn open_in_explorer(path: &Path) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

// Converts an f64 MB value to a display string without unnecessary decimal places
// `50.0` → `"50"`, `1.5` → `"1.5"`, `150.0` → `"150"`
fn mb_to_str(mb: f64) -> String {
    if mb.fract() == 0.0 {
        format!("{}", mb as u64)
    } else {
        format!("{}", mb)
    }
}

fn file_info_summary(info: &FileInfo) -> String {
    match info {
        FileInfo::Video(v) => format!(
            "{:.1} MB  |  {}  |  {}x{}",
            v.size_mb,
            format_duration(v.duration_secs),
            v.width,
            v.height
        ),
        FileInfo::Image(i) => format!("{:.2} MB  |  {}x{}  |  {}", i.size_mb, i.width, i.height, i.format),
    }
}

fn format_duration(secs: f64) -> String {
    let total = secs as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{:02}:{:02}:{:02}", h, m, s)
    } else {
        format!("{:02}:{:02}", m, s)
    }
}

fn parse_timecode(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split(':').collect();
    match parts.len() {
        1 => parts[0].parse().ok(),
        2 => Some(parts[0].parse::<f64>().ok()? * 60.0 + parts[1].parse::<f64>().ok()?),
        3 => Some(
            parts[0].parse::<f64>().ok()? * 3600.0
                + parts[1].parse::<f64>().ok()? * 60.0
                + parts[2].parse::<f64>().ok()?,
        ),
        _ => None,
    }
}