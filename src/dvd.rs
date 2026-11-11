//! Finds the audio inside a DVD-Audio or DVD-Video disc and saves each track in the chosen format.
//! Matroska and the raw streams are written here; WAV, MP3, OGG and FLAC are made by ffmpeg.
//! Only the container is read. Copy-protected (encrypted) discs are not decrypted.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::mkv::{self, Mkv};

/// Bytes read from disk at a time.
const READ_CHUNK: usize = 1 << 20;

/// How much audio to wait through looking for an AC-3 frame header before giving up on a track.
const AC3_SEARCH_LIMIT: usize = 1 << 20;

/// Audio read between progress reports while converting.
const PROGRESS_STEP: u64 = 4 << 20;

/// Bytes of each track checked for an audio signature while scanning.
const PROBE_BYTES: usize = 16 << 10;

/// Bitrate for the MP3 and OGG files; the original's default (see MECHANICS.md).
const CONVERT_BITRATE: &str = "128k";

/// AC-3 bitrates in kbit/s, by frame size code / 2.
const AC3_KBPS: [u32; 19] = [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640];

/// Full-bandwidth channels for each AC-3 audio coding mode.
const AC3_CHANNELS: [u8; 8] = [2, 1, 2, 3, 3, 4, 4, 5];

/// What to save each track as. Mkv and Raw are written here; the rest go through ffmpeg.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Mkv,
    Raw,
    Wav,
    Mp3,
    Ogg,
    Flac,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrackKey {
    pub stream: u8,
    pub sub: u8,
    /// Which track of a DVD-Audio AOB, from the IFO's sector ranges (0 = the whole stream).
    pub part: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Ac3,
    Dts,
    Lpcm,
    Mpeg,
    Mlp,
    TrueHd,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::Ac3 => "Dolby Digital (AC-3)",
            Codec::Dts => "DTS",
            Codec::Lpcm => "PCM (uncompressed)",
            Codec::Mpeg => "MPEG audio",
            Codec::Mlp => "MLP",
            Codec::TrueHd => "TrueHD",
        }
    }
}

#[derive(Clone, Debug)]
pub struct TrackInfo {
    pub key: TrackKey,
    pub codec: Codec,
    pub sample_rate: Option<u32>,
    pub channels: Option<u8>,
    pub bits: Option<u8>,
}

/// What the walker reports: a new track was seen, or some bytes of a track's audio.
pub enum Event<'a> {
    Track(&'a TrackInfo),
    Data(TrackKey, &'a [u8]),
}

/// Bytes of the console-window flag, so ffmpeg runs without a window for each track.
const NO_WINDOW: u32 = 0x0800_0000;

const FFMPEG_MISSING: &str = "ffmpeg is needed for this format. Install it and make sure it's on the PATH.";

impl Format {
    /// The name this format is saved under in the settings file.
    pub fn key(self) -> &'static str {
        match self {
            Format::Mkv => "mkv",
            Format::Raw => "raw",
            Format::Wav => "wav",
            Format::Mp3 => "mp3",
            Format::Ogg => "ogg",
            Format::Flac => "flac",
        }
    }

    pub fn from_key(text: &str) -> Option<Format> {
        [Format::Mkv, Format::Raw, Format::Wav, Format::Mp3, Format::Ogg, Format::Flac]
            .into_iter()
            .find(|format| format.key() == text)
    }
}

/// What went wrong, for the window to show.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Ffmpeg(String),
    /// The person pressed Cancel. Files from that run are removed.
    Cancelled,
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Ffmpeg(text) => write!(f, "{text}"),
            Error::Cancelled => write!(f, "Cancelled."),
        }
    }
}

/// A progress report: what is happening, and how far through the chosen tracks it is (0 to 1).
pub struct Progress {
    pub text: String,
    pub fraction: f32,
}

/// What an extraction did: the files it saved, the tracks it skipped on purpose, and the ones that failed.
#[derive(Default)]
pub struct Report {
    pub saved: Vec<String>,
    pub skipped: Vec<String>,
    pub failed: Vec<String>,
}

/// What a scan found on a disc.
pub struct Scan {
    /// "DVD-Audio", "DVD-Video" or "Blu-ray".
    pub kind: &'static str,
    /// How many disc files were read.
    pub files: usize,
    pub tracks: Vec<Found>,
}

/// One track found by a scan.
pub struct Found {
    pub info: TrackInfo,
    /// Bytes of audio on the disc for this track.
    pub bytes: u64,
    /// The data carries a real audio signature; false usually means copy-protected data.
    pub audio: bool,
}

/// Whether ffmpeg runs. MP3, OGG and FLAC need it, and so does WAV for MLP, AC-3 and 20-bit PCM.
pub fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").creation_flags(NO_WINDOW).output().is_ok_and(|run| run.status.success())
}

/// Whether saving this track in this format runs ffmpeg.
pub fn needs_ffmpeg(info: &TrackInfo, format: Format) -> bool {
    match format {
        Format::Mp3 | Format::Ogg | Format::Flac => true,
        Format::Wav => !(info.codec == Codec::Lpcm && matches!(info.bits, Some(16) | Some(24))),
        Format::Mkv | Format::Raw => false,
    }
}

/// Whether a track can be saved in this format. Matroska only takes AC-3 and PCM here, and PCM needs its bit depth, rate and channels.
pub fn convertible(info: &TrackInfo, format: Format) -> bool {
    let pcm_known = info.bits.is_some() && info.sample_rate.is_some() && info.channels.is_some();
    match (format, info.codec) {
        (Format::Raw, _) => true,
        (Format::Mkv, Codec::Ac3) => true,
        (Format::Mkv, Codec::Lpcm) => pcm_known && matches!(info.bits, Some(16) | Some(24)),
        (Format::Mkv, _) => false,
        (_, Codec::Lpcm) => pcm_known,
        _ => true,
    }
}

/// Why a track can't be saved in this format, or None if it can.
pub fn unsupported_reason(info: &TrackInfo, format: Format) -> Option<String> {
    if convertible(info, format) {
        return None;
    }
    let text = match (format, info.codec) {
        (Format::Mkv, Codec::Lpcm) if info.bits == Some(20) => "20-bit PCM doesn't fit in MKV. Choose WAV, FLAC, MP3, OGG or Raw".to_string(),
        (_, Codec::Lpcm) => "the PCM settings could not be read from the disc. Raw still works".to_string(),
        (Format::Mkv, _) => "MKV holds only AC-3 and PCM here".to_string(),
        _ => "not available in this format".to_string(),
    };
    Some(text)
}

/// A sample rate in kHz as people write it: 48, 44.1, 96.
pub fn khz(rate: u32) -> String {
    if rate % 1000 == 0 {
        (rate / 1000).to_string()
    } else {
        format!("{:.1}", rate as f64 / 1000.0)
    }
}

/// The file name for a track: `template` with its placeholders filled in, then the extension.
/// Placeholders: {n} track number (01), {codec} (PCM, MLP, AC3, DTS, MPEG, TrueHD), {rate} kHz, {bits} bit depth, {ch} channels.
/// A value the disc doesn't give (such as channels for MLP) is left out.
/// A template without {n} gets it at the end, so tracks never share a name.
pub fn file_name(template: &str, number: usize, info: &TrackInfo, ext: &str) -> String {
    let template = if template.contains("{n}") { template.to_string() } else { format!("{template} {{n}}") };
    let codec = match info.codec {
        Codec::Ac3 => "AC3",
        Codec::Dts => "DTS",
        Codec::Lpcm => "PCM",
        Codec::Mpeg => "MPEG",
        Codec::Mlp => "MLP",
        Codec::TrueHd => "TrueHD",
    };
    let rate = info.sample_rate.map_or(String::new(), khz);
    let bits = info.bits.map_or(String::new(), |b| b.to_string());
    let channels = info.channels.map_or(String::new(), |c| c.to_string());
    let name = template
        .replace("{n}", &format!("{number:02}"))
        .replace("{codec}", codec)
        .replace("{rate}", &rate)
        .replace("{bits}", &bits)
        .replace("{ch}", &channels);
    let clean: String = name.chars().map(|c| if "\\/:*?\"<>|".contains(c) || c.is_control() { '_' } else { c }).collect();
    let clean = clean.trim().trim_end_matches(|c| c == '.' || c == '-' || c == ' ');
    let clean = if clean.is_empty() { format!("track{number:02}") } else { clean.to_string() };
    format!("{clean}.{ext}")
}

/// The ATS_*.AOB or VTS_*.VOB files of a disc, taken from the AUDIO_TS or VIDEO_TS folder (or the folder itself).
pub fn disc_files(folder: &Path) -> io::Result<Vec<PathBuf>> {
    let stream = folder.join("BDMV").join("STREAM");
    if stream.is_dir() {
        return main_clip(&stream);
    }
    let dir = ["AUDIO_TS", "VIDEO_TS"]
        .iter()
        .map(|name| folder.join(name))
        .find(|path| path.is_dir())
        .unwrap_or_else(|| folder.to_path_buf());
    let mut files = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_uppercase();
        let disc_file = (name.starts_with("ATS_") && name.ends_with(".AOB")) || (name.starts_with("VTS_") && name.ends_with(".VOB"));
        if disc_file {
            files.push(path);
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "No disc audio files (ATS_*.AOB or VTS_*.VOB) were found. Pick the AUDIO_TS or VIDEO_TS folder, or the disc folder that holds it.",
        ));
    }
    Ok(files)
}

/// Every audio track on the disc in order, with how many bytes each one holds and
/// whether its data carries a real audio signature (false usually means scrambled, copy-protected data).
pub fn scan(folder: &Path, cancel: &AtomicBool) -> Result<Scan, Error> {
    let files = disc_files(folder)?;
    let mut found: BTreeMap<TrackKey, (TrackInfo, u64)> = BTreeMap::new();
    let mut probe: HashMap<TrackKey, Vec<u8>> = HashMap::new();
    walk_all(&files, &mut |event| {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        match event {
            Event::Track(info) => {
                found.insert(info.key, (info.clone(), 0));
            }
            Event::Data(key, data) => {
                if let Some(entry) = found.get_mut(&key) {
                    entry.1 += data.len() as u64;
                }
                let head = probe.entry(key).or_default();
                let take = data.len().min(PROBE_BYTES - head.len());
                head.extend_from_slice(&data[..take]);
            }
        }
        Ok(())
    })
    .map_err(|e| check_cancel(e, cancel))?;
    let tracks = found
        .into_iter()
        .map(|(key, (info, bytes))| {
            let audio = probe.get(&key).is_some_and(|head| has_audio_signature(info.codec, head));
            Found { info, bytes, audio }
        })
        .collect();
    Ok(Scan { kind: disc_kind(&files), files: files.len(), tracks })
}

/// What kind of disc the files come from, judged by their names.
fn disc_kind(files: &[PathBuf]) -> &'static str {
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_uppercase())
        .collect();
    if names.iter().any(|n| n.starts_with("ATS_")) {
        "DVD-Audio"
    } else if names.iter().any(|n| n.starts_with("VTS_")) {
        "DVD-Video"
    } else {
        "Blu-ray"
    }
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "cancelled")
}

/// A read that stopped because of Cancel is reported as Cancelled; any other failure is an error.
fn check_cancel(e: io::Error, cancel: &AtomicBool) -> Error {
    if cancel.load(Ordering::Relaxed) {
        Error::Cancelled
    } else {
        Error::Io(e)
    }
}

/// Whether the first bytes of a track look like the codec it claims to be.
/// Lossless PCM has no marker to check, so it always passes.
pub fn has_audio_signature(codec: Codec, head: &[u8]) -> bool {
    let has = |pattern: &[u8]| head.windows(pattern.len()).any(|w| w == pattern);
    match codec {
        Codec::Ac3 => has(&[0x0B, 0x77]),
        Codec::Dts => has(&[0x7F, 0xFE, 0x80, 0x01]) || has(&[0x64, 0x58, 0x20, 0x25]),
        Codec::Mlp | Codec::TrueHd => has(&[0xF8, 0x72, 0x6F]),
        Codec::Mpeg => head.windows(2).any(|w| w[0] == 0xFF && w[1] & 0xE0 == 0xE0),
        Codec::Lpcm => true,
    }
}

/// Writes the chosen tracks into `out_dir`, one file each, named from `name_template` (see `file_name`).
/// `wanted` pairs each track's number (as shown by `scan`) with its key; `total` is their audio in bytes, for the progress bar.
/// A file that already exists is kept: the new one gets a number in brackets instead.
pub fn extract(
    folder: &Path,
    out_dir: &Path,
    wanted: &[(usize, TrackKey)],
    format: Format,
    name_template: &str,
    total: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<Report, Error> {
    fs::create_dir_all(out_dir)?;
    let temp = TempDir::new()?;
    let files = disc_files(folder)?;
    let numbers: HashMap<TrackKey, usize> = wanted.iter().map(|&(n, key)| (key, n)).collect();
    let mut jobs: HashMap<TrackKey, Job> = HashMap::new();
    let mut report = Report::default();
    // Every output file this run has started, so Cancel and errors can remove them.
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut taken: HashSet<PathBuf> = HashSet::new();
    let (mut read, mut next_report) = (0u64, PROGRESS_STEP);
    let fraction = |read: u64| if total == 0 { 0.0 } else { (read as f32 / total as f32).min(1.0) };

    let walked = walk_all(&files, &mut |event| {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        match event {
            Event::Track(info) => {
                let Some(&number) = numbers.get(&info.key) else {
                    return Ok(());
                };
                if let Some(reason) = unsupported_reason(info, format) {
                    report.skipped.push(format!("Track {number} skipped: {reason}."));
                    return Ok(());
                }
                let ext = match format {
                    Format::Mkv => "mkv",
                    Format::Raw => raw_extension(info.codec),
                    Format::Wav => "wav",
                    Format::Mp3 => "mp3",
                    Format::Ogg => "ogg",
                    Format::Flac => "flac",
                };
                let path = unique_path(out_dir, &file_name(name_template, number, info, ext), &mut taken);
                paths.push(path.clone());
                // Matroska, raw streams and 16/24-bit WAV are written straight to the file. Everything else is a raw copy
                // in the temporary folder, which ffmpeg then turns into the final file.
                let direct = matches!(format, Format::Mkv | Format::Raw)
                    || (format == Format::Wav && info.codec == Codec::Lpcm && matches!(info.bits, Some(16) | Some(24)));
                let raw = if direct {
                    path.clone()
                } else {
                    temp.path().join(format!("track{number:02}.raw.{}", raw_extension(info.codec)))
                };
                progress(Progress { text: format!("Converting track {number}…"), fraction: fraction(read) });
                let output = match (format, info.codec) {
                    (Format::Mkv, Codec::Lpcm) => Output::Open(Mkv::create(&path, &pcm_params(info))?),
                    // AC-3 waits for its first frame header, which says the rate and channels.
                    (Format::Mkv, _) => Output::Waiting(Vec::new()),
                    (Format::Wav, Codec::Lpcm) if direct => Output::Wav(WavWriter::create(&path, info)?),
                    _ => Output::Raw(File::create(&raw)?),
                };
                jobs.insert(info.key, Job { number, path, raw, info: info.clone(), output });
            }
            Event::Data(key, data) => {
                if let Some(job) = jobs.get_mut(&key) {
                    job.output.write(&job.path, data)?;
                    read += data.len() as u64;
                    if read >= next_report {
                        next_report += PROGRESS_STEP;
                        progress(Progress { text: format!("Converting… {} read so far", size_text(read)), fraction: fraction(read) });
                    }
                }
            }
        }
        Ok(())
    });
    if let Err(e) = walked {
        remove_all(&paths);
        return Err(if cancel.load(Ordering::Relaxed) { Error::Cancelled } else { e.into() });
    }

    let mut jobs: Vec<Job> = jobs.into_values().collect();
    jobs.sort_by_key(|job| job.number);
    for job in jobs {
        if cancel.load(Ordering::Relaxed) {
            remove_all(&paths);
            return Err(Error::Cancelled);
        }
        let Job { number, path, raw, info, output } = job;
        match output.finish() {
            Ok(true) => {}
            Ok(false) => {
                fs::remove_file(&path).ok();
                report.skipped.push(format!("Track {number} skipped: no AC-3 frame was found."));
                continue;
            }
            Err(e) => {
                fs::remove_file(&path).ok();
                report.failed.push(format!("Track {number}: {e}"));
                continue;
            }
        }
        if raw != path {
            progress(Progress { text: format!("Converting track {number} with ffmpeg…"), fraction: 1.0 });
            let converted = to_ffmpeg(&raw, &path, &info, format);
            fs::remove_file(&raw).ok();
            if let Err(e) = converted {
                fs::remove_file(&path).ok();
                report.failed.push(format!("Track {number} not converted: {e}"));
                continue;
            }
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        report.saved.push(format!("{name} · {}", size_text(size)));
    }
    Ok(report)
}

/// Removes the given files, ignoring any that are already gone.
fn remove_all(paths: &[PathBuf]) {
    for path in paths {
        fs::remove_file(path).ok();
    }
}

/// A file name in `dir` that is not taken: `name`, or `name (2)`, `name (3)` and so on when a file of that name exists.
fn unique_path(dir: &Path, name: &str, taken: &mut HashSet<PathBuf>) -> PathBuf {
    let (stem, ext) = match name.rfind('.') {
        Some(i) => name.split_at(i),
        None => (name, ""),
    };
    let mut n = 1;
    loop {
        let candidate = if n == 1 { dir.join(name) } else { dir.join(format!("{stem} ({n}){ext}")) };
        if !taken.contains(&candidate) && !candidate.exists() {
            taken.insert(candidate.clone());
            return candidate;
        }
        n += 1;
    }
}

/// The folder for raw copies. It is removed when the run ends, including after an error or Cancel.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> io::Result<Self> {
        let dir = std::env::temp_dir().join(format!("dvd_audio_grabber_{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir)?;
        Ok(TempDir(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

/// A track being written: where its data goes, and what it is.
struct Job {
    number: usize,
    /// The final file.
    path: PathBuf,
    /// Where the data goes: the same file, unless ffmpeg makes the final one.
    raw: PathBuf,
    info: TrackInfo,
    output: Output,
}

fn raw_extension(codec: Codec) -> &'static str {
    match codec {
        Codec::Ac3 => "ac3",
        Codec::Dts => "dts",
        Codec::Mpeg => "mpa",
        Codec::Lpcm => "pcm",
        Codec::Mlp => "mlp",
        Codec::TrueHd => "thd",
    }
}

/// Converts a raw stream into WAV, MP3, OGG or FLAC with ffmpeg, which has to be installed and on the PATH.
fn to_ffmpeg(raw: &Path, out: &Path, info: &TrackInfo, format: Format) -> Result<(), Error> {
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-y"].map(String::from).to_vec();
    if info.codec == Codec::Lpcm {
        // DVD and Blu-ray PCM is big-endian, and the raw file has no header, so say what it is.
        let layout = match info.bits {
            Some(24) => "s24be",
            Some(20) => "s20be",
            _ => "s16be",
        };
        let rate = info.sample_rate.unwrap_or(48_000).to_string();
        let channels = info.channels.unwrap_or(2).to_string();
        for arg in ["-f", layout, "-ar", &rate, "-ac", &channels] {
            args.push(arg.to_string());
        }
    }
    args.push("-i".into());
    args.push(raw.display().to_string());
    let codec: &[&str] = match format {
        // 16-bit PCM stays 16-bit. Everything else (20 and 24-bit PCM, MLP and AC-3) is written as 24-bit, which keeps all their depth.
        Format::Wav if info.codec == Codec::Lpcm && info.bits == Some(16) => &["-c:a", "pcm_s16le"],
        Format::Wav => &["-c:a", "pcm_s24le"],
        Format::Flac => &["-c:a", "flac"],
        Format::Mp3 => &["-c:a", "libmp3lame", "-b:a", CONVERT_BITRATE],
        Format::Ogg => &["-c:a", "libvorbis", "-b:a", CONVERT_BITRATE],
        Format::Mkv | Format::Raw => &[],
    };
    args.extend(codec.iter().map(|arg| arg.to_string()));
    args.push(out.display().to_string());
    match Command::new("ffmpeg").args(&args).creation_flags(NO_WINDOW).output() {
        Err(_) => Err(Error::Ffmpeg(FFMPEG_MISSING.into())),
        Ok(run) if run.status.success() => Ok(()),
        Ok(run) => {
            let last = String::from_utf8_lossy(&run.stderr).lines().last().unwrap_or("ffmpeg failed").trim().to_string();
            Err(Error::Ffmpeg(last))
        }
    }
}

pub fn size_text(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.0} MB", bytes as f64 / (1u64 << 20) as f64)
    }
}

fn pcm_params(info: &TrackInfo) -> mkv::Params {
    let sample_rate = info.sample_rate.unwrap_or(48_000);
    let channels = info.channels.unwrap_or(2);
    let bits = info.bits.unwrap_or(16);
    let align = channels as usize * (bits as usize / 8);
    mkv::Params {
        codec_id: "A_PCM/INT/BIG", // DVD PCM is big-endian, which is what this codec ID means
        sample_rate,
        channels,
        bit_depth: Some(bits),
        bytes_per_second: sample_rate as u64 * align as u64,
        align,
        ac3_frames: false,
    }
}

/// Reads the first AC-3 frame header in `data`: sample rate, channels and bitrate.
/// AC-3 on DVD is constant bitrate, so the bitrate gives the timing.
pub fn ac3_params(data: &[u8]) -> Option<mkv::Params> {
    let i = data.windows(2).position(|w| w == [0x0B, 0x77])?;
    let f = data.get(i..i + 7)?; // sync word, CRC, rate and size, then the start of the bit stream info
    let sample_rate = *[48_000, 44_100, 32_000].get((f[4] >> 6) as usize)?;
    let kbps = *AC3_KBPS.get(((f[4] & 0x3F) / 2) as usize)?;
    // Bit n of the frame header, counted from the sync word.
    let bit = |n: usize| (f[n / 8] >> (7 - n % 8)) & 1;
    let field = |from: usize, len: usize| (from..from + len).fold(0usize, |v, n| (v << 1) | bit(n) as usize);
    let acmod = field(48, 3);
    let mut pos = 51;
    if acmod & 1 != 0 && acmod != 1 {
        pos += 2; // centre mix level
    }
    if acmod & 4 != 0 {
        pos += 2; // surround mix level
    }
    if acmod == 2 {
        pos += 2; // Dolby Surround mode
    }
    let channels = AC3_CHANNELS[acmod] + bit(pos);
    Some(mkv::Params {
        codec_id: "A_AC3",
        sample_rate,
        channels,
        bit_depth: None,
        bytes_per_second: kbps as u64 * 125, // kbit/s to bytes/s
        align: 1,
        ac3_frames: true,
    })
}

enum Output {
    /// AC-3 in Matroska: the audio so far, until the frame header shows the rate and channels.
    Waiting(Vec<u8>),
    Open(Mkv),
    Raw(File),
    Wav(WavWriter),
    Failed,
}

/// Writes PCM as a WAV file. DVD PCM is big-endian, so each sample's bytes are reversed.
/// shortcut: WAV sizes are 32-bit, so a track over 4 GB is refused; write RF64 or pick MKV or FLAC for it.
struct WavWriter {
    file: File,
    bits: u16,
    block: usize,
    pending: Vec<u8>,
    data_len: u64,
}

impl WavWriter {
    fn create(path: &Path, info: &TrackInfo) -> io::Result<Self> {
        let channels = info.channels.unwrap_or(2) as u16;
        let bits = info.bits.unwrap_or(16) as u16;
        let rate = info.sample_rate.unwrap_or(48_000);
        let block = channels as usize * (bits as usize / 8);
        let mut header = Vec::with_capacity(44);
        header.extend(b"RIFF");
        header.extend(0u32.to_le_bytes()); // size, filled in by finish
        header.extend(b"WAVEfmt ");
        header.extend(16u32.to_le_bytes());
        header.extend(1u16.to_le_bytes()); // PCM
        header.extend(channels.to_le_bytes());
        header.extend(rate.to_le_bytes());
        header.extend((rate * block as u32).to_le_bytes());
        header.extend((block as u16).to_le_bytes());
        header.extend(bits.to_le_bytes());
        header.extend(b"data");
        header.extend(0u32.to_le_bytes()); // data size, filled in by finish
        let mut file = File::create(path)?;
        file.write_all(&header)?;
        Ok(WavWriter { file, bits, block, pending: Vec::new(), data_len: 0 })
    }

    /// Only whole sample frames are written; a frame split between two packets waits for the rest.
    fn write_pcm(&mut self, data: &[u8]) -> io::Result<()> {
        self.pending.extend_from_slice(data);
        let whole = self.pending.len() / self.block * self.block;
        if self.data_len + whole as u64 > WAV_DATA_LIMIT {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a PCM track is over 4 GB, too long for WAV; choose MKV or FLAC"));
        }
        let mut out: Vec<u8> = self.pending.drain(..whole).collect();
        for sample in out.chunks_exact_mut(self.bits as usize / 8) {
            sample.reverse();
        }
        self.file.write_all(&out)?;
        self.data_len += whole as u64;
        Ok(())
    }

    /// Fills in the sizes in the header.
    fn finish(mut self) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(4))?;
        self.file.write_all(&((36 + self.data_len) as u32).to_le_bytes())?;
        self.file.seek(SeekFrom::Start(40))?;
        self.file.write_all(&(self.data_len as u32).to_le_bytes())?;
        Ok(())
    }
}

/// Largest sample data a WAV can hold: the 32-bit size minus the 36-byte header.
const WAV_DATA_LIMIT: u64 = u32::MAX as u64 - 36;

impl Output {
    fn write(&mut self, path: &Path, data: &[u8]) -> io::Result<()> {
        match self {
            Output::Open(mkv) => mkv.block(data),
            Output::Raw(file) => file.write_all(data),
            Output::Wav(wav) => wav.write_pcm(data),
            Output::Waiting(buf) => {
                buf.extend_from_slice(data);
                let Some(params) = ac3_params(buf) else {
                    if buf.len() > AC3_SEARCH_LIMIT {
                        *self = Output::Failed;
                    }
                    return Ok(());
                };
                let mut mkv = Mkv::create(path, &params)?;
                mkv.block(buf)?;
                *self = Output::Open(mkv);
                Ok(())
            }
            Output::Failed => Ok(()),
        }
    }

    /// Whether the data was written; false if the track never got going.
    fn finish(self) -> io::Result<bool> {
        match self {
            Output::Open(mkv) => mkv.finish().map(|_| true),
            Output::Raw(_) => Ok(true),
            Output::Wav(wav) => wav.finish().map(|_| true),
            Output::Waiting(_) | Output::Failed => Ok(false),
        }
    }
}

fn walk_all(files: &[PathBuf], on_event: &mut dyn FnMut(Event<'_>) -> io::Result<()>) -> io::Result<()> {
    let mut seen = HashSet::new();
    // Sectors already read in each titleset (ATS_xx), so a track's sector range can be found across its AOB files.
    let mut sectors_before: HashMap<String, u32> = HashMap::new();
    for path in files {
        let file = File::open(path)?;
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("m2ts")) {
            walk_ts(BufReader::with_capacity(READ_CHUNK, file), &mut seen, &mut *on_event)?;
        } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("aob")) {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_uppercase();
            let titleset = name.get(4..6).unwrap_or("").to_string();
            let ranges = find_ifo(path.parent().unwrap_or(Path::new("")), &titleset)
                .and_then(|ifo| fs::read(ifo).ok())
                .map(|ifo| aob_ranges(&ifo))
                .unwrap_or_default();
            let first_sector = *sectors_before.get(&titleset).unwrap_or(&0);
            walk(file, &mut seen, &mut *on_event, Layout::Aob { ranges: &ranges, first_sector })?;
            let sectors = fs::metadata(path)?.len() / SECTOR;
            *sectors_before.entry(titleset).or_insert(0) += sectors as u32;
        } else {
            walk(file, &mut seen, &mut *on_event, Layout::Vob)?;
        }
    }
    Ok(())
}

/// Size of a DVD sector: packs and the IFO's sector numbers are in these.
const SECTOR: u64 = 2048;

/// The ATS_xx_0.IFO of a titleset in `dir`, found without regard to letter case.
fn find_ifo(dir: &Path, titleset: &str) -> Option<PathBuf> {
    let wanted = format!("ATS_{titleset}_0.IFO");
    fs::read_dir(dir).ok()?.filter_map(|e| e.ok().map(|e| e.path())).find(|p| {
        p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.to_uppercase() == wanted)
    })
}

/// The sector range of each track in a DVD-Audio titleset, from its ATS_xx_0.IFO. Sectors are counted from the titleset's first AOB.
/// Verified on the 44 discs in the dvda-author repository: the ranges cover every sector of their AOB files.
/// Layout: title count at 2048; title index entries of 8 bytes from 2056, whose last 4 bytes give the title record's offset from 2048;
/// in a title record, the track count is at +2 and the offset of the sector table at +12; each track's 12-byte record in that
/// table has its first sector at +4 and its last sector at +8.
fn aob_ranges(ifo: &[u8]) -> Vec<(u32, u32)> {
    fn be(d: &[u8], at: usize, n: usize) -> Option<u32> {
        let b = d.get(at..at + n)?;
        Some(b.iter().fold(0u32, |v, &x| (v << 8) | x as u32))
    }
    let parse = || -> Option<Vec<(u32, u32)>> {
        if ifo.get(..12)? != b"DVDAUDIO-ATS" {
            return None;
        }
        let titles = be(ifo, SECTOR as usize, 2)? as usize;
        let mut out: Vec<(u32, u32)> = Vec::new();
        for t in 0..titles {
            let title = SECTOR as usize + be(ifo, SECTOR as usize + 8 + 8 * t + 4, 4)? as usize;
            let tracks = *ifo.get(title + 2)? as usize;
            let table = title + be(ifo, title + 12, 2)? as usize;
            for k in 0..tracks {
                let range = (be(ifo, table + 12 * k + 4, 4)?, be(ifo, table + 12 * k + 8, 4)?);
                if !out.contains(&range) {
                    out.push(range); // the same range can appear under several titles
                }
            }
        }
        Some(out)
    };
    parse().unwrap_or_default()
}

/// Channel count for each channel group assignment of a DVD-Audio AOB (the spec's table, ids 0-20).
const AOB_CHANNELS: [u8; 21] = [1, 2, 3, 4, 3, 4, 5, 3, 4, 5, 4, 5, 6, 4, 5, 4, 5, 6, 5, 5, 6];

/// The format of a DVD-Audio LPCM track, from its AOB header (`h` starts at the sub-stream ID).
/// Offsets: 7 = sample size nibbles, 8 = sample rate nibbles, 10 = channel group assignment.
/// shortcut: tracks with a second channel assignment (byte 9 non-zero) are reported with unknown channels, so they are not converted; upgrade to decode them.
fn aob_lpcm_info(key: TrackKey, h: &[u8]) -> TrackInfo {
    let split = h[7] & 0x0F != 0x0F || h[8] & 0x0F != 0x0F;
    TrackInfo {
        key,
        codec: Codec::Lpcm,
        bits: match h[7] >> 4 {
            0 => Some(16),
            1 => Some(20),
            2 => Some(24),
            _ => None,
        },
        sample_rate: match h[8] >> 4 {
            0x0 => Some(48_000),
            0x1 => Some(96_000),
            0x2 => Some(192_000),
            0x8 => Some(44_100),
            0x9 => Some(88_200),
            0xA => Some(176_400),
            _ => None,
        },
        // Byte 9 is zero when there's no second channel assignment, so the group-1 count in byte 10 is the total.
        channels: if split && h[9] != 0 { None } else { AOB_CHANNELS.get(h[10] as usize).copied() },
    }
}

/// The main clip of a Blu-ray: the largest .m2ts file in its STREAM folder.
/// shortcut: only that clip is read, so audio that appears only in other clips is not found; read them all when a disc needs it.
fn main_clip(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut clips: Vec<(u64, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("m2ts")) {
            clips.push((fs::metadata(&path)?.len(), path));
        }
    }
    clips
        .into_iter()
        .max_by_key(|(size, _)| *size)
        .map(|(_, path)| vec![path])
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No Blu-ray clips (.m2ts) were found in the STREAM folder."))
}

/// Walks a Blu-ray .m2ts file: 192-byte packets (a 4-byte timestamp, then a 188-byte transport packet).
/// The program map names the audio PIDs; each audio PES packet's payload is reported.
fn walk_ts<R: Read>(mut reader: R, seen: &mut HashSet<TrackKey>, on_event: &mut dyn FnMut(Event<'_>) -> io::Result<()>) -> io::Result<()> {
    let mut pkt = [0u8; 192];
    let mut pmt_pid: Option<u16> = None;
    let mut streams: HashMap<u16, u8> = HashMap::new(); // PID → stream type
    let mut started: HashSet<u16> = HashSet::new(); // audio PIDs that have seen a PES packet start
    loop {
        match reader.read_exact(&mut pkt) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        let ts = &pkt[4..];
        if ts[0] != 0x47 {
            continue; // shortcut: a damaged packet is skipped, without searching for the next sync byte
        }
        let pid = (((ts[1] & 0x1F) as u16) << 8) | ts[2] as u16;
        let pusi = ts[1] & 0x40 != 0;
        let afc = (ts[3] >> 4) & 3; // bit 1: adaptation field present, bit 0: payload present
        let off = if afc & 2 != 0 { 5 + ts[4] as usize } else { 4 };
        if afc & 1 == 0 || off >= 188 {
            continue;
        }
        let payload = &ts[off..188];
        if pid == 0 {
            if pusi {
                pmt_pid = parse_pat(payload).or(pmt_pid);
            }
            continue;
        }
        if Some(pid) == pmt_pid {
            if pusi {
                streams = parse_pmt(payload);
            }
            continue;
        }
        let Some(&stream_type) = streams.get(&pid) else {
            continue;
        };
        let codec = match stream_type {
            0x81 => Codec::Ac3,
            0x82 | 0x85 | 0x86 => Codec::Dts,
            0x80 => Codec::Lpcm,
            0x83 => Codec::TrueHd,
            0x03 | 0x04 => Codec::Mpeg,
            _ => continue, // video, subtitles and E-AC-3 are not tracks here
        };
        let key = TrackKey { stream: (pid >> 8) as u8, sub: pid as u8, part: 0 };
        let mut data = payload;
        if pusi {
            // PES header: start code, stream id, length, two flag bytes, then the header data length
            if payload.len() < 9 || payload[..3] != [0u8, 0, 1] || 9 + payload[8] as usize > payload.len() {
                continue;
            }
            data = &payload[9 + payload[8] as usize..];
            started.insert(pid);
            if codec == Codec::Lpcm {
                if data.len() < 4 {
                    continue;
                }
                let info = bd_lpcm_info(key, &data[..4]);
                if seen.insert(key) {
                    on_event(Event::Track(&info))?;
                }
                data = &data[4..]; // each LPCM packet starts with a 4-byte header
            }
        }
        if !started.contains(&pid) {
            continue;
        }
        if seen.insert(key) {
            on_event(Event::Track(&TrackInfo { key, codec, sample_rate: None, channels: None, bits: None }))?;
        }
        if !data.is_empty() {
            on_event(Event::Data(key, data))?;
        }
    }
    Ok(())
}

/// The payload's table section, after the pointer field.
fn section(payload: &[u8]) -> Option<&[u8]> {
    payload.get(*payload.first()? as usize + 1..)
}

/// The program map PID from a program association table.
fn parse_pat(payload: &[u8]) -> Option<u16> {
    let s = section(payload)?;
    if s.len() < 8 {
        return None;
    }
    let len = (((s[1] & 0x0F) as usize) << 8) | s[2] as usize;
    let end = (3 + len).saturating_sub(4).min(s.len()); // the last 4 bytes are the CRC
    s.get(8..end)?.chunks_exact(4).find_map(|e| {
        let program = u16::from_be_bytes([e[0], e[1]]);
        (program != 0).then(|| (((e[2] & 0x1F) as u16) << 8) | e[3] as u16)
    })
}

/// The streams in a program map table: PID → stream type.
fn parse_pmt(payload: &[u8]) -> HashMap<u16, u8> {
    let mut streams = HashMap::new();
    let Some(s) = section(payload).filter(|s| s.len() >= 12) else {
        return streams;
    };
    let len = (((s[1] & 0x0F) as usize) << 8) | s[2] as usize;
    let end = (3 + len).saturating_sub(4).min(s.len());
    let info_len = (((s[10] & 0x0F) as usize) << 8) | s[11] as usize;
    let mut i = 12 + info_len;
    while i + 5 <= end {
        let pid = (((s[i + 1] & 0x1F) as u16) << 8) | s[i + 2] as u16;
        let es_len = (((s[i + 3] & 0x0F) as usize) << 8) | s[i + 4] as usize;
        streams.insert(pid, s[i]);
        i += 5 + es_len;
    }
    streams
}

/// A Blu-ray LPCM header (4 bytes): byte 2 holds the channel layout (high nibble) and the rate code (low nibble),
/// byte 3 the bit depth code (top two bits).
/// shortcut: only mono and stereo layouts and the 48 and 96 kHz rates are named; others stay unknown, so they're not converted.
fn bd_lpcm_info(key: TrackKey, h: &[u8]) -> TrackInfo {
    TrackInfo {
        key,
        codec: Codec::Lpcm,
        sample_rate: match h[2] & 0x0F {
            1 => Some(48_000),
            4 => Some(96_000),
            _ => None,
        },
        channels: match h[2] >> 4 {
            1 => Some(1),
            3 => Some(2),
            _ => None,
        },
        bits: match h[3] >> 6 {
            1 => Some(16),
            2 => Some(20),
            3 => Some(24),
            _ => None,
        },
    }
}

/// A read-ahead window over the file, so packets that straddle a read can be parsed.
struct Window<R: Read> {
    reader: R,
    buf: Vec<u8>,
    pos: usize,
    /// Bytes of the file before `buf` (already dropped), so `base + pos` is the position in the file.
    base: u64,
}

impl<R: Read> Window<R> {
    fn avail(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn byte(&self, i: usize) -> u8 {
        self.buf[self.pos + i]
    }

    fn be16(&self, i: usize) -> usize {
        ((self.byte(i) as usize) << 8) | self.byte(i + 1) as usize
    }

    /// Makes at least `n` bytes available. False if the file ends first.
    fn ensure(&mut self, n: usize) -> io::Result<bool> {
        while self.avail() < n {
            if !self.refill()? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Steps over `n` bytes. False if the file ends first.
    fn skip(&mut self, n: usize) -> io::Result<bool> {
        if !self.ensure(n)? {
            return Ok(false);
        }
        self.pos += n;
        Ok(true)
    }

    fn refill(&mut self) -> io::Result<bool> {
        self.buf.drain(..self.pos);
        self.base += self.pos as u64;
        self.pos = 0;
        let old = self.buf.len();
        self.buf.resize(old + READ_CHUNK, 0);
        let n = loop {
            match self.reader.read(&mut self.buf[old..]) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        };
        self.buf.truncate(old + n);
        Ok(n > 0)
    }
}

/// How a file's audio is laid out.
#[derive(Clone, Copy)]
enum Layout<'a> {
    /// DVD-Video VOB: DVD-Video sub-stream headers, one track per sub-stream ID.
    Vob,
    /// DVD-Audio AOB: DVD-Audio sub-stream headers. `ranges` (from the IFO) give each track's sectors; `first_sector` is this file's
    /// first sector within its titleset. With no ranges, the whole stream is one track.
    Aob { ranges: &'a [(u32, u32)], first_sector: u32 },
}

/// Walks the packets of one file (MPEG-2 program stream). Audio payloads are reported; everything else is stepped over.
fn walk<R: Read>(
    reader: R,
    seen: &mut HashSet<TrackKey>,
    on_event: &mut dyn FnMut(Event<'_>) -> io::Result<()>,
    layout: Layout<'_>,
) -> io::Result<()> {
    let aob = matches!(layout, Layout::Aob { .. });
    let mut win = Window { reader, buf: Vec::new(), pos: 0, base: 0 };
    // The track whose payload the next `remaining` bytes belong to.
    let mut current: Option<TrackKey> = None;
    let mut remaining = 0usize;
    loop {
        if remaining > 0 {
            if win.avail() == 0 && !win.refill()? {
                break;
            }
            let n = remaining.min(win.avail());
            if let Some(key) = current {
                on_event(Event::Data(key, &win.buf[win.pos..win.pos + n]))?;
            }
            win.pos += n;
            remaining -= n;
            continue;
        }

        if !win.ensure(4)? {
            break;
        }
        if win.byte(0) != 0 || win.byte(1) != 0 || win.byte(2) != 1 {
            win.pos += 1; // not at a packet start, so move on one byte
            continue;
        }
        let id = win.byte(3);
        let packet_pos = win.base + win.pos as u64;
        match id {
            0xB9 => {
                if !win.skip(4)? {
                    break;
                }
            }
            0xBA => {
                // pack header: 14 bytes plus stuffing
                if !win.ensure(14)? {
                    break;
                }
                let stuffing = (win.byte(13) & 0x07) as usize;
                if !win.skip(14 + stuffing)? {
                    break;
                }
            }
            0xBB | 0xBC | 0xBE | 0xBF | 0xF0 | 0xF1 | 0xF2 | 0xF8 | 0xFF => {
                // system header, program map, padding and similar: skip by length
                if !win.ensure(6)? {
                    break;
                }
                let len = win.be16(4);
                if !win.skip(6 + len)? {
                    break;
                }
            }
            _ => {
                // packets with an optional header: audio, video, subtitles
                if !win.ensure(9)? {
                    break;
                }
                let len = win.be16(4);
                if len == 0 {
                    // shortcut: unbounded packets are only seen in video, so step past the header and rescan
                    if !win.skip(6)? {
                        break;
                    }
                    continue;
                }
                let packet_len = 6 + len;
                let body_off = 9 + win.byte(8) as usize;
                let audio = id == 0xBD || (0xC0..=0xDF).contains(&id);
                if !audio {
                    if !win.skip(packet_len)? {
                        break;
                    }
                    continue;
                }
                if !win.ensure(body_off + 6)? {
                    break;
                }
                let (key, codec, extra) = if id == 0xBD {
                    let sub = win.byte(body_off);
                    let (codec, extra) = match (aob, sub) {
                        (_, 0x80..=0x87) => (Codec::Ac3, 4),
                        (_, 0x88..=0x8F) => (Codec::Dts, 4),
                        // DVD-Audio AOB: a 4-byte header, then a header of the length given at offset 2
                        (true, 0xA0) => (Codec::Lpcm, 4 + win.be16(body_off + 2)),
                        (true, 0xA1) => (Codec::Mlp, 4 + win.be16(body_off + 2)),
                        (false, 0xA0..=0xA7) => (Codec::Lpcm, 6),
                        _ => {
                            // subtitles and other sub-streams are not audio
                            if !win.skip(packet_len)? {
                                break;
                            }
                            continue;
                        }
                    };
                    // On an AOB with an IFO, the packet's sector says which track it belongs to.
                    let part = match layout {
                        Layout::Aob { ranges, first_sector } if !ranges.is_empty() => {
                            let sector = first_sector as u64 + packet_pos / SECTOR;
                            match ranges.iter().position(|&(f, l)| f as u64 <= sector && sector <= l as u64) {
                                Some(t) => t as u16 + 1,
                                None => {
                                    // outside every track's sectors (for example padding or a menu)
                                    if !win.skip(packet_len)? {
                                        break;
                                    }
                                    continue;
                                }
                            }
                        }
                        _ => 0,
                    };
                    (TrackKey { stream: id, sub, part }, codec, extra)
                } else {
                    (TrackKey { stream: id, sub: 0, part: 0 }, Codec::Mpeg, 0)
                };
                let body = body_off + extra;
                if body > packet_len {
                    if !win.skip(packet_len)? {
                        break;
                    }
                    continue;
                }
                let info = if codec == Codec::Lpcm && aob {
                    if !win.ensure(body_off + 11)? {
                        break;
                    }
                    aob_lpcm_info(key, &win.buf[win.pos + body_off..])
                } else if codec == Codec::Lpcm {
                    // DVD-Video LPCM header byte 5: bits 6-7 sample size, bits 4-5 rate, bits 0-2 channels minus one
                    let b = win.byte(body_off + 5);
                    TrackInfo {
                        key,
                        codec,
                        bits: match b >> 6 {
                            0 => Some(16),
                            1 => Some(20),
                            2 => Some(24),
                            _ => None,
                        },
                        sample_rate: match (b >> 4) & 3 {
                            0 => Some(48_000),
                            1 => Some(96_000),
                            _ => None,
                        },
                        channels: Some((b & 7) + 1),
                    }
                } else {
                    TrackInfo { key, codec, sample_rate: None, channels: None, bits: None }
                };
                if seen.insert(key) {
                    on_event(Event::Track(&info))?;
                }
                if !win.ensure(body)? {
                    break;
                }
                win.pos += body;
                current = Some(key);
                remaining = packet_len - body;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A private-stream-1 packet with a zero-length optional header.
    fn pes(stream: u8, body: &[u8]) -> Vec<u8> {
        let len = (body.len() + 3) as u16;
        let mut v = vec![0, 0, 1, stream, (len >> 8) as u8, len as u8, 0x80, 0x00, 0x00];
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn reads_ac3_and_lpcm_packets() {
        let mut aob = vec![0, 0, 1, 0xBA];
        aob.extend([0u8; 10]); // pack header
        aob.extend(pes(0xBD, &[0x80, 1, 0, 0, 1, 2, 3, 4, 5])); // AC-3, 4-byte sub-header
        aob.extend(pes(0xBD, &[0xA0, 1, 0, 0, 0, 0x01, 0x12, 0x34, 0x56, 0x78])); // 16-bit, 48 kHz, 2 channels

        let mut tracks = Vec::new();
        let mut data: BTreeMap<TrackKey, Vec<u8>> = BTreeMap::new();
        let mut seen = HashSet::new();
        walk(Cursor::new(aob), &mut seen, &mut |event| {
            match event {
                Event::Track(info) => tracks.push(info.clone()),
                Event::Data(key, bytes) => data.entry(key).or_default().extend_from_slice(bytes),
            }
            Ok(())
        }, Layout::Vob)
        .unwrap();

        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].codec, Codec::Ac3);
        assert_eq!(tracks[1].codec, Codec::Lpcm);
        assert_eq!(tracks[1].bits, Some(16));
        assert_eq!(tracks[1].sample_rate, Some(48_000));
        assert_eq!(tracks[1].channels, Some(2));
        assert_eq!(data[&TrackKey { stream: 0xBD, sub: 0x80, part: 0 }], vec![1, 2, 3, 4, 5]);
        assert_eq!(data[&TrackKey { stream: 0xBD, sub: 0xA0, part: 0 }], vec![0x12, 0x34, 0x56, 0x78]);
    }

    #[test]
    fn keeps_every_byte_across_read_chunks() {
        // 60 packets of 60 KB is more than one read chunk, so packets straddle reads.
        let mut aob = Vec::new();
        let mut expected = Vec::new();
        for i in 0..60u8 {
            let mut body = vec![0x80, 1, 0, 0];
            body.extend(std::iter::repeat(i).take(60_000));
            aob.extend(pes(0xBD, &body));
            expected.extend(std::iter::repeat(i).take(60_000));
        }
        let mut seen = HashSet::new();
        let mut total = Vec::new();
        let mut tracks = 0;
        walk(Cursor::new(aob), &mut seen, &mut |event| {
            match event {
                Event::Track(_) => tracks += 1,
                Event::Data(_, bytes) => total.extend_from_slice(bytes),
            }
            Ok(())
        }, Layout::Vob)
        .unwrap();
        assert_eq!(tracks, 1);
        assert!(total == expected);
    }

    /// Runs the walker over one AOB built from `packets`, returning each track's info and its payload bytes.
    fn walk_aob(packets: &[Vec<u8>]) -> Vec<(TrackInfo, Vec<u8>)> {
        let aob: Vec<u8> = packets.concat();
        let mut tracks: Vec<TrackInfo> = Vec::new();
        let mut data: BTreeMap<TrackKey, Vec<u8>> = BTreeMap::new();
        let mut seen = HashSet::new();
        walk(Cursor::new(aob), &mut seen, &mut |event| {
            match event {
                Event::Track(info) => tracks.push(info.clone()),
                Event::Data(key, bytes) => data.entry(key).or_default().extend_from_slice(bytes),
            }
            Ok(())
        }, Layout::Aob { ranges: &[], first_sector: 0 })
        .unwrap();
        tracks.into_iter().map(|info| {
            let bytes = data.remove(&info.key).unwrap_or_default();
            (info, bytes)
        }).collect()
    }

    #[test]
    fn reads_dvd_audio_lpcm_with_its_own_header() {
        // Sub-stream 0xA0, then a header of 11 bytes counted from offset 4: the samples start 15 bytes in.
        // 16-bit (0x0F = group 1 used, group 2 unused), 48 kHz, channel group 1 = L R.
        let header = [0xA0, 0x00, 0x00, 0x0B, 0x00, 0x0A, 0x10, 0x0F, 0x0F, 0x00, 0x01, 0x80, 0x00, 0x00, 0x00];
        let samples = [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0];
        let packet = pes(0xBD, &[&header[..], &samples[..]].concat());
        let tracks = walk_aob(&[packet]);
        assert_eq!(tracks.len(), 1);
        let (info, bytes) = &tracks[0];
        assert_eq!((info.codec, info.bits, info.sample_rate, info.channels), (Codec::Lpcm, Some(16), Some(48_000), Some(2)));
        assert_eq!(bytes, &samples.to_vec(), "header bytes must not leak into the audio");
    }

    /// A minimal ATS_xx_0.IFO: one title whose track records give the given sector ranges (in the layout `aob_ranges` reads).
    fn ifo_with_tracks(ranges: &[(u32, u32)]) -> Vec<u8> {
        let mut d = vec![0u8; 4096];
        d[..12].copy_from_slice(b"DVDAUDIO-ATS");
        d[2048..2050].copy_from_slice(&1u16.to_be_bytes()); // one title
        d[2048 + 8 + 4..2048 + 8 + 8].copy_from_slice(&0x10u32.to_be_bytes()); // title record at offset 0x10
        let title = 2048 + 0x10;
        d[title + 2] = ranges.len() as u8; // tracks
        let table = 16 + 20 * ranges.len(); // sector table after the timestamp records, relative to the title record
        d[title + 12..title + 14].copy_from_slice(&(table as u16).to_be_bytes());
        for (k, &(first, last)) in ranges.iter().enumerate() {
            let rec = title + table + 12 * k;
            d[rec + 1..rec + 4].copy_from_slice(&[0, 0, 0]);
            d[rec] = 1;
            d[rec + 4..rec + 8].copy_from_slice(&first.to_be_bytes());
            d[rec + 8..rec + 12].copy_from_slice(&last.to_be_bytes());
        }
        d
    }

    #[test]
    fn reads_track_sector_ranges_from_the_ifo() {
        // The real 16-bit disc has one track over sectors 0 to 301 (302 sectors, the size of its AOB).
        assert_eq!(aob_ranges(&ifo_with_tracks(&[(0, 301)])), vec![(0, 301)]);
        // Three tracks in one AOB, as on the repository's G1T3 discs.
        let three = [(0, 578), (579, 1157), (1158, 1736)];
        assert_eq!(aob_ranges(&ifo_with_tracks(&three)), three.to_vec());
        assert!(aob_ranges(&[0u8; 4096]).is_empty(), "not an ATS IFO");
    }

    #[test]
    fn splits_one_aob_into_tracks_by_sector() {
        // Track 1 is sector 0, track 2 is sector 1; a packet in sector 2 is in neither and is skipped.
        let a = pes(0xBD, &[&[0xA1, 0, 0, 6, 0, 5, 0x0B, 0xFF, 0, 0][..], &[1u8, 2, 3][..]].concat());
        let b = pes(0xBD, &[&[0xA1, 1, 0, 6, 0, 5, 0x0B, 0xFF, 0, 0][..], &[4u8, 5][..]].concat());
        let c = pes(0xBD, &[&[0xA1, 2, 0, 6, 0, 5, 0x0B, 0xFF, 0, 0][..], &[9u8][..]].concat());
        let mut aob = a;
        aob.resize(2048, 0); // pad to the next sector
        aob.extend(b);
        aob.resize(4096, 0);
        aob.extend(c);
        let ranges = [(0u32, 0u32), (1, 1)];
        let mut tracks: Vec<TrackInfo> = Vec::new();
        let mut data: BTreeMap<TrackKey, Vec<u8>> = BTreeMap::new();
        let mut seen = HashSet::new();
        walk(Cursor::new(aob), &mut seen, &mut |event| {
            match event {
                Event::Track(info) => tracks.push(info.clone()),
                Event::Data(key, bytes) => data.entry(key).or_default().extend_from_slice(bytes),
            }
            Ok(())
        }, Layout::Aob { ranges: &ranges, first_sector: 0 })
        .unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(data[&TrackKey { stream: 0xBD, sub: 0xA1, part: 1 }], vec![1, 2, 3]);
        assert_eq!(data[&TrackKey { stream: 0xBD, sub: 0xA1, part: 2 }], vec![4, 5]);
    }

    #[test]
    fn reads_dvd_audio_mlp_with_its_own_header() {
        // Sub-stream 0xA1 (MLP), header length 6 counted from offset 4: the access units start 10 bytes in.
        let header = [0xA1, 0x00, 0x00, 0x06, 0x00, 0x05, 0x0B, 0xFF, 0x00, 0x00];
        let units = [0xF8, 0x72, 0x6F, 0xBB, 0x01, 0x02];
        let packet = pes(0xBD, &[&header[..], &units[..]].concat());
        let tracks = walk_aob(&[packet]);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].0.codec, Codec::Mlp);
        assert_eq!(tracks[0].1, units.to_vec());
    }

    #[test]
    fn writes_wav_with_swapped_samples() {
        let path = std::env::temp_dir().join("dvd_audio_grabber_test.wav");
        let info = TrackInfo { key: TrackKey { stream: 0xBD, sub: 0xA0, part: 0 }, codec: Codec::Lpcm, sample_rate: Some(48_000), channels: Some(2), bits: Some(16) };
        let mut wav = WavWriter::create(&path, &info).unwrap();
        wav.write_pcm(&[0x12, 0x34, 0x56]).unwrap(); // one sample frame and half of the next
        wav.write_pcm(&[0x78]).unwrap();
        wav.finish().unwrap();

        let bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 36 + 4);
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 4);
        assert_eq!(&bytes[44..], &[0x34, 0x12, 0x78, 0x56]);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn reads_blu_ray_program_tables_and_pcm_header() {
        // pointer field 0, then a PAT (program 1 is on PID 0x1000) and a PMT (AC-3 on PID 0x1100)
        let pat = [0, 0, 0xB0, 0x0D, 0, 1, 0xC1, 0, 0, 0, 1, 0xF0, 0x00, 0, 0, 0, 0];
        let pmt = [0, 0x02, 0xB0, 0x12, 0, 1, 0xC1, 0, 0, 0xE1, 0x00, 0xF0, 0x00, 0x81, 0xF1, 0x00, 0xF0, 0x00, 0, 0, 0, 0];
        assert_eq!(parse_pat(&pat), Some(0x1000));
        assert_eq!(parse_pmt(&pmt).get(&0x1100), Some(&0x81));
        // stereo, 48 kHz, 16-bit
        let info = bd_lpcm_info(TrackKey { stream: 0x11, sub: 0x01, part: 0 }, &[0, 0, 0x31, 0x40]);
        assert_eq!((info.channels, info.sample_rate, info.bits), (Some(2), Some(48_000), Some(16)));
    }

    #[test]
    fn checks_audio_signatures() {
        assert!(has_audio_signature(Codec::Ac3, &[0, 0, 0x0B, 0x77, 0x14]));
        assert!(!has_audio_signature(Codec::Ac3, &[0x55; 64])); // scrambled-looking data
        assert!(has_audio_signature(Codec::Mlp, &[0, 0, 0, 0, 0xF8, 0x72, 0x6F, 0xBB]));
        assert!(has_audio_signature(Codec::Lpcm, &[0x55; 4]));
    }

    #[test]
    fn reads_ac3_stereo_header() {
        // 48 kHz, 192 kbit/s, 2/0 (stereo), no LFE
        let p = ac3_params(&[0x0B, 0x77, 0, 0, 0x14, 0x40, 0x40]).unwrap();
        assert_eq!((p.sample_rate, p.channels, p.bytes_per_second), (48_000, 2, 24_000));
    }

    #[test]
    fn reads_ac3_5_1_header() {
        // 48 kHz, 192 kbit/s, 3/2 with LFE
        let p = ac3_params(&[0x0B, 0x77, 0, 0, 0x14, 0x40, 0xE1]).unwrap();
        assert_eq!(p.channels, 6);
    }

    #[test]
    fn names_files_from_the_template() {
        let info = TrackInfo {
            key: TrackKey { stream: 0xBD, sub: 0xA0, part: 0 },
            codec: Codec::Lpcm,
            sample_rate: Some(44_100),
            channels: Some(2),
            bits: Some(16),
        };
        assert_eq!(file_name("track{n}", 3, &info, "wav"), "track03.wav");
        assert_eq!(file_name("{codec} {rate}kHz {bits}bit {ch}ch", 1, &info, "flac"), "PCM 44.1kHz 16bit 2ch.flac");
        assert_eq!(file_name("a/b:c", 2, &info, "wav"), "a_b_c 02.wav");
    }

    #[test]
    fn twenty_bit_pcm_is_convertible_except_to_mkv() {
        let info = TrackInfo {
            key: TrackKey { stream: 0xBD, sub: 0xA0, part: 0 },
            codec: Codec::Lpcm,
            sample_rate: Some(96_000),
            channels: Some(6),
            bits: Some(20),
        };
        assert!(convertible(&info, Format::Wav));
        assert!(convertible(&info, Format::Flac));
        assert!(!convertible(&info, Format::Mkv));
        assert!(unsupported_reason(&info, Format::Mkv).is_some());
        assert!(unsupported_reason(&info, Format::Wav).is_none());
    }

    #[test]
    fn reads_six_channels_when_second_group_is_unassigned() {
        // Header from a real 24-bit 44.1 kHz 5.1 AOB: group 2 has the same settings but byte 9 is zero, byte 10 = 0x11 (6 channels).
        let h = [0xA0, 0x00, 0x00, 0x0F, 0x00, 0x0E, 0x00, 0x22, 0x88, 0x00, 0x11, 0x80, 0, 0, 0, 0];
        let info = aob_lpcm_info(TrackKey { stream: 0xBD, sub: 0xA0, part: 1 }, &h);
        assert_eq!((info.bits, info.sample_rate, info.channels), (Some(24), Some(44_100), Some(6)));
    }

    /// Set DVD_AUDIO_TEST_FOLDER to an AUDIO_TS folder and run `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn converts_folder_from_env() {
        let folder = std::env::var("DVD_AUDIO_TEST_FOLDER").expect("set DVD_AUDIO_TEST_FOLDER");
        let out = std::env::temp_dir().join("dvd_audio_grabber_out");
        let cancel = AtomicBool::new(false);
        let disc = scan(Path::new(&folder), &cancel).unwrap();
        for format in [Format::Mkv, Format::Raw, Format::Wav, Format::Mp3, Format::Ogg, Format::Flac] {
            let wanted: Vec<(usize, TrackKey)> = disc
                .tracks
                .iter()
                .enumerate()
                .filter(|(_, t)| convertible(&t.info, format))
                .map(|(i, t)| (i + 1, t.info.key))
                .collect();
            let total: u64 = disc.tracks.iter().filter(|t| convertible(&t.info, format)).map(|t| t.bytes).sum();
            let report = extract(Path::new(&folder), &out, &wanted, format, "track{n}", total, &cancel, &mut |_| {}).unwrap();
            println!("{format:?}: saved {:?}, skipped {:?}, failed {:?}", report.saved, report.skipped, report.failed);
        }
    }
}
