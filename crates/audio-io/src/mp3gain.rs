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
}
