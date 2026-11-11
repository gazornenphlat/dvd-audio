# DVD Audio Grabber

A small Windows program that finds the audio tracks on a DVD or Blu-ray disc and saves them as files.

It reads the disc's files directly and does not decrypt copy-protected discs.

## What it reads

- **DVD-Audio:** `ATS_*.AOB` files, using the `.IFO` index to find each track. Supports MLP and PCM (LPCM).
- **DVD-Video:** `VTS_*.VOB` files with AC-3, DTS, and PCM audio.
- **Blu-ray:** the largest `.m2ts` clip in `BDMV\STREAM`, with AC-3, DTS, PCM, and TrueHD/MLP.

## What it saves

| Format | Notes |
| --- | --- |
| WAV | 16- and 24-bit PCM need nothing else. MLP, AC-3, and 20-bit PCM need ffmpeg. |
| MKV | One file per track. AC-3 and 16/24-bit PCM only. |
| Raw streams | `.ac3`, `.dts`, `.mpa`, `.pcm`, `.mlp`, `.thd`, copied from the disc unchanged. |
| MP3, OGG, FLAC | Need ffmpeg. MP3 and OGG use 128 kbit/s. |

## Requirements

- Windows 10 or 11
- [ffmpeg](https://ffmpeg.org/) on the PATH (`winget install Gyan.FFmpeg`), for the formats marked above

## Building

You need the Rust toolchain from [rustup.rs](https://rustup.rs) and the Visual Studio C++ Build Tools.

```
cargo build --release
```

The program is written to `target\release\dvd_audio_grabber_ifo.exe`. The icon is embedded at build time from `icon\dvd_icon.ico`.

To run the tests:

```
cargo test
```

The test that converts a real disc is skipped by default. To run it, set `DVD_AUDIO_TEST_FOLDER` to an `AUDIO_TS` folder and run `cargo test -- --ignored --nocapture`.

## Using it

1. Drop a disc folder on the window, or click **Browse…**. The folder can be `AUDIO_TS`, `VIDEO_TS`, or the disc folder that contains them.
2. Click **Scan for audio**. The window shows the disc type and each track's codec, rate, bit depth, channels, and size.
3. Tick the tracks you want, choose a format and a save folder, and click **Convert selected**.

A track that can't be saved in the chosen format is greyed out, with the reason on its row. **Cancel** stops a scan or conversion and removes the files from that run.

**File names** come from a pattern. Placeholders:

- `{n}` track number
- `{codec}` codec name
- `{rate}` sample rate in kHz
- `{bits}` bit depth
- `{ch}` channel count

The default is `track{n}`. Values the disc doesn't give are left out.

Existing files are never overwritten. A new file gets a number in brackets, such as `track01 (2).wav`.

Your folders, format, and file name pattern are saved in `%APPDATA%\DVD Audio Grabber\settings.txt`.

## Known limitations

- **PCM (uncompressed) tracks:** converted audio from some 24-bit, 6-channel DVD-Audio PCM tracks has been garbled in testing. The cause is not yet found. The MLP path has been checked against reference files on a 16-bit disc.
- **20-bit PCM** can't be saved to MKV.
- **Blu-ray and DVD-Video** have not been checked against real discs.
- **Blu-ray:** only the largest clip is read, so audio that appears only in other clips isn't found.
- **Scanning** reads the whole disc to find track sizes, so large discs take a while.

## Layout

```
src/main.rs      window, layout, and background threads
src/dvd.rs       disc reading, track detection, and conversion
src/mkv.rs       Matroska writer
src/settings.rs  saved settings
src/theme.rs     colours, fonts, and button styles
icon/            the program icon (PNG and ICO)
build.rs         embeds the icon in the .exe
```
