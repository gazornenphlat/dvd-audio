//! Writes one audio track into a Matroska (.mkv) file.
//! Each block holds a run of audio, and its time comes from how many bytes came before it.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

/// Audio bytes per block. shortcut: one cluster per block, so very long tracks make many small clusters, upgrade to fewer larger ones if players stutter.
const BLOCK_BYTES: usize = 65_536;

pub struct Params {
    pub codec_id: &'static str,
    pub sample_rate: u32,
    pub channels: u8,
    pub bit_depth: Option<u8>,
    /// Audio bytes per second, used to turn a byte count into time.
    pub bytes_per_second: u64,
    /// Bytes per sample frame; a block never splits one.
    pub align: usize,
    /// AC-3: blocks end just before a frame's sync word, so no frame is split.
    pub ac3_frames: bool,
}

pub struct Mkv {
    file: File,
    size_at: u64,
    bytes_per_second: u64,
    align: usize,
    chunk: usize,
    ac3_frames: bool,
    pending: Vec<u8>,
    done: u64,
}

/// How many bytes of `pending` make the next block, or None to wait for more audio.
fn next_cut(pending: &[u8], chunk: usize, ac3_frames: bool) -> Option<usize> {
    if pending.len() < chunk {
        return None;
    }
    if !ac3_frames {
        return Some(chunk);
    }
    match pending[chunk / 2..].windows(2).position(|w| w == [0x0B, 0x77]) {
        Some(p) => Some(chunk / 2 + p),
        None if pending.len() >= 2 * chunk => Some(chunk), // no sync word in sight: give up on aligning
        None => None,
    }
}

impl Mkv {
    pub fn create(path: &Path, p: &Params) -> io::Result<Self> {
        let mut file = File::create(path)?;
        let mut head = Vec::new();

        let mut ebml = Vec::new();
        element(&mut ebml, &[0x42, 0x86], &[1]); // EBMLVersion
        element(&mut ebml, &[0x42, 0xF7], &[1]); // EBMLReadVersion
        element(&mut ebml, &[0x42, 0xF2], &[4]); // EBMLMaxIDLength
        element(&mut ebml, &[0x42, 0xF3], &[8]); // EBMLMaxSizeLength
        element(&mut ebml, &[0x42, 0x82], b"matroska"); // DocType
        element(&mut ebml, &[0x42, 0x87], &[4]); // DocTypeVersion
        element(&mut ebml, &[0x42, 0x85], &[2]); // DocTypeReadVersion
        element(&mut head, &[0x1A, 0x45, 0xDF, 0xA3], &ebml);

        // The segment's size is filled in by finish.
        head.extend([0x18, 0x53, 0x80, 0x67]);
        let size_at = head.len() as u64;
        head.extend(size8(0));

        let mut info = Vec::new();
        element(&mut info, &[0x2A, 0xD7, 0xB1], &1_000_000u64.to_be_bytes()); // timecodes in milliseconds
        element(&mut head, &[0x15, 0x49, 0xA9, 0x66], &info);

        let mut audio = Vec::new();
        element(&mut audio, &[0xB5], &(p.sample_rate as f64).to_be_bytes()); // SamplingFrequency
        element(&mut audio, &[0x9F], &[p.channels]); // Channels
        if let Some(bits) = p.bit_depth {
            element(&mut audio, &[0x62, 0x64], &[bits]); // BitDepth
        }
        let mut track = Vec::new();
        element(&mut track, &[0xD7], &[1]); // TrackNumber
        element(&mut track, &[0x73, 0xC5], &[1]); // TrackUID
        element(&mut track, &[0x83], &[2]); // TrackType: audio
        element(&mut track, &[0x86], p.codec_id.as_bytes()); // CodecID
        element(&mut track, &[0xE1], &audio); // Audio
        let mut tracks = Vec::new();
        element(&mut tracks, &[0xAE], &track); // TrackEntry
        element(&mut head, &[0x16, 0x54, 0xAE, 0x6B], &tracks);

        file.write_all(&head)?;
        let align = p.align.max(1);
        Ok(Mkv {
            file,
            size_at,
            bytes_per_second: p.bytes_per_second.max(1),
            align,
            chunk: BLOCK_BYTES / align * align,
            ac3_frames: p.ac3_frames,
            pending: Vec::new(),
            done: 0,
        })
    }

    pub fn block(&mut self, data: &[u8]) -> io::Result<()> {
        self.pending.extend_from_slice(data);
        while let Some(n) = next_cut(&self.pending, self.chunk, self.ac3_frames) {
            self.flush(n)?;
        }
        Ok(())
    }

    fn flush(&mut self, n: usize) -> io::Result<()> {
        let mut block = vec![0x81, 0, 0, 0x80]; // track 1, time 0 inside its cluster, keyframe
        block.extend(self.pending.drain(..n));
        let time_ms = self.done * 1000 / self.bytes_per_second;
        let mut body = Vec::new();
        element(&mut body, &[0xE7], &time_ms.to_be_bytes()); // Timecode
        element(&mut body, &[0xA3], &block); // SimpleBlock
        let mut cluster = vec![0x1F, 0x43, 0xB6, 0x75];
        cluster.extend(size8(body.len() as u64));
        cluster.extend(body);
        self.file.write_all(&cluster)?;
        self.done += n as u64;
        Ok(())
    }

    /// Writes the last block, fills in the segment size, and returns the file size.
    pub fn finish(mut self) -> io::Result<u64> {
        let rest = self.pending.len() / self.align * self.align; // a partial sample frame at the very end is dropped
        if rest > 0 {
            self.flush(rest)?;
        }
        let end = self.file.stream_position()?;
        self.file.seek(SeekFrom::Start(self.size_at))?;
        self.file.write_all(&size8(end - self.size_at - 8))?;
        Ok(end)
    }
}

/// A size field, 8 bytes wide so it can be filled in later.
fn size8(n: u64) -> [u8; 8] {
    let mut b = n.to_be_bytes();
    b[0] = 0x01;
    b
}

fn element(out: &mut Vec<u8>, id: &[u8], payload: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&size8(payload.len() as u64));
    out.extend_from_slice(payload);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_in_the_segment_size() {
        let path = std::env::temp_dir().join("dvd_audio_grabber_test.mkv");
        let params = Params { codec_id: "A_AC3", sample_rate: 48_000, channels: 2, bit_depth: None, bytes_per_second: 24_000, align: 1, ac3_frames: true };
        let mut mkv = Mkv::create(&path, &params).unwrap();
        mkv.block(&vec![7u8; 100_000]).unwrap();
        let size = mkv.finish().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, size);
        let at = bytes.windows(4).position(|w| w == [0x18, 0x53, 0x80, 0x67]).unwrap() + 4;
        let seg = u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap()) & ((1 << 56) - 1);
        assert_eq!(seg as usize, bytes.len() - at - 8);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn cuts_ac3_blocks_at_sync_words() {
        let mut data = vec![0u8; 5000];
        for at in [0, 1000, 3000] {
            data[at..at + 2].copy_from_slice(&[0x0B, 0x77]);
        }
        // 4096-byte blocks: the cut goes at the first sync word past the halfway mark (2048)
        assert_eq!(next_cut(&data, 4096, true), Some(3000));
        assert_eq!(next_cut(&data[..3000], 4096, true), None);
        assert_eq!(next_cut(&data, 4096, false), Some(4096));
    }
}
