//! Writing Matroska / WebM: EBML header, a Segment with SeekHead, Info (with duration),
//! Tracks, Clusters of SimpleBlocks (a new one at each video keyframe, and at least every
//! five seconds) and Cues for the keyframes, so players can seek. Streams: nothing is held
//! back except the cue list; sizes are patched in at the end.

use dotty_core::{Error, Result, WriteSeek};
use std::io::SeekFrom;

const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const INFO: u32 = 0x1549_A966;
const TRACKS: u32 = 0x1654_AE6B;
const CLUSTER: u32 = 0x1F43_B675;
const CUES: u32 = 0x1C53_BB6B;
const VOID: u32 = 0xEC;
/// Room reserved after the segment start for the SeekHead written at the end.
const SEEK_HEAD_ROOM: u64 = 96;
/// Longest cluster, in milliseconds (block times are 16-bit offsets from the cluster's).
const MAX_CLUSTER_MS: i64 = 5000;

fn io(e: std::io::Error) -> Error {
    Error::Codec(format!("write failed: {e}"))
}

fn id_bytes(id: u32) -> Vec<u8> {
    id.to_be_bytes().iter().copied().skip_while(|&b| b == 0).collect()
}

/// An EBML size in as few bytes as it needs.
fn size_bytes(n: u64) -> Vec<u8> {
    let len = (1..=8).find(|&l| n < (1u64 << (7 * l)) - 1).unwrap_or(8);
    let mut v = n.to_be_bytes()[8 - len..].to_vec();
    v[0] |= 0x80 >> (len - 1);
    v
}

fn element(id: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = id_bytes(id);
    v.extend(size_bytes(payload.len() as u64));
    v.extend_from_slice(payload);
    v
}

fn uint(id: u32, n: u64) -> Vec<u8> {
    let bytes = n.to_be_bytes();
    let skip = bytes.iter().take(7).take_while(|&&b| b == 0).count();
    element(id, &bytes[skip..])
}

fn float(id: u32, f: f64) -> Vec<u8> {
    element(id, &f.to_be_bytes())
}

fn string(id: u32, s: &str) -> Vec<u8> {
    element(id, s.as_bytes())
}

/// A Void element of exactly `n` bytes (n ≥ 2).
fn void(n: u64) -> Vec<u8> {
    // One ID byte, then the size field takes the rest of the header.
    let body = n - 1;
    let size_len = (1..=8).find(|&l| body - l < (1u64 << (7 * l)) - 1).unwrap_or(8);
    let payload = body - size_len;
    let mut v = vec![VOID as u8];
    let mut s = payload.to_be_bytes()[8 - size_len as usize..].to_vec();
    s[0] |= 0x80 >> (size_len - 1);
    v.extend(s);
    v.resize(n as usize, 0);
    v
}

pub struct VideoTrack {
    /// `V_VP9`, `V_AV1`.
    pub codec: &'static str,
    pub width: u32,
    pub height: u32,
    pub private: Option<Vec<u8>>,
    /// Matrix the picture was converted with (ISO/IEC 23001-8): 1 = BT.709, 6 = BT.601.
    pub matrix: u8,
}

pub struct AudioTrack {
    pub channels: u8,
    /// OpusHead.
    pub private: Vec<u8>,
    /// Pre-skip, in nanoseconds.
    pub delay_ns: u64,
}

pub const VIDEO: u64 = 1;
pub const AUDIO: u64 = 2;

pub struct MkvWriter<'a> {
    out: &'a mut dyn WriteSeek,
    /// Where the segment's payload starts (positions inside it are relative to this).
    segment: u64,
    duration_at: u64,
    info_at: u64,
    tracks_at: u64,
    /// The open cluster: where its size goes, and its time.
    cluster: Option<(u64, i64)>,
    cues: Vec<(i64, u64)>,
    last_ms: i64,
}

impl<'a> MkvWriter<'a> {
    pub fn new(out: &'a mut dyn WriteSeek, webm: bool, video: &VideoTrack, audio: Option<&AudioTrack>) -> Result<Self> {
        let doc = if webm { "webm" } else { "matroska" };
        let header = element(
            EBML,
            &[uint(0x4286, 1), uint(0x42F7, 1), uint(0x42F2, 4), uint(0x42F3, 8), string(0x4282, doc), uint(0x4287, 4), uint(0x4285, 2)].concat(),
        );
        out.write_all(&header).map_err(io)?;
        // Segment with an 8-byte size patched at the end.
        out.write_all(&id_bytes(SEGMENT)).map_err(io)?;
        out.write_all(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]).map_err(io)?;
        let segment = out.stream_position().map_err(io)?;
        out.write_all(&void(SEEK_HEAD_ROOM)).map_err(io)?;

        let info_at = out.stream_position().map_err(io)? - segment;
        let info_head = [uint(0x2AD7B1, 1_000_000), string(0x4D80, "Dotty"), string(0x5741, "Dotty")].concat();
        let duration = float(0x4489, 0.0);
        let info = element(INFO, &[info_head.clone(), duration.clone()].concat());
        // The duration's 8 bytes sit at the end of Info.
        let duration_at = segment + info_at + info.len() as u64 - 8;
        out.write_all(&info).map_err(io)?;

        let tracks_at = out.stream_position().map_err(io)? - segment;
        let mut v = [uint(0xD7, VIDEO), uint(0x73C5, VIDEO), uint(0x83, 1), uint(0x9C, 0), string(0x86, video.codec)].concat();
        if let Some(p) = &video.private {
            v.extend(element(0x63A2, p));
        }
        // Colour: matrix, transfer and primaries share one value for BT.709 and BT.601
        // (SMPTE 170M, 6), as do their code points; limited ("broadcast") range.
        let colour = element(0x55B0, &[uint(0x55B1, video.matrix as u64), uint(0x55BA, video.matrix as u64), uint(0x55BB, video.matrix as u64), uint(0x55B9, 1)].concat());
        v.extend(element(0xE0, &[uint(0xB0, video.width as u64), uint(0xBA, video.height as u64), colour].concat()));
        let mut entries = element(0xAE, &v);
        if let Some(a) = audio {
            let mut e = [uint(0xD7, AUDIO), uint(0x73C5, AUDIO), uint(0x83, 2), uint(0x9C, 0), string(0x86, "A_OPUS")].concat();
            e.extend(element(0x63A2, &a.private));
            e.extend(uint(0x56AA, a.delay_ns));
            e.extend(uint(0x56BB, 80_000_000)); // SeekPreRoll: 80 ms, as Opus needs
            e.extend(element(0xE1, &[float(0xB5, 48000.0), uint(0x9F, a.channels as u64)].concat()));
            entries.extend(element(0xAE, &e));
        }
        out.write_all(&element(TRACKS, &entries)).map_err(io)?;
        Ok(Self { out, segment, duration_at, info_at, tracks_at, cluster: None, cues: Vec::new(), last_ms: 0 })
    }

    fn close_cluster(&mut self) -> Result<()> {
        if let Some((size_at, _)) = self.cluster.take() {
            let end = self.out.stream_position().map_err(io)?;
            let size = end - size_at - 8;
            self.out.seek(SeekFrom::Start(size_at)).map_err(io)?;
            self.out.write_all(&[&[0x01u8][..], &size.to_be_bytes()[1..]].concat()).map_err(io)?;
            self.out.seek(SeekFrom::Start(end)).map_err(io)?;
        }
        Ok(())
    }

    /// Add a frame of `track` at `ms` milliseconds (frames in time order, across tracks).
    pub fn write(&mut self, track: u64, ms: i64, key: bool, data: &[u8]) -> Result<()> {
        let new_cluster = match self.cluster {
            None => true,
            Some((_, start)) => (track == VIDEO && key) || ms - start >= MAX_CLUSTER_MS || ms < start,
        };
        if new_cluster {
            self.close_cluster()?;
            let pos = self.out.stream_position().map_err(io)?;
            self.out.write_all(&id_bytes(CLUSTER)).map_err(io)?;
            let size_at = self.out.stream_position().map_err(io)?;
            self.out.write_all(&[0x01, 0, 0, 0, 0, 0, 0, 0]).map_err(io)?;
            self.out.write_all(&uint(0xE7, ms.max(0) as u64)).map_err(io)?;
            self.cluster = Some((size_at, ms));
            if track == VIDEO && key {
                self.cues.push((ms, pos - self.segment));
            }
        }
        let start = self.cluster.map_or(0, |c| c.1);
        let mut block = vec![0x80 | track as u8];
        block.extend_from_slice(&((ms - start) as i16).to_be_bytes());
        block.push(if key { 0x80 } else { 0 });
        self.out.write_all(&id_bytes(0xA3)).map_err(io)?;
        self.out.write_all(&size_bytes((block.len() + data.len()) as u64)).map_err(io)?;
        self.out.write_all(&block).map_err(io)?;
        self.out.write_all(data).map_err(io)?;
        self.last_ms = self.last_ms.max(ms);
        Ok(())
    }

    /// Close the file: cues, duration (milliseconds), sizes, SeekHead.
    pub fn finish(mut self, duration_ms: f64) -> Result<()> {
        self.close_cluster()?;
        let cues_at = self.out.stream_position().map_err(io)? - self.segment;
        let points: Vec<u8> = self
            .cues
            .iter()
            .flat_map(|&(ms, pos)| {
                element(0xBB, &[uint(0xB3, ms.max(0) as u64), element(0xB7, &[uint(0xF7, VIDEO), uint(0xF1, pos)].concat())].concat())
            })
            .collect();
        if !points.is_empty() {
            self.out.write_all(&element(CUES, &points)).map_err(io)?;
        }
        let end = self.out.stream_position().map_err(io)?;
        // Segment size.
        self.out.seek(SeekFrom::Start(self.segment - 7)).map_err(io)?;
        self.out.write_all(&(end - self.segment).to_be_bytes()[1..]).map_err(io)?;
        // Duration.
        self.out.seek(SeekFrom::Start(self.duration_at)).map_err(io)?;
        self.out.write_all(&duration_ms.to_be_bytes()).map_err(io)?;
        // SeekHead in the reserved room, padded with Void.
        let seek = |id: u32, pos: u64| element(SEEK, &[element(SEEK_ID, &id_bytes(id)), uint(SEEK_POSITION, pos)].concat());
        let mut entries = [seek(INFO, self.info_at), seek(TRACKS, self.tracks_at)].concat();
        if !points.is_empty() {
            entries.extend(seek(CUES, cues_at));
        }
        let head = element(SEEK_HEAD, &entries);
        if head.len() as u64 + 2 <= SEEK_HEAD_ROOM {
            self.out.seek(SeekFrom::Start(self.segment)).map_err(io)?;
            self.out.write_all(&head).map_err(io)?;
            self.out.write_all(&void(SEEK_HEAD_ROOM - head.len() as u64)).map_err(io)?;
        }
        self.out.seek(SeekFrom::Start(end)).map_err(io)?;
        let _ = self.last_ms;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ebml_sizes_and_voids() {
        assert_eq!(size_bytes(0), vec![0x80]);
        assert_eq!(size_bytes(126), vec![0xFE]);
        assert_eq!(size_bytes(127), vec![0x40, 0x7F], "0x7F alone would mean 'unknown'");
        assert_eq!(uint(0xD7, 1), vec![0xD7, 0x81, 1]);
        assert_eq!(uint(0x2AD7B1, 1_000_000), vec![0x2A, 0xD7, 0xB1, 0x83, 0x0F, 0x42, 0x40]);
        for n in [2u64, 3, 10, 96, 130, 200] {
            let v = void(n);
            assert_eq!(v.len() as u64, n, "void({n})");
            assert_eq!(v[0], 0xEC);
        }
    }
}
