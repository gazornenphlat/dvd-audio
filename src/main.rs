#![windows_subsystem = "windows"]
//! DVD Audio Grabber: finds the audio tracks on a DVD and saves them as files.

mod dvd;
mod mkv;
mod settings;
mod theme;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use eframe::egui;

/// Frames drawn before the `--screenshot` picture is taken.
const SHOT_FRAMES: u32 = 120;

/// The formats in the menu, with the names shown to the person.
const FORMATS: [(dvd::Format, &str); 6] = [
    (dvd::Format::Mkv, "MKV (one file per track)"),
    (dvd::Format::Raw, "Raw streams (.ac3, .dts, .mpa, .pcm)"),
    (dvd::Format::Wav, "WAV (16 and 24-bit PCM need no ffmpeg)"),
    (dvd::Format::Mp3, "MP3 (needs ffmpeg)"),
    (dvd::Format::Ogg, "OGG (needs ffmpeg)"),
    (dvd::Format::Flac, "FLAC (needs ffmpeg)"),
];

/// The file name pattern used until the person changes it.
const DEFAULT_NAME: &str = "track{n}";

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let shot = args
        .windows(2)
        .find(|pair| pair[0] == "--screenshot")
        .map(|pair| PathBuf::from(&pair[1]));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DVD Audio Grabber")
            .with_inner_size([900.0, 680.0])
            .with_icon(window_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "DVD Audio Grabber",
        options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(App::new(shot)))
        }),
    )
}

struct Row {
    number: usize,
    info: dvd::TrackInfo,
    bytes: u64,
    /// The data carries a real audio signature; false usually means copy-protected data.
    found: bool,
    selected: bool,
}

/// Messages from the background threads (scan and extract) back to the window.
enum Msg {
    Scanned(Result<dvd::Scan, dvd::Error>),
    Progress(dvd::Progress),
    Extracted(Result<dvd::Report, dvd::Error>),
}

struct App {
    folder: String,
    out_dir: String,
    format: dvd::Format,
    name: String,
    rows: Vec<Row>,
    /// One line about the disc that was scanned, such as "DVD-Audio disc · 3 file(s) · 4 track(s)".
    disc: String,
    status: String,
    busy: bool,
    /// Progress of a conversion, 0 to 1; None when no conversion is running.
    progress: Option<f32>,
    cancel: Arc<AtomicBool>,
    /// Whether ffmpeg was found when the app started.
    ffmpeg: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    shot: Option<PathBuf>,
    frames: u32,
}

impl App {
    fn new(shot: Option<PathBuf>) -> Self {
        let (tx, rx) = mpsc::channel();
        let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
        let saved = settings::load(settings::Settings {
            folder: String::new(),
            out_dir: format!("{home}\\Music"),
            format: dvd::Format::Wav,
            name: DEFAULT_NAME.into(),
        });
        App {
            folder: saved.folder,
            out_dir: saved.out_dir,
            format: saved.format,
            name: saved.name,
            rows: Vec::new(),
            disc: String::new(),
            status: "Pick the DVD's folder, then press Scan.".into(),
            busy: false,
            progress: None,
            cancel: Arc::new(AtomicBool::new(false)),
            ffmpeg: dvd::ffmpeg_available(),
            tx,
            rx,
            shot,
            frames: 0,
        }
    }

    fn save_settings(&self) {
        settings::save(&settings::Settings {
            folder: self.folder.clone(),
            out_dir: self.out_dir.clone(),
            format: self.format,
            name: self.name.clone(),
        });
    }

    fn start_scan(&mut self) {
        self.save_settings();
        self.busy = true;
        self.progress = None;
        self.rows.clear();
        self.disc.clear();
        self.status = "Scanning the disc. A big disc can take a minute.".into();
        self.cancel.store(false, Ordering::Relaxed);
        let folder = PathBuf::from(self.folder.trim());
        let tx = self.tx.clone();
        let cancel = Arc::clone(&self.cancel);
        thread::spawn(move || {
            let result = dvd::scan(&folder, &cancel);
            let _ = tx.send(Msg::Scanned(result));
        });
    }

    fn start_extract(&mut self) {
        self.save_settings();
        let wanted: Vec<(usize, dvd::TrackKey)> = self
            .rows
            .iter()
            .filter(|row| row.selected)
            .map(|row| (row.number, row.info.key))
            .collect();
        let total: u64 = self.rows.iter().filter(|row| row.selected).map(|row| row.bytes).sum();
        self.busy = true;
        self.progress = Some(0.0);
        self.status = "Converting…".into();
        self.cancel.store(false, Ordering::Relaxed);
        let folder = PathBuf::from(self.folder.trim());
        let out = PathBuf::from(self.out_dir.trim());
        let format = self.format;
        let name = self.name.trim().to_string();
        let tx = self.tx.clone();
        let cancel = Arc::clone(&self.cancel);
        thread::spawn(move || {
            let mut progress = |p: dvd::Progress| {
                let _ = tx.send(Msg::Progress(p));
            };
            let result = dvd::extract(&folder, &out, &wanted, format, &name, total, &cancel, &mut progress);
            let _ = tx.send(Msg::Extracted(result));
        });
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Scanned(Ok(scan)) => {
                let format = self.format;
                self.disc = format!("{} disc · {} file(s) · {} track(s)", scan.kind, scan.files, scan.tracks.len());
                self.rows = scan
                    .tracks
                    .into_iter()
                    .enumerate()
                    .map(|(i, found)| Row {
                        number: i + 1,
                        selected: found.audio && dvd::convertible(&found.info, format),
                        found: found.audio,
                        info: found.info,
                        bytes: found.bytes,
                    })
                    .collect();
                self.busy = false;
                self.status = if self.rows.is_empty() {
                    "No audio found. The folder may not hold a DVD-Audio disc, or the disc may be copy-protected, which this app can't read.".into()
                } else {
                    format!("Found {} audio track(s). Tick the ones you want.", self.rows.len())
                };
            }
            Msg::Scanned(Err(dvd::Error::Cancelled)) => {
                self.busy = false;
                self.status = "Scan cancelled.".into();
            }
            Msg::Scanned(Err(e)) => {
                self.busy = false;
                self.status = format!("Could not scan: {e}");
            }
            Msg::Progress(p) => {
                let fraction = p.fraction;
                self.status = p.text;
                self.progress = Some(fraction);
            }
            Msg::Extracted(Ok(report)) => {
                self.busy = false;
                self.progress = None;
                self.status = report_text(&report);
            }
            Msg::Extracted(Err(dvd::Error::Cancelled)) => {
                self.busy = false;
                self.progress = None;
                self.status = "Cancelled. Files from this run were removed.".into();
            }
            Msg::Extracted(Err(e)) => {
                self.busy = false;
                self.progress = None;
                self.status = format!("Could not convert: {e}");
            }
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.save_settings();
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg);
        }
        if self.busy {
            // Background work doesn't wake the window, so keep redrawing to show its progress.
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        // A folder dropped on the window, or a file from inside one, becomes the DVD folder.
        if !self.busy {
            let dropped = ctx.input(|input| input.raw.dropped_files.first().and_then(|file| file.path.clone()));
            if let Some(path) = dropped {
                let folder = if path.is_dir() {
                    path
                } else {
                    path.parent().map(Path::to_path_buf).unwrap_or_default()
                };
                self.folder = folder.display().to_string();
            }
        }

        let mut scan = false;
        let mut extract = false;
        let mut cancel = false;
        let mut browse_folder = false;
        let mut browse_out = false;
        let format_before = self.format;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(egui::RichText::new("DVD AUDIO GRABBER").color(theme::NAVY));
            ui.label("Drop a DVD folder here, or choose it. Scan it, tick the audio you want, pick a format, then convert.");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("DVD folder:");
                ui.add(egui::TextEdit::singleline(&mut self.folder).desired_width(400.0));
                if ui.button("Browse…").clicked() {
                    browse_folder = true;
                }
            });
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.busy, egui::Button::new("Scan for audio")).clicked() {
                    scan = true;
                }
                if ui.add_enabled(self.busy, egui::Button::new("Cancel")).clicked() {
                    cancel = true;
                }
            });
            if !self.disc.is_empty() {
                ui.label(self.disc.as_str());
            }
            ui.separator();
            if self.rows.is_empty() {
                ui.label("No audio tracks yet.");
            }
            theme::card(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("tracks").max_height(280.0).auto_shrink([false, false]).show(ui, |ui| {
                    for row in &mut self.rows {
                        let reason = dvd::unsupported_reason(&row.info, self.format);
                        let warning = dvd::track_warning(&row.info, self.format);
                        let label = row_label(row, reason.as_deref(), warning.as_deref());
                        ui.add_enabled(reason.is_none(), egui::Checkbox::new(&mut row.selected, label));
                    }
                });
            });
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Save to:");
                ui.add(egui::TextEdit::singleline(&mut self.out_dir).desired_width(400.0));
                if ui.button("Browse…").clicked() {
                    browse_out = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label("Format:");
                let shown = FORMATS.iter().find(|(format, _)| *format == self.format).map_or("", |(_, name)| name);
                egui::ComboBox::from_id_salt("format").selected_text(shown).show_ui(ui, |ui| {
                    for (format, name) in FORMATS {
                        ui.selectable_value(&mut self.format, format, name);
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label("File name:");
                ui.add(egui::TextEdit::singleline(&mut self.name).desired_width(260.0));
                ui.weak("{n} number, {codec}, {rate} kHz, {bits} bit, {ch} channels");
            });
            if !self.ffmpeg {
                ui.colored_label(
                    warning_color(),
                    "ffmpeg was not found. MP3, OGG and FLAC need it, and so do WAV files of MLP, AC-3 and 20-bit PCM. Install ffmpeg, then restart this app.",
                );
            }
            let any_selected = self.rows.iter().any(|row| row.selected);
            let needs_ffmpeg = self.rows.iter().any(|row| row.selected && dvd::needs_ffmpeg(&row.info, self.format));
            let can_convert = !self.busy && any_selected && (self.ffmpeg || !needs_ffmpeg);
            ui.horizontal(|ui| {
                if theme::primary(ui, can_convert, "Convert selected").clicked() {
                    extract = true;
                }
            });
            if let Some(fraction) = self.progress {
                ui.add(egui::ProgressBar::new(fraction).show_percentage());
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if self.busy {
                    ui.spinner();
                }
            });
            theme::card(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("status").max_height(200.0).auto_shrink([false, true]).show(ui, |ui| {
                    ui.label(self.status.as_str());
                });
            });
        });

        if format_before != self.format {
            // Tracks the new format can't take are unticked.
            for row in &mut self.rows {
                row.selected &= dvd::convertible(&row.info, self.format);
            }
        }
        if browse_folder {
            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                self.folder = path.display().to_string();
            }
        }
        if browse_out {
            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                self.out_dir = path.display().to_string();
            }
        }
        if cancel {
            self.cancel.store(true, Ordering::Relaxed);
            self.status = "Cancelling…".into();
        }
        if scan {
            self.start_scan();
        }
        if extract {
            self.start_extract();
        }

        if let Some(path) = self.shot.clone() {
            self.frames += 1;
            if self.frames == SHOT_FRAMES {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
            }
            let shot = ctx.input(|input| {
                input.events.iter().find_map(|event| match event {
                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
            });
            if let Some(image) = shot {
                if let Err(e) = save_png(&image, &path) {
                    eprintln!("could not save the picture: {e}");
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                self.shot = None;
            } else if self.frames > SHOT_FRAMES + 600 {
                eprintln!("no picture arrived from the window");
                std::process::exit(1);
            }
            ctx.request_repaint();
        }
    }
}

fn warning_color() -> egui::Color32 {
    theme::ORANGE_HOVER
}

/// The DVD picture, shown on the title bar and taskbar.
fn window_icon() -> egui::IconData {
    image::load_from_memory(include_bytes!("../icon/dvd_icon.png"))
        .map(|picture| {
            let picture = picture.to_rgba8();
            let (width, height) = picture.dimensions();
            egui::IconData { rgba: picture.into_raw(), width, height }
        })
        .unwrap_or_default()
}

/// One line per track: what it is, and why it can't be ticked if it can't.
fn row_label(row: &Row, reason: Option<&str>, warning: Option<&str>) -> String {
    let mut parts = vec![format!("Track {}", row.number), row.info.codec.name().to_string()];
    if let Some(rate) = row.info.sample_rate {
        parts.push(format!("{} kHz", dvd::khz(rate)));
    }
    if let Some(bits) = row.info.bits {
        parts.push(format!("{bits}-bit"));
    }
    if let Some(channels) = row.info.channels {
        parts.push(format!("{channels} ch"));
    }
    parts.push(dvd::size_text(row.bytes));
    if let Some(reason) = reason {
        parts.push(format!("can't convert: {reason}"));
    }
    if let Some(warning) = warning {
        parts.push(format!("warning: {warning}"));
    }
    if !row.found {
        parts.push("no audio found: maybe copy-protected".to_string());
    }
    parts.join(" · ")
}

/// The result of a conversion, one file or problem per line.
fn report_text(report: &dvd::Report) -> String {
    let mut lines = Vec::new();
    if report.saved.is_empty() {
        lines.push("Nothing was saved.".to_string());
    } else {
        lines.push(format!("Saved {} file(s):", report.saved.len()));
        lines.extend(report.saved.iter().map(|line| format!("  {line}")));
    }
    if !report.skipped.is_empty() {
        lines.push("Skipped:".to_string());
        lines.extend(report.skipped.iter().map(|line| format!("  {line}")));
    }
    if !report.failed.is_empty() {
        lines.push("Failed:".to_string());
        lines.extend(report.failed.iter().map(|line| format!("  {line}")));
    }
    lines.join("\n")
}

fn save_png(image: &egui::ColorImage, path: &Path) -> Result<(), String> {
    let pixels: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
    let [width, height] = image.size;
    image::RgbaImage::from_raw(width as u32, height as u32, pixels)
        .ok_or("the picture size did not match")?
        .save(path)
        .map_err(|e| e.to_string())
}
