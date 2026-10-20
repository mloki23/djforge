//! Fills an MP3's missing key and BPM tags.

use crate::mp3gain::{skip_id3v2, syncsafe};
use id3::frame::ExtendedText;
use id3::{Content, Encoder, Encoding, Frame, Tag, TagLike, Version};
use std::io::Cursor;
use std::ops::Range;

/// Description of the `TXXX` frame that records what prep did.
pub const PREP_DESCRIPTION: &str = "djforge_prep";

/// What [`fill`] adds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TagFill<'a> {
    /// Camelot key (`9A`), written to `TKEY` when the tag has none.
    pub camelot: Option<&'a str>,
    /// Written, rounded, to `TBPM` when the tag has none; must be finite and within 20–400.
    pub bpm: Option<f64>,
    /// Value of `TXXX:djforge_prep`, always set.
    pub prep_note: &'a str,
}

/// What [`fill`] found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagReport {
    /// `"key"` and/or `"bpm"`, for the frames that were added.
    pub filled: Vec<&'static str>,
    /// The `TKEY` already there (Mixed In Key's), if any.
    pub existing_key: Option<String>,
    /// The `TBPM` already there, if any.
    pub existing_bpm: Option<String>,
    /// `TXXX:EnergyLevel` (Mixed In Key), if any.
    pub energy: Option<String>,
    /// `TCON`, if any.
    pub genre: Option<String>,
    /// Whether `TALB` holds something.
    pub album: bool,
    /// IDs of frames `id3` could not decode; they are copied as they are.
    pub unreadable: Vec<String>,
}

/// Why [`fill`] left a file alone.
#[derive(Debug, thiserror::Error)]
pub enum TagError {
    #[error("not a Camelot key: {0:?}")]
    Key(String),
    #[error("BPM {0} is not between 20 and 400")]
    Bpm(f64),
    #[error("malformed ID3v2 tag: {0}")]
    Malformed(String),
    #[error("unsupported ID3v2 tag: {0}")]
    Unsupported(String),
}

/// `bytes` with its ID3v2 tag replaced by one that has TKEY/TBPM added where missing and
/// TXXX:djforge_prep set; every other frame and the tag version are kept. A file with no tag gets a v2.3 one.
///
/// The old frames are copied byte for byte, in order, and the new ones follow them, taking their
/// room from the padding when there is enough. `id3` only decodes the frames read here and
/// encodes the new ones: decoding a whole tag and encoding it again would merge frames that share
/// a description, cut v2.3 text at its first null, re-encode text and drop frame flags.
///
/// A tag whose frames cannot be walked to its end, or to zero padding, is refused, since new
/// frames could not be placed after them. A v2.3 tag's unsynchronisation and extended header are
/// undone; in a v2.4 tag that is unsynchronised, the new frames are too. Header flags a version
/// does not define are dropped.
pub fn fill(bytes: &[u8], fill: &TagFill) -> Result<(Vec<u8>, TagReport), TagError> {
    let key = fill.camelot.map(|k| if is_camelot(k) { Ok(k.to_owned()) } else { Err(TagError::Key(k.to_owned())) }).transpose()?;
    let bpm = fill.bpm.map(|b| if (20.0..=400.0).contains(&b) { Ok(format!("{}", b.round() as i64)) } else { Err(TagError::Bpm(b)) }).transpose()?;
    let old = if bytes.starts_with(b"ID3") { OldTag::parse(bytes)? } else { OldTag::none() };

    // Read the frames the report needs; the others are only copied.
    let mut report = TagReport::default();
    let mut removed = Vec::new();
    let (mut keys, mut bpms) = (Vec::new(), Vec::new());
    for (i, range) in old.frames.iter().enumerate() {
        let raw = old.body.get(range.clone()).unwrap_or_default();
        let id = raw.get(..4).unwrap_or_default();
        if !matches!(id, b"TKEY" | b"TBPM" | b"TCON" | b"TALB" | b"TXXX") {
            continue;
        }
        let key_or_bpm = matches!(id, b"TKEY" | b"TBPM");
        // A key or BPM frame holding nothing, or only its encoding byte, is blank.
        let frame = if key_or_bpm && raw.len() <= 11 {
            None
        } else {
            match decode(old.major, raw) {
                Ok(frame) => Some(frame),
                Err(e) if key_or_bpm => return Err(e),
                Err(e) if id == b"TXXX" && could_be_note(old.major, raw) => {
                    return Err(TagError::Malformed(format!("{e}; it may be an earlier {PREP_DESCRIPTION} note")));
                }
                Err(_) => {
                    report.unreadable.push(String::from_utf8_lossy(id).into_owned());
                    continue;
                }
            }
        };
        let text = frame.as_ref().and_then(|f| f.content().text()).unwrap_or_default();
        match id {
            b"TKEY" => keys.push((i, text.to_owned())),
            b"TBPM" => bpms.push((i, text.to_owned())),
            b"TCON" => report.genre = report.genre.take().or_else(|| present(text)),
            b"TALB" => report.album |= present(text).is_some(),
            _ => match frame.as_ref().and_then(|f| f.content().extended_text()) {
                Some(x) if x.description == PREP_DESCRIPTION => removed.push(i),
                Some(x) if x.description.eq_ignore_ascii_case("EnergyLevel") => report.energy = report.energy.take().or_else(|| present(&x.value)),
                _ => {}
            },
        }
    }
    report.existing_key = keys.iter().find_map(|(_, v)| present(v));
    report.existing_bpm = bpms.iter().find_map(|(_, v)| present(v));

    let mut added = Vec::new();
    for (name, id, value, found, existing) in [("key", "TKEY", key, &keys, &report.existing_key), ("bpm", "TBPM", bpm, &bpms, &report.existing_bpm)] {
        if let (Some(value), None) = (value, existing) {
            // Blank frames give way to the new one.
            removed.extend(found.iter().map(|&(i, _)| i));
            added.push(Frame::text(id, value).set_encoding(Some(Encoding::Latin1)));
            report.filled.push(name);
        }
    }
    added.push(Frame::with_content(
        "TXXX",
        Content::ExtendedText(ExtendedText { description: PREP_DESCRIPTION.into(), value: fill.prep_note.into() }),
    ));

    let mut body = Vec::with_capacity(old.size.max(old.body.len()).saturating_add(256));
    for (_, range) in old.frames.iter().enumerate().filter(|(i, _)| !removed.contains(i)) {
        body.extend_from_slice(old.body.get(range.clone()).unwrap_or_default());
    }
    body.extend(old.encode(added)?);
    // Keep the tag's size when the padding has room, else its padding (a footer allows none).
    let padding = old.body.len().saturating_sub(old.frames_end);
    let padding = if old.flags & FOOTER != 0 { 0 } else { old.size.checked_sub(body.len()).unwrap_or(padding) };
    body.resize(body.len().saturating_add(padding), 0);

    let size = syncsafe_bytes(body.len()).ok_or_else(|| TagError::Unsupported("the new tag would be over 256 MiB".into()))?;
    let head = [b'I', b'D', b'3', old.major, old.revision, old.flags];
    let mut out = Vec::with_capacity(bytes.len().saturating_add(body.len()));
    out.extend_from_slice(&head);
    out.extend_from_slice(&size);
    out.extend(body);
    if old.flags & FOOTER != 0 {
        out.extend_from_slice(b"3DI");
        out.extend_from_slice(head.get(3..).unwrap_or_default());
        out.extend_from_slice(&size);
    }
    out.extend_from_slice(bytes.get(old.rest_at..).unwrap_or_default());
    Ok((out, report))
}

const UNSYNC: u8 = 0x80;
const EXTENDED: u8 = 0x40;
const FOOTER: u8 = 0x10;

/// The leading tag, its frame area made plain (no unsynchronisation, no extended header).
struct OldTag {
    major: u8,
    revision: u8,
    /// Only the flags the version defines.
    flags: u8,
    /// The size its header gave.
    size: usize,
    body: Vec<u8>,
    /// Each frame's bytes in `body`, header included.
    frames: Vec<Range<usize>>,
    /// Where the frames stop; only zero padding follows.
    frames_end: usize,
    /// A v2.4 tag whose frame sizes are plain 32-bit numbers, as old iTunes wrote them.
    plain_sizes: bool,
    /// Where the bytes after the tag start.
    rest_at: usize,
}

impl OldTag {
    /// The tag a file without one gets.
    fn none() -> Self {
        OldTag { major: 3, revision: 0, flags: 0, size: 0, body: Vec::new(), frames: Vec::new(), frames_end: 0, plain_sizes: false, rest_at: 0 }
    }

    /// The tag at the start of `b`.
    fn parse(b: &[u8]) -> Result<Self, TagError> {
        let malformed = |what: &str| TagError::Malformed(what.into());
        let Some(&[_, _, _, major, revision, flags, ..]) = b.get(..10) else {
            return Err(malformed("the header is cut short"));
        };
        if !matches!(major, 3 | 4) {
            return Err(TagError::Unsupported(format!("ID3v2.{major}")));
        }
        // v2.3 defines unsynchronisation, extended header and experimental; v2.4 adds the footer.
        let mut flags = flags & if major == 3 { 0xE0 } else { 0xF0 };
        let size = syncsafe(b.get(6..10).unwrap_or_default());
        let rest_at = size.saturating_add(if flags & FOOTER != 0 { 20 } else { 10 });
        if rest_at > b.len() {
            return Err(malformed("the tag runs past the end of the file"));
        }
        if skip_id3v2(b.get(rest_at..).unwrap_or_default()) > 0 {
            return Err(TagError::Unsupported("more than one ID3v2 tag".into()));
        }
        let mut body = b.get(10..10 + size).unwrap_or_default().to_vec();
        if major == 3 && flags & UNSYNC != 0 {
            body = resync(&body);
            flags &= !UNSYNC;
        }
        if flags & EXTENDED != 0 {
            let n = body.get(..4).map(|s| if major == 3 { be_size(s).saturating_add(4) } else { syncsafe(s) });
            match n.filter(|&n| n <= body.len()) {
                Some(n) => drop(body.drain(..n)),
                None => return Err(malformed("the extended header runs past the end of the tag")),
            }
            flags &= !EXTENDED;
        }
        // v2.4 sizes are syncsafe, but old iTunes wrote plain ones: take whichever walks cleanly.
        let ways: &[bool] = if major == 3 { &[true] } else { &[false, true] };
        let Some((frames, frames_end, plain)) = ways.iter().find_map(|&plain| walk(&body, plain).map(|(f, end)| (f, end, plain))) else {
            return Err(malformed(
                "the frames do not walk to the end of the tag (an unknown frame ID, a frame that overruns it, or data in the padding)",
            ));
        };
        Ok(OldTag { major, revision, flags, size, body, frames, frames_end, plain_sizes: major == 4 && plain, rest_at })
    }

    /// `frames` encoded to sit among this tag's frames, without a tag header.
    fn encode(&self, frames: Vec<Frame>) -> Result<Vec<u8>, TagError> {
        let version = if self.major == 4 { Version::Id3v24 } else { Version::Id3v23 };
        let mut tag = Tag::with_version(version);
        frames.into_iter().for_each(|f| drop(tag.add_frame(f)));
        // v2.4 6.1: the tag-wide flag must not be set while any frame is not unsynchronised.
        let unsync = self.major == 4 && self.flags & UNSYNC != 0;
        let mut out = Vec::new();
        Encoder::new()
            .version(version)
            .unsynchronisation(unsync)
            .encode(&tag, &mut out)
            .map_err(|e| TagError::Unsupported(format!("encoding the new frames: {e}")))?;
        let mut out = out.get(10..).unwrap_or_default().to_vec();
        let mut at = 0usize;
        while let Some(head) = out.get_mut(at..at.saturating_add(10)) {
            let len =
                if version == Version::Id3v24 { syncsafe(head.get(4..8).unwrap_or_default()) } else { be_size(head.get(4..8).unwrap_or_default()) };
            if self.plain_sizes
                && let (Some(s), Ok(n)) = (head.get_mut(4..8), u32::try_from(len))
            {
                s.copy_from_slice(&n.to_be_bytes());
            }
            at = at.saturating_add(10).saturating_add(len);
        }
        Ok(out)
    }
}

/// The frames of `body` and where they stop, if they walk to its end or to zero padding.
/// `plain`: sizes are plain 32-bit numbers rather than syncsafe ones.
fn walk(body: &[u8], plain: bool) -> Option<(Vec<Range<usize>>, usize)> {
    let mut frames = Vec::new();
    let mut at = 0usize;
    while let Some(&[a, b, c, d, s0, s1, s2, s3, _, _]) = body.get(at..at.saturating_add(10))
        && [a, b, c, d].iter().all(|x| x.is_ascii_uppercase() || x.is_ascii_digit())
    {
        let s = [s0, s1, s2, s3];
        if !plain && s.iter().any(|x| x & 0x80 != 0) {
            return None;
        }
        let next = at.saturating_add(10).saturating_add(if plain { be_size(&s) } else { syncsafe(&s) });
        if next > body.len() {
            return None;
        }
        frames.push(at..next);
        at = next;
    }
    body.get(at..)?.iter().all(|&x| x == 0).then_some((frames, at))
}

/// Whether an undecodable TXXX frame might be a `djforge_prep` note: it is compressed or
/// encrypted, or the name appears in it in Latin-1/UTF-8 or UTF-16.
fn could_be_note(major: u8, raw: &[u8]) -> bool {
    let flags = raw.get(9).copied().unwrap_or(0xFF);
    let hidden = if major == 3 { flags & 0xC0 != 0 } else { flags & 0x0C != 0 };
    let name = PREP_DESCRIPTION.as_bytes();
    let le: Vec<u8> = name.iter().flat_map(|&c| [c, 0]).collect();
    let be: Vec<u8> = name.iter().flat_map(|&c| [0, c]).collect();
    hidden || [name, &le, &be].iter().any(|n| raw.windows(n.len()).any(|w| w == *n))
}

/// A plain big-endian size (v2.3 frames and extended headers).
fn be_size(b: &[u8]) -> usize {
    b.iter().fold(0usize, |n, &x| (n << 8) | usize::from(x))
}

fn syncsafe_bytes(n: usize) -> Option<[u8; 4]> {
    (n < 1 << 28).then_some([(n >> 21) as u8 & 0x7F, (n >> 14) as u8 & 0x7F, (n >> 7) as u8 & 0x7F, n as u8 & 0x7F])
}

/// Undoes unsynchronisation: every FF 00 becomes FF.
fn resync(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut after_ff = false;
    for &x in b {
        if !(after_ff && x == 0) {
            out.push(x);
        }
        after_ff = x == 0xFF;
    }
    out
}

/// `1A`–`12B`, as Mixed In Key writes it.
fn is_camelot(k: &str) -> bool {
    let n = k.strip_suffix(['A', 'B']).unwrap_or("");
    !n.starts_with('0') && n.bytes().all(|b| b.is_ascii_digit()) && n.parse::<u8>().is_ok_and(|n| (1..=12).contains(&n))
}

/// `v`, unless it is blank.
fn present(v: &str) -> Option<String> {
    (!v.trim().is_empty()).then(|| v.to_owned())
}

/// One raw frame, decoded by `id3` on its own.
fn decode(major: u8, raw: &[u8]) -> Result<Frame, TagError> {
    let size = syncsafe_bytes(raw.len()).ok_or_else(|| TagError::Unsupported("a frame over 256 MiB".into()))?;
    let tag = [&[b'I', b'D', b'3', major, 0, 0][..], &size, raw].concat();
    let id = String::from_utf8_lossy(raw.get(..4).unwrap_or_default()).into_owned();
    match Tag::read_from2(Cursor::new(tag)) {
        Ok(t) => t.frames().next().cloned().ok_or_else(|| TagError::Malformed(format!("frame {id} is empty"))),
        Err(e) => Err(TagError::Malformed(format!("frame {id}: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp3gain::skip_id3v2;
    use id3::frame::{Chapter, Comment, EncapsulatedObject, ExtendedText, Picture, PictureType, TableOfContents};
    use id3::{Encoder, Frame, Tag, TagLike, Version};
    use proptest::prelude::*;
    use std::io::Cursor;

    const CBR128: &[u8] = include_bytes!("../tests/fixtures/mp3/cbr128_44k_stereo.mp3");
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
        0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x60, 0x00,
        0x02, 0x00, 0x00, 0x05, 0x00, 0x01, 0xE9, 0xFA, 0xDC, 0xD8, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// The fixture's audio, without the tag ffmpeg wrote.
    fn audio() -> &'static [u8] {
        &CBR128[skip_id3v2(CBR128)..]
    }

    fn prep(note: &str) -> TagFill<'_> {
        TagFill { camelot: Some("4B"), bpm: Some(145.2), prep_note: note }
    }

    /// A tag like the library's: Mixed In Key's GEOB, energy, key and comment, cover art,
    /// chapters, and a junk genre with no album.
    fn library_tag(key: Option<&str>) -> Tag {
        let mut t = Tag::with_version(Version::Id3v23);
        // 200 bytes of noise, including a false MP3 sync and an FF 00 that unsynchronisation must escape.
        let mut noise: Vec<u8> = (0..195u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        noise.extend([0xFF, 0xFB, 0x90, 0xFF, 0x00]);
        t.add_frame(EncapsulatedObject {
            mime_type: "application/octet-stream".into(),
            filename: String::new(),
            description: "CuePoints".into(),
            data: noise,
        });
        t.add_frame(Picture { mime_type: "image/png".into(), picture_type: PictureType::CoverFront, description: String::new(), data: PNG.to_vec() });
        t.add_frame(Chapter {
            element_id: "ch0".into(),
            start_time: 0,
            end_time: 30_000,
            start_offset: u32::MAX,
            end_offset: u32::MAX,
            frames: vec![Frame::text("TIT2", "Intro")],
        });
        t.add_frame(TableOfContents { element_id: "toc".into(), top_level: true, ordered: true, elements: vec!["ch0".into()], frames: Vec::new() });
        t.add_frame(ExtendedText { description: "EnergyLevel".into(), value: "7".into() });
        t.add_frame(ExtendedText { description: "purl".into(), value: "https://example.com/t".into() });
        if let Some(k) = key {
            t.set_text("TKEY", k);
        }
        t.set_text("TCON", "Music");
        t.add_frame(Comment { lang: "eng".into(), description: String::new(), text: "9A - Energy 7".into() });
        t
    }

    fn tagged(tag: &Tag, version: Version) -> Vec<u8> {
        let mut out = Vec::new();
        tag.write_to(&mut out, version).unwrap();
        out.extend(audio());
        out
    }

    fn read(b: &[u8]) -> Tag {
        Tag::read_from2(Cursor::new(b)).unwrap()
    }

    fn text<'a>(t: &'a Tag, id: &str) -> Option<&'a str> {
        t.get(id).and_then(|f| f.content().text())
    }

    fn note(t: &Tag) -> Option<&str> {
        t.extended_texts().find(|x| x.description == PREP_DESCRIPTION).map(|x| x.value.as_str())
    }

    fn without_note(t: &Tag) -> Vec<Frame> {
        t.frames().filter(|f| f.content().extended_text().is_none_or(|x| x.description != PREP_DESCRIPTION)).cloned().collect()
    }

    /// A v2.3 frame built by hand.
    fn raw_frame(id: &[u8; 4], flags: u16, body: &[u8]) -> Vec<u8> {
        [&id[..], &u32::try_from(body.len()).unwrap().to_be_bytes(), &flags.to_be_bytes(), body].concat()
    }

    /// An ID3v2 tag built by hand around `body` (frames, padding, extended header).
    fn raw_tag(major: u8, flags: u8, body: &[u8]) -> Vec<u8> {
        let n = body.len();
        let size = [(n >> 21) as u8 & 0x7F, (n >> 14) as u8 & 0x7F, (n >> 7) as u8 & 0x7F, n as u8 & 0x7F];
        [&b"ID3"[..], &[major, 0, flags], &size, body].concat()
    }

    #[test]
    fn every_frame_and_mixed_in_keys_key_are_kept() {
        let src = tagged(&library_tag(Some("9A")), Version::Id3v23);
        let (out, report) = fill(&src, &prep("gain -3.0 dB")).unwrap();
        let (before, after) = (read(&src), read(&out));

        assert_eq!(after.version(), Version::Id3v23);
        for f in before.frames() {
            assert!(after.frames().any(|g| g == f), "lost {f:?}");
        }
        assert_eq!(after.frames().count(), before.frames().count() + 2);
        assert_eq!((text(&after, "TKEY"), text(&after, "TBPM"), note(&after)), (Some("9A"), Some("145"), Some("gain -3.0 dB")));

        // The old frames are untouched, byte for byte, and come first.
        let frames = skip_id3v2(&src);
        assert_eq!(out.get(10..frames), src.get(10..frames));
        assert_eq!(&out[skip_id3v2(&out)..], audio());

        assert_eq!(
            report,
            TagReport {
                filled: vec!["bpm"],
                existing_key: Some("9A".into()),
                existing_bpm: None,
                energy: Some("7".into()),
                genre: Some("Music".into()),
                album: false,
                unreadable: Vec::new(),
            }
        );
    }

    #[test]
    fn a_missing_or_blank_key_is_filled() {
        for key in [None, Some(" ")] {
            let mut tag = library_tag(key);
            tag.set_text("TALB", "Album");
            let src = tagged(&tag, Version::Id3v23);
            let (out, report) = fill(&src, &prep("p")).unwrap();
            let after = read(&out);
            assert_eq!(report.filled, ["key", "bpm"], "{key:?}");
            assert_eq!((report.existing_key, report.album), (None, true));
            assert_eq!((text(&after, "TKEY"), text(&after, "TBPM")), (Some("4B"), Some("145")));
            assert_eq!(after.frames().filter(|f| f.id() == "TKEY").count(), 1);
            assert_eq!(&out[skip_id3v2(&out)..], audio());
        }
    }

    #[test]
    fn nothing_is_filled_without_a_value() {
        let src = tagged(&library_tag(None), Version::Id3v23);
        let (out, report) = fill(&src, &TagFill { camelot: None, bpm: None, prep_note: "p" }).unwrap();
        assert!(report.filled.is_empty());
        assert_eq!((text(&read(&out), "TKEY"), note(&read(&out))), (None, Some("p")));
    }

    #[test]
    fn a_file_without_a_tag_gets_a_v23_one() {
        let (out, report) = fill(audio(), &prep("p")).unwrap();
        let after = read(&out);
        assert_eq!(out.get(..4), Some(&b"ID3\x03"[..]));
        assert_eq!(after.version(), Version::Id3v23);
        assert_eq!((text(&after, "TKEY"), text(&after, "TBPM"), note(&after)), (Some("4B"), Some("145"), Some("p")));
        assert_eq!(after.frames().count(), 3);
        assert_eq!(&out[skip_id3v2(&out)..], audio());
        assert_eq!(report, TagReport { filled: vec!["key", "bpm"], ..TagReport::default() });
    }

    #[test]
    fn running_again_changes_only_the_prep_note() {
        let src = tagged(&library_tag(None), Version::Id3v23);
        let (once, _) = fill(&src, &prep("first")).unwrap();
        let (same, report) = fill(&once, &prep("first")).unwrap();
        assert_eq!(same, once);
        assert!(report.filled.is_empty());
        assert_eq!(report.existing_key.as_deref(), Some("4B"));

        let (twice, _) = fill(&once, &prep("second run")).unwrap();
        assert_eq!(without_note(&read(&twice)), without_note(&read(&once)));
        assert_eq!(note(&read(&twice)), Some("second run"));
        assert_eq!(read(&twice).extended_texts().filter(|x| x.description == PREP_DESCRIPTION).count(), 1);
        assert_eq!(&twice[skip_id3v2(&twice)..], audio());
    }

    #[test]
    fn a_v24_tag_stays_v24_with_its_footer() {
        let mut src = Vec::new();
        library_tag(Some("9A")).write_to(&mut src, Version::Id3v24).unwrap();
        src[5] |= 0x10;
        let footer = [&b"3DI"[..], &src[3..10]].concat();
        src.extend(footer);
        src.extend(audio());

        let (out, _) = fill(&src, &prep("gain −3.0 dB; −10.9 → −13.9 LUFS")).unwrap();
        let after = read(&out);
        assert_eq!((out[3], out[5]), (4, 0x10));
        assert_eq!(after.version(), Version::Id3v24);
        assert_eq!((text(&after, "TKEY"), text(&after, "TBPM")), (Some("9A"), Some("145")));
        assert_eq!(note(&after), Some("gain −3.0 dB; −10.9 → −13.9 LUFS"));
        let end = skip_id3v2(&out);
        assert_eq!(out.get(end - 10..end - 7), Some(&b"3DI"[..]));
        assert_eq!(out.get(end - 7..end), out.get(3..10));
        assert_eq!(&out[end..], audio());
    }

    /// Decoding a whole tag with `id3` and encoding it again would change these frames: it keeps
    /// one of two TXXX frames with the same description, cuts a v2.3 text frame at its first
    /// null, re-encodes Latin-1 text as UTF-16 and drops frame flags. `fill` copies them as they are.
    #[test]
    fn frames_id3_would_rewrite_survive_byte_for_byte() {
        let frames = [
            raw_frame(b"TIT2", 0, b"\x00Caf\xE9"),
            raw_frame(b"TCON", 0, b"\x00House\x00Techno"),
            raw_frame(b"TXXX", 0, b"\x00purl\x00https://a.example/1"),
            raw_frame(b"TXXX", 0, b"\x00purl\x00https://a.example/2"),
            raw_frame(b"TCOM", 0x2000, b"\x00Someone"),
        ]
        .concat();
        let src = [raw_tag(3, 0, &[&frames[..], &[0; 64]].concat()), audio().to_vec()].concat();

        let mut again = Vec::new();
        read(&src).write_to(&mut again, Version::Id3v23).unwrap();
        assert_ne!(again.get(10..10 + frames.len()), Some(&frames[..]));
        assert_eq!(read(&src).extended_texts().count(), 1);
        assert_eq!(text(&read(&src), "TCON"), Some("House"));

        let (out, report) = fill(&src, &prep("p")).unwrap();
        assert_eq!(out.get(10..10 + frames.len()), Some(&frames[..]));
        assert_eq!(report.genre.as_deref(), Some("House"));
        assert_eq!((text(&read(&out), "TKEY"), note(&read(&out))), (Some("4B"), Some("p")));
        assert_eq!(&out[skip_id3v2(&out)..], audio());
    }

    #[test]
    fn unsynchronisation_and_the_extended_header_are_undone() {
        let tag = library_tag(Some("9A"));
        let mut unsynced = Vec::new();
        Encoder::new().version(Version::Id3v23).unsynchronisation(true).encode(&tag, &mut unsynced).unwrap();
        assert!(unsynced.windows(2).any(|w| w == [0xFF, 0x00]), "the GEOB noise needs unsynchronising");
        // `id3` reads an unsynchronised v2.3 tag past its end when audio follows, so the reference
        // is read from the tag alone.
        let before = read(&unsynced);
        assert!(Tag::read_from2(Cursor::new([&unsynced[..], audio()].concat())).is_err());
        let src = [unsynced, audio().to_vec()].concat();
        let (out, _) = fill(&src, &prep("p")).unwrap();
        assert_eq!(out[5], 0);
        assert!(before.frames().all(|f| read(&out).frames().any(|g| g == f)));
        assert_eq!(read(&out).frames().count(), before.frames().count() + 2);
        assert_eq!(&out[skip_id3v2(&out)..], audio());

        let frames = raw_frame(b"TKEY", 0, b"\x009A");
        let ext = [0, 0, 0, 6, 0, 0, 0, 0, 0, 0];
        let src = [raw_tag(3, 0x40, &[&ext[..], &frames].concat()), audio().to_vec()].concat();
        let (out, report) = fill(&src, &prep("p")).unwrap();
        assert_eq!(out[5], 0);
        assert_eq!(out.get(10..10 + frames.len()), Some(&frames[..]));
        assert_eq!(report.existing_key.as_deref(), Some("9A"));
        assert_eq!(&out[skip_id3v2(&out)..], audio());
    }

    #[test]
    fn bad_values_are_refused() {
        let src = tagged(&library_tag(None), Version::Id3v23);
        for bpm in [f64::NAN, f64::INFINITY, 19.0, 400.6, -120.0] {
            assert!(matches!(fill(&src, &TagFill { bpm: Some(bpm), ..prep("p") }), Err(TagError::Bpm(_))), "{bpm}");
        }
        for key in ["", "0A", "13B", "9C", "9a", "A", "12AB"] {
            assert!(matches!(fill(&src, &TagFill { camelot: Some(key), ..prep("p") }), Err(TagError::Key(_))), "{key}");
        }
        for (bpm, key) in [(20.0, "1A"), (399.4, "12B")] {
            assert!(fill(&src, &TagFill { camelot: Some(key), bpm: Some(bpm), prep_note: "p" }).is_ok());
        }
    }

    #[test]
    fn tags_it_cannot_rewrite_safely_are_refused() {
        let frame = raw_frame(b"TKEY", 0, b"\x009A");
        let too_short = [&raw_tag(3, 0, &frame)[..frame.len() + 5]].concat();
        let frame_past_end = raw_tag(3, 0, &[&frame[..], b"TBPM\x00\x00\x01\x00\x00\x00\x00145"].concat());
        let v22 = [raw_tag(2, 0, b"TT2\x00\x00\x04\x00abc"), audio().to_vec()].concat();
        let two = [raw_tag(3, 0, &frame), raw_tag(3, 0, &frame), audio().to_vec()].concat();
        assert!(matches!(fill(&too_short, &prep("p")), Err(TagError::Malformed(_))));
        assert!(matches!(fill(&frame_past_end, &prep("p")), Err(TagError::Malformed(_))));
        assert!(matches!(fill(&v22, &prep("p")), Err(TagError::Unsupported(_))));
        assert!(matches!(fill(&two, &prep("p")), Err(TagError::Unsupported(_))));
    }

    /// A v2.4 frame, with a syncsafe size or, as iTunes once wrote them, a plain one.
    fn v24_frame(id: &[u8; 4], plain: bool, flags: u16, body: &[u8]) -> Vec<u8> {
        let n = body.len();
        let size = if plain { u32::try_from(n).unwrap().to_be_bytes() } else { syncsafe_bytes(n).unwrap() };
        [&id[..], &size, &flags.to_be_bytes(), body].concat()
    }

    /// The IDs of the frames in `b`'s tag, as `fill` walks them.
    fn walked(b: &[u8]) -> Vec<String> {
        let t = OldTag::parse(b).unwrap();
        t.frames.iter().map(|r| String::from_utf8_lossy(&t.body[r.start..r.start + 4]).into_owned()).collect()
    }

    #[test]
    fn a_v24_tag_with_plain_frame_sizes_is_walked_as_itunes_wrote_it() {
        let long_note = "gain −3.0 dB; −10.9 → −13.9 LUFS; TP −2.7 → −5.7 dBTP; target −14; and enough words to pass 127 bytes";
        for noise in [false, true] {
            let mut pic = b"\x00image/png\x00\x03\x00".to_vec();
            pic.extend((0..300u32).map(|i| if noise { (i.wrapping_mul(2_654_435_761) >> 24) as u8 } else { b'A' + (i % 26) as u8 }));
            let frames = [v24_frame(b"APIC", true, 0, &pic), v24_frame(b"TKEY", true, 0, b"\x009A")].concat();
            let src = [raw_tag(4, 0, &[&frames[..], &[0; 32]].concat()), audio().to_vec()].concat();
            let (out, report) = fill(&src, &prep(long_note)).unwrap();
            assert_eq!((report.existing_key.as_deref(), report.filled.clone()), (Some("9A"), vec!["bpm"]), "noise {noise}");
            assert_eq!(out.get(10..10 + frames.len()), Some(&frames[..]));
            // The new frames' sizes are plain too, so the whole tag walks one way.
            assert_eq!(walked(&out), ["APIC", "TKEY", "TBPM", "TXXX"]);
            assert_eq!(&out[skip_id3v2(&out)..], audio());
        }
    }

    #[test]
    fn frames_the_walk_cannot_read_to_the_end_refuse_the_file() {
        let after_odd_id = [raw_frame(b"TIT2", 0, b"\x00x"), raw_frame(b"Xabc", 0, b"zz"), raw_frame(b"TKEY", 0, b"\x009A")].concat();
        let junk_padding = [raw_frame(b"TIT2", 0, b"\x00x"), vec![0, 0, 7, 0]].concat();
        let neither = [v24_frame(b"TIT2", true, 0, &[b'x'; 200]), b"junk".to_vec()].concat();
        for (major, frames) in [(3, after_odd_id), (3, junk_padding), (4, neither)] {
            let src = [raw_tag(major, 0, &[&frames[..], &[0; 16]].concat()), audio().to_vec()].concat();
            assert!(matches!(fill(&src, &prep("p")), Err(TagError::Malformed(_))), "{frames:02X?}");
        }
    }

    #[test]
    fn blank_and_unreadable_frames() {
        let cases: [(&str, Vec<u8>, bool, &[&str]); 7] = [
            ("empty TKEY", raw_frame(b"TKEY", 0, b""), true, &[]),
            ("TKEY with only its encoding", raw_frame(b"TKEY", 0, b"\x00"), true, &[]),
            ("empty TBPM", raw_frame(b"TBPM", 0, b""), true, &[]),
            ("TXXX in encoding 7", raw_frame(b"TXXX", 0, b"\x07abc\x00def"), false, &["TXXX"]),
            ("empty TALB", raw_frame(b"TALB", 0, b""), false, &["TALB"]),
            ("TCON in encoding 9", raw_frame(b"TCON", 0, b"\x09House"), false, &["TCON"]),
            ("TCON with only its encoding", raw_frame(b"TCON", 0, b"\x00"), false, &[]),
        ];
        for (name, frame, replaced, unreadable) in cases {
            let src = [raw_tag(3, 0, &[&frame[..], &[0; 16]].concat()), audio().to_vec()].concat();
            let (out, report) = fill(&src, &prep("p")).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(report.filled, ["key", "bpm"], "{name}");
            assert_eq!(report.unreadable, unreadable, "{name}");
            assert_eq!(out.get(10..10 + frame.len()) == Some(&frame[..]), !replaced, "{name}");
            let ids = walked(&out);
            assert_eq!(ids.iter().filter(|i| *i == "TKEY").count(), 1, "{name}");
            assert_eq!(ids.iter().filter(|i| *i == "TBPM").count(), 1, "{name}");
        }
    }

    #[test]
    fn an_unreadable_txxx_that_could_be_a_prep_note_refuses_the_file() {
        for frame in [
            raw_frame(b"TXXX", 0x0040, b"\x01abc"),
            raw_frame(b"TXXX", 0x0080, b"\x00\x00\x00\x10junk"),
            raw_frame(b"TXXX", 0, b"\x07djforge_prep\x00x"),
            raw_frame(b"TXXX", 0, b"\x09d\x00j\x00f\x00o\x00r\x00g\x00e\x00_\x00p\x00r\x00e\x00p\x00\x00\x00x"),
        ] {
            let src = [raw_tag(3, 0, &frame), audio().to_vec()].concat();
            assert!(matches!(fill(&src, &prep("p")), Err(TagError::Malformed(_))), "{frame:02X?}");
        }
    }

    #[test]
    fn only_the_flags_a_version_defines_are_kept() {
        // v2.3 defines no footer: the ten bytes 0x10 would claim stay where they are.
        let ten = [0x55u8; 10];
        let src = [raw_tag(3, 0x3F, &raw_frame(b"TKEY", 0, b"\x009A")), ten.to_vec(), audio().to_vec()].concat();
        let (out, _) = fill(&src, &prep("p")).unwrap();
        assert_eq!(out[5], 0x20);
        assert_eq!(&out[skip_id3v2(&out)..], &[&ten[..], audio()].concat()[..]);

        let src = [raw_tag(4, 0x2F, &v24_frame(b"TKEY", false, 0, b"\x009A")), audio().to_vec()].concat();
        let (out, _) = fill(&src, &prep("p")).unwrap();
        assert_eq!(out[5], 0x20);
        assert_eq!(&out[skip_id3v2(&out)..], audio());
    }

    #[test]
    fn new_frames_in_an_unsynchronised_v24_tag_are_unsynchronised() {
        let mut unsynced = Vec::new();
        Encoder::new().version(Version::Id3v24).unsynchronisation(true).encode(&library_tag(Some("9A")), &mut unsynced).unwrap();
        assert_eq!(unsynced[5] & 0x80, 0x80);
        let src = [unsynced, audio().to_vec()].concat();
        let (out, report) = fill(&src, &TagFill { camelot: None, bpm: Some(128.0), prep_note: "gain −3.0 dB" }).unwrap();
        assert_eq!((out[5] & 0x80, report.filled), (0x80, vec!["bpm"]));
        let tag = OldTag::parse(&out).unwrap();
        for r in &tag.frames {
            assert_eq!(tag.body[r.start + 9] & 0x02, 0x02, "{:?}", String::from_utf8_lossy(&tag.body[r.start..r.start + 4]));
        }
        let after = read(&out);
        assert_eq!((text(&after, "TKEY"), text(&after, "TBPM"), note(&after)), (Some("9A"), Some("128"), Some("gain −3.0 dB")));
    }

    #[test]
    fn new_frames_take_their_room_from_the_padding() {
        let src = [raw_tag(3, 0, &[&raw_frame(b"TKEY", 0, b"\x009A")[..], &[0; 400]].concat()), audio().to_vec()].concat();
        let (out, _) = fill(&src, &prep("p")).unwrap();
        assert_eq!(skip_id3v2(&out), skip_id3v2(&src));
        assert_eq!(walked(&out), ["TKEY", "TBPM", "TXXX"]);
        assert_eq!(&out[skip_id3v2(&out)..], audio());

        let (again, _) = fill(&out, &prep("a longer note this time")).unwrap();
        assert_eq!(skip_id3v2(&again), skip_id3v2(&src));
    }

    proptest! {
        #[test]
        fn any_bytes_after_a_tag_header_never_panic(major in 2u8..6, flags: u8, body in proptest::collection::vec(any::<u8>(), 0..300)) {
            let src = [raw_tag(major, flags, &body), body.clone()].concat();
            let _ = fill(&src, &prep("p"));
            let _ = fill(&body, &prep("p"));
        }

        #[test]
        fn any_content_in_a_frame_that_is_read_never_panics(
            major in 3u8..5,
            id in prop::sample::select(vec![*b"TKEY", *b"TBPM", *b"TCON", *b"TALB", *b"TXXX"]),
            flags: u16,
            content in proptest::collection::vec(any::<u8>(), 0..64),
        ) {
            let src = raw_tag(major, 0, &raw_frame(&id, flags, &content));
            let _ = fill(&src, &prep("p"));
        }
    }
}
