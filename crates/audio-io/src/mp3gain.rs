//! Lossless MP3 gain: moves every Layer III granule's `global_gain` in 1.5 dB steps, written from
//! ISO/IEC 11172-3 and 13818-3.

// Items are used by the frame walker and gain writer added next.
#![cfg_attr(not(test), allow(dead_code))]

/// CRC-16 over `bytes`: polynomial 0x8005, MSB first, not reflected, no final XOR (11172-3 2.4.3.1).
pub(crate) fn crc16(init: u16, bytes: &[u8]) -> u16 {
    bytes.iter().fold(init, |crc, &b| (0..8).fold(crc ^ (u16::from(b) << 8), |c, _| if c & 0x8000 != 0 { (c << 1) ^ 0x8005 } else { c << 1 }))
}

/// MPEG audio version from the header's 2 version bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    Mpeg1,
    Mpeg2,
    Mpeg25,
}

/// Layer III bitrates in kbit/s for indexes 1..=14 (0 is free format, 15 invalid).
const KBPS_MPEG1: [u32; 14] = [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
const KBPS_MPEG2: [u32; 14] = [8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];

/// A Layer III frame header (32 bits, big-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameHeader {
    pub version: Version,
    pub rate: u32,
    pub kbps: u32,
    pub mono: bool,
    /// A 16-bit CRC follows the header (protection bit 0).
    pub crc: bool,
    /// Whole frame in bytes, header included.
    pub len: usize,
}

impl FrameHeader {
    /// Parse a Layer III header; None for anything else, free format, or an invalid index.
    pub fn parse(b: [u8; 4]) -> Option<Self> {
        let h = u32::from_be_bytes(b);
        if h >> 21 != 0x7FF || (h >> 17) & 3 != 0b01 {
            return None;
        }
        let version = match (h >> 19) & 3 {
            0b00 => Version::Mpeg25,
            0b10 => Version::Mpeg2,
            0b11 => Version::Mpeg1,
            _ => return None,
        };
        let crc = (h >> 16) & 1 == 0;
        let bitrate = usize::try_from((h >> 12) & 0xF).ok()?;
        let kbps = *match version {
            Version::Mpeg1 => &KBPS_MPEG1,
            Version::Mpeg2 | Version::Mpeg25 => &KBPS_MPEG2,
        }
        .get(bitrate.checked_sub(1)?)?;
        let rate = match version {
            Version::Mpeg1 => [44_100, 48_000, 32_000],
            Version::Mpeg2 => [22_050, 24_000, 16_000],
            Version::Mpeg25 => [11_025, 12_000, 8_000],
        };
        let rate = *rate.get(usize::try_from((h >> 10) & 3).ok()?)?;
        let padding = (h >> 9) & 1;
        let per_kbps: u32 = if version == Version::Mpeg1 { 144_000 } else { 72_000 };
        let len = per_kbps.checked_mul(kbps)?.checked_div(rate)?.checked_add(padding)?;
        Some(Self { version, rate, kbps, mono: (h >> 6) & 3 == 0b11, crc, len: usize::try_from(len).ok()? })
    }

    /// Side info bytes: MPEG-1 17 mono / 32 stereo, MPEG-2/2.5 9 mono / 17 stereo.
    pub fn side_info_len(&self) -> usize {
        match (self.version, self.mono) {
            (Version::Mpeg1, true) => 17,
            (Version::Mpeg1, false) => 32,
            (_, true) => 9,
            (_, false) => 17,
        }
    }

    /// Offset of the side info in the frame: after the header and its CRC, if any.
    pub fn side_info_at(&self) -> usize {
        if self.crc { 6 } else { 4 }
    }
}

/// Why a file's gain cannot be changed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GainError {
    #[error("not an MPEG audio Layer III file")]
    NotLayer3,
    #[error("free-format bitrate is not supported")]
    FreeFormat,
    #[error("frame sync lost at byte {0}")]
    SyncLost(usize),
    #[error("global_gain would leave 0..=255 for every step")]
    NoRoom,
}

/// One audio frame: its offset, header, and whether it is a Xing/Info/VBRI frame (no audio).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Frame {
    pub at: usize,
    pub header: FrameHeader,
    pub info: bool,
}

/// Where the audio sits between the leading and trailing tags, and its whole frames.
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    pub audio_start: usize,
    pub audio_end: usize,
    pub frames: Vec<Frame>,
}

/// How far past the leading tags the first frame is looked for.
const SEARCH: usize = 64 * 1024;

fn header_at(b: &[u8], at: usize) -> Option<FrameHeader> {
    FrameHeader::parse(b.get(at..at.checked_add(4)?)?.try_into().ok()?)
}

/// A Layer III header with bitrate index 0 (free format).
fn free_format_at(b: &[u8], at: usize) -> bool {
    matches!(b.get(at..at.saturating_add(4)), Some(&[0xFF, b1, b2, _]) if b1 & 0xE6 == 0xE2 && b1 & 0x18 != 0x08 && b2 >> 4 == 0)
}

/// Offset after the ID3v2 tags at the start: 10-byte header, syncsafe size at 6..10, plus a
/// 10-byte footer when flag 0x10 is set. Files may carry more than one.
fn skip_id3v2(b: &[u8]) -> usize {
    let mut at = 0usize;
    while let (Some(b"ID3"), Some(&flags), Some(size)) = (b.get(at..at + 3), b.get(at + 5), b.get(at + 6..at + 10)) {
        let size = size.iter().fold(0usize, |n, &x| (n << 7) | usize::from(x & 0x7F));
        let footer = if flags & 0x10 != 0 { 10 } else { 0 };
        at = at.saturating_add(10 + footer).saturating_add(size).min(b.len());
    }
    at
}

/// End of the audio: before a trailing ID3v1 tag (128 bytes, `TAG`), then before an APEv2 tag
/// (32-byte `APETAGEX` footer; size, footer included, is the LE u32 at 12; flags bit 31 = has a
/// 32-byte header).
fn trim_tags(b: &[u8], start: usize) -> usize {
    let mut end = b.len();
    if let Some(v1) = end.checked_sub(128).filter(|&at| b.get(at..at + 3) == Some(b"TAG")) {
        end = v1;
    }
    let le = |at: usize| b.get(at..at + 4).and_then(|x| x.try_into().ok()).map(u32::from_le_bytes);
    if let Some(foot) = end.checked_sub(32).filter(|&at| b.get(at..at + 8) == Some(b"APETAGEX"))
        && let (Some(size), Some(flags)) = (le(foot + 12), le(foot + 20))
    {
        let header = if flags & 0x8000_0000 != 0 { 32 } else { 0 };
        let total = usize::try_from(size).ok().and_then(|n| n.checked_add(header));
        if let Some(at) = total.and_then(|n| end.checked_sub(n)) {
            end = at;
        }
    }
    end.max(start)
}

/// Xing/Info after zeroed side info, or VBRI 32 bytes after the header.
fn is_info(b: &[u8], at: usize, h: &FrameHeader) -> bool {
    let side = at + h.side_info_at();
    let zero = b.get(side..side + h.side_info_len()).is_some_and(|s| s.iter().all(|&x| x == 0));
    let tag = b.get(side + h.side_info_len()..side + h.side_info_len() + 4);
    (zero && matches!(tag, Some(b"Xing" | b"Info"))) || b.get(at + 36..at + 40) == Some(b"VBRI")
}

/// Find the audio between the tags and chain its frames.
pub(crate) fn frames(b: &[u8]) -> Result<Layout, GainError> {
    let audio_start = skip_id3v2(b);
    let audio_end = trim_tags(b, audio_start);
    let audio = b.get(..audio_end).unwrap_or(b);
    let search_end = audio_start.saturating_add(SEARCH).min(audio_end);
    let mut free = false;
    let first = (audio_start..search_end).find(|&at| {
        free |= free_format_at(audio, at);
        header_at(audio, at).is_some_and(|h| {
            let next = at + h.len;
            next == audio_end || header_at(audio, next).is_some_and(|n| (n.version, n.rate) == (h.version, h.rate))
        })
    });
    let Some(mut at) = first else { return Err(if free { GainError::FreeFormat } else { GainError::NotLayer3 }) };
    let mut frames = Vec::new();
    while at.checked_add(4).is_some_and(|e| e <= audio_end) {
        let header = header_at(audio, at).ok_or(GainError::SyncLost(at))?;
        let next = at.checked_add(header.len).ok_or(GainError::SyncLost(at))?;
        if next > audio_end {
            break;
        }
        frames.push(Frame { at, header, info: is_info(audio, at, &header) });
        at = next;
    }
    Ok(Layout { audio_start, audio_end, frames })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc16_matches_the_standard_check_value() {
        // CRC-16/CMS (poly 0x8005, init 0xFFFF, no reflection, no xorout): check = 0xAEE7.
        assert_eq!(crc16(0xFFFF, b"123456789"), 0xAEE7);
    }

    #[test]
    fn headers_parse_and_give_frame_lengths() {
        // MPEG-1 L3 128 kbit/s 44.1 kHz stereo, no CRC, no padding: FF FB 90 00 -> 417 bytes.
        let h = FrameHeader::parse([0xFF, 0xFB, 0x90, 0x00]).unwrap();
        assert_eq!((h.version, h.rate, h.kbps, h.mono, h.crc, h.len), (Version::Mpeg1, 44_100, 128, false, false, 417));
        assert_eq!((h.side_info_len(), h.side_info_at()), (32, 4));
        // Same with padding: 418.
        assert_eq!(FrameHeader::parse([0xFF, 0xFB, 0x92, 0x00]).unwrap().len, 418);
        // MPEG-2 L3 64 kbit/s 22.05 kHz mono with CRC: FF F2 80 C0 -> 72000*64/22050 = 208.
        let h = FrameHeader::parse([0xFF, 0xF2, 0x80, 0xC0]).unwrap();
        assert_eq!((h.version, h.rate, h.mono, h.crc, h.len), (Version::Mpeg2, 22_050, true, true, 208));
        assert_eq!((h.side_info_len(), h.side_info_at()), (9, 6));
        // Rate index 1 (0x84) is 24 kHz: 72000*64/24000 = 192.
        assert_eq!(FrameHeader::parse([0xFF, 0xF2, 0x84, 0xC0]).map(|h| (h.rate, h.len)), Some((24_000, 192)));
        // MPEG-2.5 L3 8 kbit/s 8 kHz stereo: FF E3 18 00 -> 72000*8/8000 = 72.
        let h = FrameHeader::parse([0xFF, 0xE3, 0x18, 0x00]).unwrap();
        assert_eq!((h.version, h.rate, h.kbps, h.len, h.side_info_len()), (Version::Mpeg25, 8_000, 8, 72, 17));
    }

    #[test]
    fn bad_headers_are_rejected() {
        for b in [
            [0xFF, 0xFB, 0x00, 0x00], // free format
            [0xFF, 0xFB, 0xF0, 0x00], // bitrate 15
            [0xFF, 0xFB, 0x9C, 0x00], // rate 3
            [0xFF, 0xFD, 0x90, 0x00], // layer II
            [0xFF, 0xEB, 0x90, 0x00], // version reserved
            [0xFE, 0xFB, 0x90, 0x00], // no sync
        ] {
            assert!(FrameHeader::parse(b).is_none(), "{b:02X?}");
        }
    }
    const CBR128: &[u8] = include_bytes!("../tests/fixtures/mp3/cbr128_44k_stereo.mp3");
    const VBR48: &[u8] = include_bytes!("../tests/fixtures/mp3/vbr_48k_stereo.mp3");
    const CBR64_MONO: &[u8] = include_bytes!("../tests/fixtures/mp3/cbr64_44k_mono.mp3");
    const MPEG2_MONO: &[u8] = include_bytes!("../tests/fixtures/mp3/mpeg2_22k_mono.mp3");
    const MPEG2_STEREO: &[u8] = include_bytes!("../tests/fixtures/mp3/mpeg2_24k_stereo.mp3");
    const FIXTURES: [&[u8]; 5] = [CBR128, VBR48, CBR64_MONO, MPEG2_MONO, MPEG2_STEREO];

    /// An ID3v2.3 tag of `body` zero bytes, with a footer when `footer` is set.
    fn id3v2(body: usize, footer: bool) -> Vec<u8> {
        let syncsafe = |n: usize| [(n >> 21) as u8 & 0x7F, (n >> 14) as u8 & 0x7F, (n >> 7) as u8 & 0x7F, n as u8 & 0x7F];
        let mut t = b"ID3\x03\x00".to_vec();
        t.push(if footer { 0x10 } else { 0 });
        t.extend(syncsafe(body));
        t.resize(10 + body, 0);
        if footer {
            t.extend(b"3DI\x03\x00\x10");
            t.extend(syncsafe(body));
        }
        t
    }

    /// An APEv2 tag with a header, one item and a footer.
    fn ape() -> Vec<u8> {
        let item = [&5u32.to_le_bytes()[..], &0u32.to_le_bytes(), b"Title\0hello"].concat();
        let size = u32::try_from(item.len() + 32).unwrap();
        let block =
            |flags: u32| [&b"APETAGEX"[..], &2000u32.to_le_bytes(), &size.to_le_bytes(), &1u32.to_le_bytes(), &flags.to_le_bytes(), &[0; 8]].concat();
        [block(0xA000_0000), item, block(0x8000_0000)].concat()
    }

    #[test]
    fn every_fixture_walks_to_the_end() {
        for (i, f) in FIXTURES.iter().enumerate() {
            let l = frames(f).unwrap();
            assert_eq!((l.audio_start, l.audio_end), (44, f.len()), "fixture {i}");
            assert!(l.frames.len() > 20, "fixture {i}");
            let last = l.frames.last().unwrap();
            assert_eq!(last.at + last.header.len, f.len(), "fixture {i} ends on a frame");
            assert!(l.frames.windows(2).all(|w| w[0].at + w[0].header.len == w[1].at));
        }
    }

    #[test]
    fn info_frames_are_marked() {
        for f in [VBR48, MPEG2_STEREO] {
            let l = frames(f).unwrap();
            assert!(l.frames[0].info);
            assert!(l.frames[1..].iter().all(|f| !f.info));
        }
        for f in [CBR128, CBR64_MONO, MPEG2_MONO] {
            assert!(frames(f).unwrap().frames.iter().all(|f| !f.info));
        }
    }

    #[test]
    fn cbr_frame_counts_match_the_duration() {
        for (f, rate, samples) in [(CBR128, 44_100.0, 1152), (CBR64_MONO, 44_100.0, 1152), (MPEG2_MONO, 22_050.0, 576)] {
            let n = frames(f).unwrap().frames.len() as f64;
            let expected = (2.0 * rate / samples as f64).round();
            assert!((n - expected).abs() <= 2.0, "{n} frames, expected {expected}");
        }
    }

    #[test]
    fn leading_id3v2_tags_are_skipped() {
        let (a, b) = (id3v2(290, false), id3v2(50, true));
        assert_eq!((a.len(), b.len()), (300, 70));
        let tagged = [&a[..], &b, CBR128].concat();
        let (l, plain) = (frames(&tagged).unwrap(), frames(CBR128).unwrap());
        assert_eq!(l.audio_start, plain.audio_start + 370);
        assert_eq!(l.frames.len(), plain.frames.len());
        assert_eq!(l.frames[0].at, plain.frames[0].at + 370);
    }

    #[test]
    fn trailing_ape_and_id3v1_tags_are_trimmed() {
        let mut v1 = b"TAG".to_vec();
        v1.resize(128, b' ');
        for f in FIXTURES {
            let tagged = [f, &ape(), &v1].concat();
            let l = frames(&tagged).unwrap();
            assert_eq!(l.audio_end, f.len());
            assert_eq!(l.frames.len(), frames(f).unwrap().frames.len());
        }
    }

    #[test]
    fn a_broken_header_loses_sync() {
        let at = frames(CBR128).unwrap().frames[5].at;
        let mut b = CBR128.to_vec();
        b[at + 1] = 0x00;
        assert_eq!(frames(&b).unwrap_err(), GainError::SyncLost(at));
    }

    #[test]
    fn a_truncated_last_frame_is_left_out() {
        let full = frames(CBR128).unwrap();
        let cut = &CBR128[..CBR128.len() - 100];
        let l = frames(cut).unwrap();
        assert_eq!((l.audio_end, l.frames.len()), (cut.len(), full.frames.len() - 1));
    }

    #[test]
    fn other_files_are_refused() {
        // 1 s of a 16-bit 440 Hz sine in a WAV.
        let pcm: Vec<u8> =
            (0..44_100).flat_map(|i| (((i as f64 * 440.0 * std::f64::consts::TAU / 44_100.0).sin() * 20_000.0) as i16).to_le_bytes()).collect();
        let fmt = [
            &1u16.to_le_bytes()[..],
            &1u16.to_le_bytes(),
            &44_100u32.to_le_bytes(),
            &88_200u32.to_le_bytes(),
            &2u16.to_le_bytes(),
            &16u16.to_le_bytes(),
        ]
        .concat();
        let len = |n: usize| u32::try_from(n).unwrap().to_le_bytes();
        let wav =
            [&b"RIFF"[..], &len(4 + 8 + fmt.len() + 8 + pcm.len()), b"WAVEfmt ", &len(fmt.len()), &fmt, b"data", &len(pcm.len()), &pcm].concat();
        assert_eq!(frames(&wav).unwrap_err(), GainError::NotLayer3);
        assert_eq!(frames(&[]).unwrap_err(), GainError::NotLayer3);
        // Free-format Layer III frames: bitrate index 0.
        let free = [[0xFF, 0xFB, 0x00, 0x00].as_slice(), &[0; 400]].concat().repeat(4);
        assert_eq!(frames(&free).unwrap_err(), GainError::FreeFormat);
    }
}
