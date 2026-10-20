//! Lossless MP3 gain: moves every Layer III granule's `global_gain` in 1.5 dB steps, written from
//! ISO/IEC 11172-3 and 13818-3.

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

/// Where the audio sits between the leading and trailing tags, and its whole frames. Bytes in
/// `audio_end..tags_at` are trailing data (a partial frame or junk), copied unchanged.
#[derive(Debug, Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Layout {
    pub audio_start: usize,
    /// End of the last whole frame.
    pub audio_end: usize,
    /// Start of the trailing tags (ID3v1, Lyrics3v2, appended ID3v2, APEv2), or the file end.
    pub tags_at: usize,
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
/// 10-byte footer when flag 0x10 is set in a v2.4 tag (earlier versions define no footer). Files
/// may carry more than one.
pub(crate) fn skip_id3v2(b: &[u8]) -> usize {
    let mut at = 0usize;
    while let (Some(&[b'I', b'D', b'3', major, _, flags]), Some(size)) = (b.get(at..at + 6), b.get(at + 6..at + 10)) {
        let footer = if major == 4 && flags & 0x10 != 0 { 10 } else { 0 };
        at = at.saturating_add(10 + footer).saturating_add(syncsafe(size)).min(b.len());
    }
    at
}

pub(crate) fn syncsafe(b: &[u8]) -> usize {
    b.iter().fold(0usize, |n, &x| (n << 7) | usize::from(x & 0x7F))
}

/// The candidate ends of the audio, innermost first, each before one more trailing tag:
/// - ID3v1: the last 128 bytes, starting `TAG`;
/// - Lyrics3v2, right before ID3v1: `LYRICSBEGIN`… then a 6-digit size and `LYRICS200`; the
///   size excludes those 15 bytes;
/// - an appended ID3v2 tag: a 10-byte `3DI` footer, syncsafe size at 6..10, + 20 for header
///   and footer;
/// - APEv2: a 32-byte `APETAGEX` footer; size, footer included, is the LE u32 at 12; flags
///   bit 31 = a 32-byte header too.
fn tag_ends(b: &[u8], start: usize) -> Vec<usize> {
    let mut ends = vec![b.len()];
    let mut end = b.len();
    if let Some(v1) = end.checked_sub(128).filter(|&at| at >= start && b.get(at..at + 3) == Some(b"TAG")) {
        end = v1;
        ends.push(end);
        if let Some(at) = lyrics3v2(b, end).filter(|&at| at >= start) {
            end = at;
            ends.push(end);
        }
    }
    let le = |at: usize| b.get(at..at + 4).and_then(|x| x.try_into().ok()).map(u32::from_le_bytes);
    loop {
        let id3 = end.checked_sub(10).filter(|&at| b.get(at..at + 3) == Some(b"3DI")).and_then(|foot| {
            let total = syncsafe(b.get(foot + 6..foot + 10)?).checked_add(20)?;
            end.checked_sub(total).filter(|&at| b.get(at..at + 3) == Some(b"ID3"))
        });
        let ape = end.checked_sub(32).filter(|&at| b.get(at..at + 8) == Some(b"APETAGEX")).and_then(|foot| {
            let header = le(foot + 20)? & 0x8000_0000 != 0;
            let at = end.checked_sub(usize::try_from(le(foot + 12)?).ok()?.checked_add(if header { 32 } else { 0 })?)?;
            (!header || b.get(at..at + 8) == Some(b"APETAGEX")).then_some(at)
        });
        // A claimed start on a frame header is a corrupt size, not a tag.
        match id3.or(ape).filter(|&at| at >= start && at < end && header_at(b, at).is_none()) {
            Some(at) => {
                end = at;
                ends.push(end);
            }
            None => break,
        }
    }
    ends.reverse();
    ends
}

/// Start of a Lyrics3v2 block ending at `end`.
fn lyrics3v2(b: &[u8], end: usize) -> Option<usize> {
    let foot = end.checked_sub(15)?;
    if b.get(foot + 6..end)? != b"LYRICS200" {
        return None;
    }
    let digits = std::str::from_utf8(b.get(foot..foot + 6)?).ok()?;
    let at = foot.checked_sub(digits.parse::<usize>().ok()?)?;
    (b.get(at..at + 11)? == b"LYRICSBEGIN").then_some(at)
}

/// Xing/Info after zeroed side info, or VBRI 32 bytes after the header.
fn is_info(b: &[u8], at: usize, h: &FrameHeader) -> bool {
    let side = at + h.side_info_at();
    let zero = b.get(side..side + h.side_info_len()).is_some_and(|s| s.iter().all(|&x| x == 0));
    let tag = b.get(side + h.side_info_len()..side + h.side_info_len() + 4);
    (zero && matches!(tag, Some(b"Xing" | b"Info"))) || b.get(at + 36..at + 40) == Some(b"VBRI")
}

/// A header at `at` whose next frame's header has the same version and rate.
fn chained_at(b: &[u8], at: usize) -> Option<FrameHeader> {
    let h = header_at(b, at)?;
    header_at(b, at + h.len).filter(|n| (n.version, n.rate) == (h.version, h.rate)).map(|_| h)
}

/// A free-format header followed, within the largest free-format frame (640 kbit/s at
/// 32 kHz: 2880 bytes), by another with the same version and rate.
fn free_chain_at(b: &[u8], at: usize) -> bool {
    let same = |x: usize| b.get(x + 1) == b.get(at + 1) && b.get(x + 2).map(|v| v & 0x0C) == b.get(at + 2).map(|v| v & 0x0C);
    free_format_at(b, at) && (at + 5..at + 2_882).any(|x| free_format_at(b, x) && same(x))
}

/// Chain the frames of `b[..end]` from the first one found at or after `start`. Returns them
/// and where the chain stops; that is `end` unless trailing data follows.
fn walk(b: &[u8], start: usize, end: usize) -> Result<(Vec<Frame>, usize), GainError> {
    let audio = b.get(..end).unwrap_or(b);
    let search_end = start.saturating_add(SEARCH).min(end);
    // A lone frame counts only if it starts the audio and fills it.
    let first = (start..search_end)
        .find_map(|at| chained_at(audio, at).or_else(|| header_at(audio, at).filter(|h| at == start && at + h.len == end)).map(|h| (at, h)));
    let Some((mut at, first)) = first else {
        return Err(if (start..search_end).any(|at| free_chain_at(audio, at)) { GainError::FreeFormat } else { GainError::NotLayer3 });
    };
    let mut frames: Vec<Frame> = Vec::new();
    while at < end {
        match header_at(audio, at).filter(|h| (h.version, h.rate) == (first.version, first.rate)) {
            Some(header) if at + header.len <= end => {
                frames.push(Frame { at, header, info: frames.is_empty() && is_info(audio, at, &header) });
                at += header.len;
            }
            // Trailing data, unless frames chain again further on (corruption mid-file).
            _ if frames.len() >= 2 && !(at..end).any(|x| chained_at(audio, x).is_some()) => break,
            _ => return Err(GainError::SyncLost(at)),
        }
    }
    Ok((frames, at))
}

/// Find the audio between the tags and chain its frames. A trailing tag is trusted only if the
/// frames end exactly where it starts; otherwise the walk is retried with it counted as audio.
/// Without an exact match, the successful walk with the most audio wins (all share the same
/// chain, so it holds the others; a corrupt trim can stop one mid-frame, and an outer one may
/// fail on frame-like bytes in a tag body), else the last error.
pub(crate) fn frames(b: &[u8]) -> Result<Layout, GainError> {
    let audio_start = skip_id3v2(b);
    let (mut inner, mut err) = (None, GainError::NotLayer3);
    for tags_at in tag_ends(b, audio_start) {
        match walk(b, audio_start, tags_at) {
            Ok((frames, audio_end)) if audio_end == tags_at => return Ok(Layout { audio_start, audio_end, tags_at, frames }),
            Ok((frames, audio_end)) => {
                if inner.as_ref().is_none_or(|l: &Layout| audio_end > l.audio_end) {
                    inner = Some(Layout { audio_start, audio_end, tags_at, frames });
                }
            }
            Err(e) => err = e,
        }
    }
    inner.ok_or(err)
}

/// One global_gain step in dB: amplitude ×2^(1/4).
pub const STEP_DB: f64 = 1.505_149_978_319_906;

/// Bit offsets of each granule/channel block's 8-bit global_gain in the side info: after the
/// side-info bits before the blocks, block `gr * channels + ch`, 21 bits in (after
/// part2_3_length 12 and big_values 9).
fn gain_bits(h: &FrameHeader) -> impl Iterator<Item = usize> {
    // (bits before the blocks, block size, granules); MPEG-1 main_data_begin 9 + private 5/3 +
    // scfsi 4 per channel, MPEG-2/2.5 main_data_begin 8 + private 1/2.
    let (before, size, granules) = match (h.version, h.mono) {
        (Version::Mpeg1, true) => (18, 59, 2),
        (Version::Mpeg1, false) => (20, 59, 2),
        (_, true) => (9, 63, 1),
        (_, false) => (10, 63, 1),
    };
    let channels = if h.mono { 1 } else { 2 };
    (0..granules * channels).map(move |block| before + block * size + 21)
}

/// `n` (≤ 32) bits at bit offset `bit`, MSB first.
fn get_bits(buf: &[u8], bit: usize, n: usize) -> Option<u32> {
    if n > 32 {
        return None;
    }
    (bit..bit.checked_add(n)?).try_fold(0u32, |v, i| Some((v << 1) | u32::from((buf.get(i / 8)? >> (7 - i % 8)) & 1)))
}

/// Write `v` into `n` (≤ 32) bits at bit offset `bit`, MSB first; None (and nothing written) if
/// the field runs past `buf` or `v` does not fit.
fn set_bits(buf: &mut [u8], bit: usize, n: usize, v: u32) -> Option<()> {
    let end = bit.checked_add(n)?;
    if n > 32 || (n < 32 && v >> n != 0) || end > buf.len().checked_mul(8)? {
        return None;
    }
    for (k, i) in (bit..end).enumerate() {
        let on = (v >> (n - 1 - k)) & 1 == 1;
        let byte = buf.get_mut(i / 8)?;
        let m = 0x80u8 >> (i % 8);
        *byte = if on { *byte | m } else { *byte & !m };
    }
    Some(())
}

/// A granule/channel block with no Huffman data (part2_3_length 0, the 12 bits at its start,
/// 21 bits before global_gain): it decodes to zeros.
fn silent(side: &[u8], gain_bit: usize) -> bool {
    gain_bit.checked_sub(21).and_then(|bit| get_bits(side, bit, 12)) == Some(0)
}

/// Every audio (non-info) frame with the offset of its side info.
fn audio_frames(b: &[u8]) -> Result<Vec<(Frame, usize)>, GainError> {
    let audio: Vec<_> = frames(b)?.frames.into_iter().filter(|f| !f.info).map(|f| (f, f.at + f.header.side_info_at())).collect();
    if audio.is_empty() { Err(GainError::NotLayer3) } else { Ok(audio) }
}

/// The steps that keep every audio granule's global_gain within 0..=255: (most negative, most positive).
/// Silent granules (part2_3_length 0) don't count; [`adjust`] saturates them.
pub fn step_range(bytes: &[u8]) -> Result<(i32, i32), GainError> {
    range(bytes, &audio_frames(bytes)?)
}

fn range(bytes: &[u8], audio: &[(Frame, usize)]) -> Result<(i32, i32), GainError> {
    let (mut lo, mut hi) = (255u32, 0u32);
    for &(f, side) in audio {
        let side = bytes.get(side..).ok_or(GainError::SyncLost(f.at))?;
        for bit in gain_bits(&f.header) {
            if silent(side, bit) {
                continue;
            }
            let g = get_bits(side, bit, 8).ok_or(GainError::SyncLost(f.at))?;
            (lo, hi) = (lo.min(g), hi.max(g));
        }
    }
    let (lo, hi) = (i32::try_from(lo).unwrap_or(0), i32::try_from(hi).unwrap_or(255));
    Ok((-lo, 255 - hi))
}

/// A copy of `bytes` with every audio frame's global_gain moved by `steps` (CRC frames re-protected,
/// info frames, tags and trailing data copied unchanged; silent granules saturate into 0..=255).
/// Once a silent granule has saturated, adjusting back by −`steps` decodes the same but is not
/// byte-identical: keep the original rather than re-adjusting.
pub fn adjust(bytes: &[u8], steps: i32) -> Result<Vec<u8>, GainError> {
    let audio = audio_frames(bytes)?;
    let (lo, hi) = range(bytes, &audio)?;
    if !(lo..=hi).contains(&steps) {
        return Err(GainError::NoRoom);
    }
    let mut out = bytes.to_vec();
    for (f, side) in audio {
        let lost = GainError::SyncLost(f.at);
        let side_bytes = out.get_mut(side..side + f.header.side_info_len()).ok_or(lost.clone())?;
        for bit in gain_bits(&f.header) {
            let g = get_bits(side_bytes, bit, 8).and_then(|g| i32::try_from(g).ok()).ok_or(lost.clone())?;
            let g = g.saturating_add(steps);
            // A silent granule decodes to zeros at any gain, so it saturates instead.
            let g = if silent(side_bytes, bit) { g.clamp(0, 255) } else { g };
            let g = u32::try_from(g).map_err(|_| GainError::NoRoom)?;
            set_bits(side_bytes, bit, 8, g).ok_or(GainError::NoRoom)?;
        }
        if f.header.crc {
            // CRC-16 over header bytes 2..4 then the side info, stored big-endian after the header.
            let head = out.get(f.at + 2..f.at + 4).ok_or(lost.clone())?;
            let side_bytes = out.get(side..side + f.header.side_info_len()).ok_or(lost.clone())?;
            let crc = crc16(crc16(0xFFFF, head), side_bytes);
            out.get_mut(f.at + 4..f.at + 6).ok_or(lost)?.copy_from_slice(&crc.to_be_bytes());
        }
    }
    Ok(out)
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
    const MPEG25_MONO: &[u8] = include_bytes!("../tests/fixtures/mp3/mpeg25_11k_mono.mp3");
    const FIXTURES: [&[u8]; 6] = [CBR128, VBR48, CBR64_MONO, MPEG2_MONO, MPEG2_STEREO, MPEG25_MONO];

    /// An ID3v2 tag of `body` zero bytes: v2.3, or v2.4 with a footer when `footer` is set.
    fn id3v2(body: usize, footer: bool) -> Vec<u8> {
        let syncsafe = |n: usize| [(n >> 21) as u8 & 0x7F, (n >> 14) as u8 & 0x7F, (n >> 7) as u8 & 0x7F, n as u8 & 0x7F];
        let mut t = if footer { b"ID3\x04\x00".to_vec() } else { b"ID3\x03\x00".to_vec() };
        t.push(if footer { 0x10 } else { 0 });
        t.extend(syncsafe(body));
        t.resize(10 + body, 0);
        if footer {
            t.extend(b"3DI\x04\x00\x10");
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
            assert_eq!((l.audio_start, l.audio_end, l.tags_at), (44, f.len(), f.len()), "fixture {i}");
            assert!(l.frames.len() > 20, "fixture {i}");
            let last = l.frames.last().unwrap();
            assert_eq!(last.at + last.header.len, f.len(), "fixture {i} ends on a frame");
            assert!(l.frames.windows(2).all(|w| w[0].at + w[0].header.len == w[1].at));
        }
    }

    #[test]
    fn info_frames_are_marked() {
        for f in [VBR48, MPEG2_STEREO, MPEG25_MONO] {
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
    fn only_a_v24_tag_has_a_footer() {
        let mut v23 = id3v2(50, false);
        v23[5] = 0x10;
        assert_eq!(skip_id3v2(&[&v23[..], CBR128].concat()), 60 + skip_id3v2(CBR128));
        assert_eq!(skip_id3v2(&id3v2(50, true)), 70);
    }

    #[test]
    fn trailing_ape_and_id3v1_tags_are_trimmed() {
        let mut v1 = b"TAG".to_vec();
        v1.resize(128, b' ');
        for f in FIXTURES {
            let tagged = [f, &ape(), &v1].concat();
            let l = frames(&tagged).unwrap();
            assert_eq!((l.audio_end, l.tags_at), (f.len(), f.len()));
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
        let last = full.frames[full.frames.len() - 2];
        assert_eq!((l.audio_end, l.tags_at, l.frames.len()), (last.at + last.header.len, cut.len(), full.frames.len() - 1));
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
    #[test]
    fn bits_round_trip_across_byte_boundaries() {
        let mut buf = [0u8; 6];
        for (bit, n, v) in [(3, 8, 0xA5), (13, 12, 0xABC), (1, 1, 1), (7, 9, 0x1FF), (21, 8, 0x5A), (5, 32, 0xDEAD_BEEF)] {
            let (mut copy, old) = (buf, get_bits(&buf, bit, n).unwrap());
            set_bits(&mut copy, bit, n, v).unwrap();
            assert_eq!(get_bits(&copy, bit, n), Some(v), "bit {bit} n {n}");
            // Nothing outside the field changed.
            set_bits(&mut copy, bit, n, old).unwrap();
            assert_eq!(copy, buf);
            buf[0] ^= 0x5A; // vary the surroundings for the next case
        }
        assert_eq!(get_bits(&[0b1010_0000], 0, 3), Some(0b101));
        assert_eq!(get_bits(&[0; 2], 9, 8), None);
        assert_eq!(set_bits(&mut [0; 2], 9, 8, 1), None);
        assert_eq!(set_bits(&mut [0; 2], 0, 4, 16), None, "value wider than the field");
    }

    #[test]
    fn gain_fields_sit_where_the_spec_puts_them() {
        // Side-info bits before the blocks + block * block size + 21 (part2_3_length 12, big_values 9).
        let offsets = |b: [u8; 4]| gain_bits(&FrameHeader::parse(b).unwrap()).collect::<Vec<_>>();
        assert_eq!(offsets([0xFF, 0xFB, 0x90, 0xC0]), [18 + 21, 18 + 59 + 21]);
        assert_eq!(offsets([0xFF, 0xFB, 0x90, 0x00]), [20 + 21, 20 + 59 + 21, 20 + 118 + 21, 20 + 177 + 21]);
        assert_eq!(offsets([0xFF, 0xF3, 0x80, 0xC0]), [9 + 21]);
        assert_eq!(offsets([0xFF, 0xF3, 0x80, 0x00]), [10 + 21, 10 + 63 + 21]);
        assert_eq!(offsets([0xFF, 0xE3, 0x18, 0x40]), [10 + 21, 10 + 63 + 21]);
    }

    /// Every audio block's global_gain, in file order.
    fn gains(b: &[u8]) -> Vec<u32> {
        let l = frames(b).unwrap();
        let mut out = Vec::new();
        for f in l.frames.iter().filter(|f| !f.info) {
            let side = &b[f.at + f.header.side_info_at()..][..f.header.side_info_len()];
            out.extend(gain_bits(&f.header).map(|bit| get_bits(side, bit, 8).unwrap()));
        }
        out
    }

    /// global_gain of every audio block with Huffman data (part2_3_length > 0).
    fn audible_gains(b: &[u8]) -> Vec<u32> {
        let l = frames(b).unwrap();
        let mut out = Vec::new();
        for f in l.frames.iter().filter(|f| !f.info) {
            let side = &b[f.at + f.header.side_info_at()..][..f.header.side_info_len()];
            out.extend(gain_bits(&f.header).filter(|&bit| get_bits(side, bit - 21, 12) != Some(0)).map(|bit| get_bits(side, bit, 8).unwrap()));
        }
        out
    }

    #[test]
    fn adjusting_up_and_back_restores_every_byte() {
        for f in FIXTURES {
            let up = adjust(f, 2).unwrap();
            assert_ne!(up, f);
            assert_eq!(adjust(&up, -2).unwrap(), f);
            assert_eq!(adjust(f, 0).unwrap(), f);
        }
    }

    #[test]
    fn adjusting_moves_only_global_gain() {
        for f in FIXTURES {
            let out = adjust(f, 3).unwrap();
            assert_eq!(out.len(), f.len());
            let (before, after) = (gains(f), gains(&out));
            assert!(!before.is_empty());
            assert_eq!(after, before.iter().map(|g| g + 3).collect::<Vec<_>>());
            // Mask the global_gain bits; every other bit must match.
            let mut mask = vec![0u8; f.len()];
            for fr in frames(f).unwrap().frames.iter().filter(|fr| !fr.info) {
                let side = fr.at + fr.header.side_info_at();
                for bit in gain_bits(&fr.header) {
                    set_bits(&mut mask[side..], bit, 8, 0xFF).unwrap();
                }
            }
            let stray = f.iter().zip(&out).zip(&mask).position(|((a, b), m)| (a ^ b) & !m != 0);
            assert_eq!(stray, None);
        }
    }

    /// `CBR128` with every frame CRC-protected: protection bit cleared and a CRC inserted after
    /// the header. Frame lengths stay the same, so the 2 bytes come off the main data (the audio
    /// is garbage; only the bits matter).
    fn protected(src: &[u8]) -> Vec<u8> {
        let l = frames(src).unwrap();
        let mut out = src[..l.frames[0].at].to_vec();
        for f in &l.frames {
            let frame = &src[f.at..f.at + f.header.len];
            let mut p = vec![frame[0], frame[1] & !1, frame[2], frame[3], 0, 0];
            p.extend(&frame[4..frame.len() - 2]);
            out.extend(p);
        }
        out.extend(&src[l.frames.last().map(|f| f.at + f.header.len).unwrap()..]);
        let l = frames(&out).unwrap();
        for f in &l.frames {
            let crc = expected_crc(&out, f);
            out[f.at + 4..f.at + 6].copy_from_slice(&crc.to_be_bytes());
        }
        out
    }

    /// CRC over header bytes 2..4 and the side info.
    fn expected_crc(b: &[u8], f: &Frame) -> u16 {
        let side = &b[f.at + 6..][..f.header.side_info_len()];
        crc16(crc16(0xFFFF, &b[f.at + 2..f.at + 4]), side)
    }

    fn stored_crc(b: &[u8], f: &Frame) -> u16 {
        u16::from_be_bytes([b[f.at + 4], b[f.at + 5]])
    }

    #[test]
    fn crc_frames_are_reprotected() {
        for src in [CBR128, MPEG2_MONO, MPEG2_STEREO] {
            crc_frames_are_reprotected_in(src);
        }
    }

    fn crc_frames_are_reprotected_in(src: &[u8]) {
        let p = protected(src);
        let l = frames(&p).unwrap();
        assert_eq!(l.frames.len(), frames(src).unwrap().frames.len());
        assert!(l.frames.iter().all(|f| f.header.crc && stored_crc(&p, f) == expected_crc(&p, f)));
        let out = adjust(&p, 2).unwrap();
        assert_eq!(gains(&out), gains(&p).iter().map(|g| g + 2).collect::<Vec<_>>());
        let out_frames = frames(&out).unwrap().frames;
        for f in &out_frames {
            assert_eq!(stored_crc(&out, f), expected_crc(&out, f), "frame at {}", f.at);
        }
        // The CRCs did have to change, so a skipped recompute fails the check above.
        assert!(out_frames.iter().filter(|f| !f.info).all(|f| stored_crc(&p, f) != expected_crc(&out, f)));
    }

    #[test]
    fn a_stale_crc_is_caught() {
        let mut out = adjust(&protected(CBR128), 2).unwrap();
        let f = frames(&out).unwrap().frames[7];
        out[f.at + 5] ^= 0x01;
        assert_ne!(stored_crc(&out, &f), expected_crc(&out, &f));
    }

    #[test]
    fn step_range_keeps_global_gain_in_a_byte() {
        for f in FIXTURES {
            let g = audible_gains(f);
            let (lo, hi) = (*g.iter().min().unwrap() as i32, *g.iter().max().unwrap() as i32);
            assert_eq!(step_range(f).unwrap(), (-lo, 255 - hi));
            assert!(adjust(f, 255 - hi).is_ok());
            assert!(adjust(f, -lo).is_ok());
            assert_eq!(adjust(f, 255 - hi + 1).unwrap_err(), GainError::NoRoom);
            assert_eq!(adjust(f, -lo - 1).unwrap_err(), GainError::NoRoom);
        }
        // The real encoder leaves silent granules in some fixtures, so the exclusion is exercised.
        assert!(FIXTURES.iter().any(|f| audible_gains(f).len() < gains(f).len()));
    }

    #[test]
    fn tags_and_info_frames_are_copied_unchanged() {
        let mut v1 = b"TAG".to_vec();
        v1.resize(128, b'x');
        let tagged = [&id3v2(290, false)[..], VBR48, &ape(), &v1].concat();
        let out = adjust(&tagged, -2).unwrap();
        let l = frames(&tagged).unwrap();
        let first = l.frames[0];
        assert!(first.info);
        assert_eq!(out[..first.at + first.header.len], tagged[..first.at + first.header.len]);
        assert_eq!(out[l.audio_end..], tagged[l.audio_end..]);
    }

    /// An ID3v1 tag (128 bytes).
    fn id3v1() -> Vec<u8> {
        let mut v1 = b"TAG".to_vec();
        v1.resize(128, b' ');
        v1
    }

    #[test]
    fn lyrics3v2_before_id3v1_is_trimmed() {
        // Size counts LYRICSBEGIN and the fields, not the 6 digits or LYRICS200.
        let lyrics = [&b"LYRICSBEGIN"[..], b"LYR00005hello", b"000024", b"LYRICS200"].concat();
        for f in FIXTURES {
            let l = frames(&[f, &lyrics, &id3v1()].concat()).unwrap();
            assert_eq!((l.audio_end, l.tags_at), (f.len(), f.len()));
            assert_eq!(l.frames.len(), frames(f).unwrap().frames.len());
        }
    }

    #[test]
    fn an_appended_id3v2_tag_with_a_footer_is_trimmed() {
        for f in FIXTURES {
            for tail in [id3v2(500, true), [id3v2(500, true), id3v1()].concat()] {
                let l = frames(&[f, &tail].concat()).unwrap();
                assert_eq!((l.audio_end, l.tags_at), (f.len(), f.len()));
            }
        }
    }

    #[test]
    fn trailing_data_after_the_last_frame_is_kept() {
        for f in FIXTURES {
            let padded = [f, &[0; 1024][..]].concat();
            let l = frames(&padded).unwrap();
            assert_eq!((l.audio_end, l.tags_at, l.frames.len()), (f.len(), padded.len(), frames(f).unwrap().frames.len()));
            let out = adjust(&padded, 2).unwrap();
            assert_eq!(out[f.len()..], padded[f.len()..]);
            assert_eq!(adjust(&out, -2).unwrap(), padded);
        }
    }

    #[test]
    fn garbage_mid_file_loses_sync() {
        let at = frames(CBR128).unwrap().frames[10].at;
        let b = [&CBR128[..at], &[0x55; 50], &CBR128[at..]].concat();
        assert_eq!(frames(&b).unwrap_err(), GainError::SyncLost(at));
    }

    #[test]
    fn a_tag_size_that_cuts_into_the_audio_is_not_trusted() {
        let mut tag = ape();
        let foot = tag.len() - 32;
        tag[foot + 12..foot + 16].copy_from_slice(&5_000u32.to_le_bytes());
        let tagged = [CBR128, &tag, &id3v1()].concat();
        let plain = frames(CBR128).unwrap();
        let l = frames(&tagged).unwrap();
        assert_eq!((l.frames.len(), l.audio_end), (plain.frames.len(), CBR128.len()));
        let out = adjust(&tagged, 2).unwrap();
        assert_eq!(gains(&out), gains(&tagged).iter().map(|g| g + 2).collect::<Vec<_>>());
    }

    #[test]
    fn only_the_first_frame_can_be_an_info_frame() {
        let mut b = CBR128.to_vec();
        let f = frames(CBR128).unwrap().frames[10];
        let side = f.at + f.header.side_info_at();
        b[side..side + f.header.side_info_len()].fill(0);
        b[side + f.header.side_info_len()..][..4].copy_from_slice(b"Info");
        b[f.at + 36..f.at + 40].copy_from_slice(b"VBRI");
        assert!(!frames(&b).unwrap().frames[10].info);
    }

    #[test]
    fn a_lone_frame_counts_only_at_the_start() {
        let f = frames(CBR128).unwrap().frames[3];
        let frame = &CBR128[f.at..f.at + f.header.len];
        assert_eq!(frames(frame).unwrap().frames.len(), 1);
        assert_eq!(frames(&[&b"abc"[..], frame].concat()).unwrap_err(), GainError::NotLayer3);
    }

    #[test]
    fn a_lone_free_format_header_is_not_free_format() {
        let b = [&[0u8; 300][..], &[0xFF, 0xFB, 0x00, 0x00], &[0; 300]].concat();
        assert_eq!(frames(&b).unwrap_err(), GainError::NotLayer3);
    }

    #[test]
    fn silent_granules_do_not_limit_the_range() {
        // Make the last block (gr 1, ch 1) of frames 10..20 silent: part2_3_length 0, big_values
        // 0, global_gain 0, scalefac_compress 0. Nothing in those frames' main data follows it,
        // so the rest still decodes.
        let mut b = CBR128.to_vec();
        let l = frames(CBR128).unwrap();
        for f in &l.frames[10..20] {
            let side = &mut b[f.at + f.header.side_info_at()..][..f.header.side_info_len()];
            let gain = gain_bits(&f.header).last().unwrap();
            for (bit, n) in [(gain - 21, 12), (gain - 9, 9), (gain, 8), (gain + 8, 4)] {
                set_bits(side, bit, n, 0).unwrap();
            }
        }
        let (lo, _) = step_range(&b).unwrap();
        assert!(lo <= -3, "{lo}");
        let (_, x) = crate::decode(&b, Some("mp3")).unwrap();
        for steps in [-3, 2] {
            let out = adjust(&b, steps).unwrap();
            // The silent blocks saturate into 0..=255.
            let f = l.frames[12];
            let gain = gain_bits(&f.header).last().unwrap();
            assert_eq!(get_bits(&out[f.at + f.header.side_info_at()..], gain, 8), Some(steps.max(0) as u32));
            let (_, y) = crate::decode(&out, Some("mp3")).unwrap();
            let (x, y) = (x.interleaved(), y.interleaved());
            assert_eq!(x.len(), y.len());
            let want = 2f64.powf(f64::from(steps) / 4.0);
            let err = x.iter().zip(&y).map(|(&a, &b)| (f64::from(b) - want * f64::from(a)).abs()).fold(0.0, f64::max);
            assert!(err < 1e-4, "{steps}: max residual {err}");
        }
    }

    #[test]
    fn a_tag_size_that_lands_on_a_frame_is_not_trusted() {
        let plain = frames(CBR128).unwrap();
        let start = plain.frames[30].at;
        let item = [&5u32.to_le_bytes()[..], &0u32.to_le_bytes(), b"Title\0hello"].concat();
        // A footer-only tag and one claiming a header, each sized to start exactly on frame 30.
        for flags in [0u32, 0x8000_0000] {
            let header = if flags != 0 { 32 } else { 0 };
            let size = u32::try_from(CBR128.len() - start + item.len() + 32 - header).unwrap();
            let footer = [&b"APETAGEX"[..], &2000u32.to_le_bytes(), &size.to_le_bytes(), &1u32.to_le_bytes(), &flags.to_le_bytes(), &[0; 8]].concat();
            let tagged = [CBR128, &item, &footer, &id3v1()].concat();
            let l = frames(&tagged).unwrap();
            assert_eq!((l.frames.len(), l.audio_end), (plain.frames.len(), CBR128.len()), "flags {flags:#X}");
            let out = adjust(&tagged, 2).unwrap();
            assert_eq!(gains(&out), gains(&tagged).iter().map(|g| g + 2).collect::<Vec<_>>());
        }
    }

    #[test]
    fn frame_like_bytes_in_an_outer_tag_do_not_override_a_good_walk() {
        // CBR128, 1 junk byte, an ID3v2.4 tag (with footer) whose body holds two real frames, ID3v1.
        let l = frames(CBR128).unwrap();
        let body = &CBR128[l.frames[3].at..l.frames[5].at];
        let mut tag = id3v2(body.len(), true);
        tag[10..10 + body.len()].copy_from_slice(body);
        let b = [CBR128, &[0x42][..], &tag, &id3v1()].concat();
        let got = frames(&b).unwrap();
        assert_eq!((got.frames.len(), got.audio_end, got.tags_at), (l.frames.len(), CBR128.len(), CBR128.len() + 1));
    }

    #[test]
    fn a_tag_size_that_lands_mid_frame_is_not_trusted() {
        // A footer-only APE tag whose size claims a start 100 bytes into frame 30.
        let plain = frames(CBR128).unwrap();
        let start = plain.frames[30].at + 100;
        let item = [&5u32.to_le_bytes()[..], &0u32.to_le_bytes(), b"Title hello"].concat();
        let size = u32::try_from(CBR128.len() - start + item.len() + 32).unwrap();
        let footer = [&b"APETAGEX"[..], &2000u32.to_le_bytes(), &size.to_le_bytes(), &1u32.to_le_bytes(), &0u32.to_le_bytes(), &[0; 8]].concat();
        let tagged = [CBR128, &item, &footer, &id3v1()].concat();
        let l = frames(&tagged).unwrap();
        assert_eq!((l.frames.len(), l.audio_end), (plain.frames.len(), CBR128.len()));
        let out = adjust(&tagged, 2).unwrap();
        assert_eq!(gains(&out), gains(&tagged).iter().map(|g| g + 2).collect::<Vec<_>>());
    }
}
