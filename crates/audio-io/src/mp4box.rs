//! MP4 boxes for NI STEM files: listing the top-level boxes and those in `moov`, and putting
//! the `stem` metadata box at `moov/udta/stem`.
//!
//! Only files whose sizes are all 32-bit and whose `moov` comes after `mdat` are changed: then
//! growing `moov` moves no audio, so the chunk offsets in it stay right.

use crate::pcm::be_u32;

/// A box of an MP4 file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mp4Box {
    /// Its path from the top: `moov`, `moov/udta`.
    pub path: String,
    /// Where it starts in the file.
    pub start: usize,
    /// Its size, header included.
    pub size: usize,
}

impl Mp4Box {
    /// Its payload's range in the file.
    pub fn body(&self) -> std::ops::Range<usize> {
        self.start.saturating_add(8)..self.start.saturating_add(self.size)
    }
}

/// The boxes in `b[from..to]`, one level: each with a 32-bit size inside the range. A 64-bit
/// size (1), a box running to the end of the file (0) or one past the range is refused.
fn level(b: &[u8], from: usize, to: usize, parent: &str) -> Result<Vec<Mp4Box>, String> {
    let mut out = Vec::new();
    let mut pos = from;
    while pos < to {
        let size = be_u32(b, pos).ok_or_else(|| format!("a box header at byte {pos} runs past the file"))?;
        let kind = b.get(pos + 4..pos + 8).ok_or_else(|| format!("a box header at byte {pos} runs past the file"))?;
        let name: String = kind.iter().map(|&c| char::from(c)).collect();
        let size = usize::try_from(size).map_err(|_| "a box size does not fit".to_string())?;
        match size {
            1 => return Err(format!("box `{name}` has a 64-bit size, which is not supported")),
            0 => return Err(format!("box `{name}` runs to the end of the file, which is not supported")),
            s if s < 8 || pos.checked_add(s).is_none_or(|e| e > to) => return Err(format!("box `{name}` at byte {pos} has a bad size ({s})")),
            _ => {}
        }
        let path = if parent.is_empty() { name } else { format!("{parent}/{name}") };
        out.push(Mp4Box { path, start: pos, size });
        pos += size;
    }
    Ok(out)
}

/// The top-level boxes of MP4 file `b`, each followed by its children when it is `moov`, and
/// theirs when that is `udta`.
pub fn walk(b: &[u8]) -> Result<Vec<Mp4Box>, String> {
    let mut out = Vec::new();
    for top in level(b, 0, b.len(), "")? {
        let children = if top.path == "moov" { level(b, top.body().start, top.body().end, "moov")? } else { Vec::new() };
        out.push(top);
        for c in children {
            let grand = if c.path == "moov/udta" { level(b, c.body().start, c.body().end, "moov/udta")? } else { Vec::new() };
            out.push(c);
            out.extend(grand);
        }
    }
    Ok(out)
}

/// The payload of `moov/udta/stem` in MP4 file `b`, when it has one.
pub fn stem_payload(b: &[u8]) -> Option<&[u8]> {
    let found = walk(b).ok()?.into_iter().find(|x| x.path == "moov/udta/stem")?;
    b.get(found.body())
}

/// `size` as a big-endian 32-bit box size.
fn size32(size: usize, what: &str) -> Result<[u8; 4], String> {
    u32::try_from(size).map(u32::to_be_bytes).map_err(|_| format!("{what} would be larger than 4 GB"))
}

/// MP4 file `b` with a `stem` box holding `json` at the end of `moov/udta` (made when missing),
/// replacing a `stem` box there already; the `udta` and `moov` sizes fixed. Refused unless
/// every size is 32-bit and `moov` comes after `mdat`.
pub fn insert_stem_box(b: &[u8], json: &[u8]) -> Result<Vec<u8>, String> {
    let boxes = walk(b)?;
    for (path, what) in [("moov", "`moov` boxes"), ("moov/udta", "`udta` boxes in `moov`")] {
        if boxes.iter().filter(|x| x.path == path).count() > 1 {
            return Err(format!("the file has more than one of its {what}"));
        }
    }
    let moov = boxes.iter().find(|x| x.path == "moov").ok_or("the file has no `moov` box")?;
    let mdat = boxes.iter().find(|x| x.path == "mdat").ok_or("the file has no `mdat` box")?;
    if moov.start < mdat.start {
        return Err("`moov` comes before `mdat`: adding to it would move the audio".into());
    }
    let udta = boxes.iter().find(|x| x.path == "moov/udta");
    let old = boxes.iter().find(|x| x.path == "moov/udta/stem");
    let stem_size = json.len().checked_add(8).ok_or("the stem metadata is too large")?;
    let mut stem = Vec::with_capacity(stem_size);
    stem.extend_from_slice(&size32(stem_size, "the stem box")?);
    stem.extend_from_slice(b"stem");
    stem.extend_from_slice(json);

    // The bytes removed (an old stem box) and where the new box goes.
    let removed = old.map_or(0..0, |o| o.start..o.start + o.size);
    let (insert_at, new_udta) = match udta {
        Some(u) => (u.start + u.size, None),
        None => (moov.start + moov.size, Some(())),
    };
    let added = stem.len() + if new_udta.is_some() { 8 } else { 0 };
    let grow =
        |size: usize, what: &str| size.checked_add(added).and_then(|s| s.checked_sub(removed.len())).ok_or_else(|| format!("{what} size overflows"));
    let moov_size = size32(grow(moov.size, "moov")?, "moov")?;

    let part = |r: std::ops::Range<usize>| b.get(r).ok_or_else(|| "a box runs past the file".to_string());
    let mut out = Vec::with_capacity(b.len() - removed.len() + added);
    // Up to the end of udta (or moov), leaving out an old stem box (inside udta).
    if removed.is_empty() {
        out.extend_from_slice(part(0..insert_at)?);
    } else {
        out.extend_from_slice(part(0..removed.start)?);
        out.extend_from_slice(part(removed.end..insert_at)?);
    }
    match new_udta {
        Some(()) => {
            out.extend_from_slice(&size32(stem.len() + 8, "udta")?);
            out.extend_from_slice(b"udta");
            out.extend_from_slice(&stem);
        }
        None => out.extend_from_slice(&stem),
    }
    out.extend_from_slice(part(insert_at..b.len())?);

    // The sizes: moov is before the change; udta's start is before it too.
    let set = |out: &mut Vec<u8>, at: usize, size: [u8; 4]| -> Result<(), String> {
        out.get_mut(at..at + 4).ok_or("a box header runs past the file")?.copy_from_slice(&size);
        Ok(())
    };
    set(&mut out, moov.start, moov_size)?;
    if let Some(u) = udta {
        set(&mut out, u.start, size32(grow(u.size, "udta")?, "udta")?)?;
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    /// `ftyp`, `mdat`, then `moov` holding `mvhd` and `extra` (more boxes), then a `free`.
    fn file(extra: &[u8]) -> Vec<u8> {
        let mut moov_body = bx(b"mvhd", &[0; 100]);
        moov_body.extend_from_slice(extra);
        [bx(b"ftyp", b"isom\0\0\0\0"), bx(b"mdat", &[7; 50]), bx(b"moov", &moov_body), bx(b"free", &[0; 4])].concat()
    }

    fn paths(b: &[u8]) -> Vec<String> {
        walk(b).unwrap().into_iter().map(|x| x.path).collect()
    }

    #[test]
    fn a_stem_box_goes_into_a_new_udta_with_the_sizes_fixed() {
        let b = file(&[]);
        let out = insert_stem_box(&b, b"{\"version\":1}").unwrap();
        assert_eq!(paths(&out), ["ftyp", "mdat", "moov", "moov/mvhd", "moov/udta", "moov/udta/stem", "free"]);
        assert_eq!(stem_payload(&out), Some(&b"{\"version\":1}"[..]));
        // Everything before moov (the audio) is where it was.
        let moov = walk(&b).unwrap().into_iter().find(|x| x.path == "moov").unwrap();
        assert_eq!(out[..moov.start], b[..moov.start]);
        assert_eq!(out.len(), b.len() + 8 + 8 + 13);
        assert_eq!(&out[out.len() - 12..], &bx(b"free", &[0; 4])[..]);
    }

    #[test]
    fn an_existing_udta_keeps_its_boxes_and_an_old_stem_box_is_replaced() {
        let udta = bx(b"udta", &[bx(b"meta", &[1; 20]), bx(b"stem", b"old")].concat());
        let b = file(&udta);
        let out = insert_stem_box(&b, b"{\"new\":true}").unwrap();
        assert_eq!(paths(&out), ["ftyp", "mdat", "moov", "moov/mvhd", "moov/udta", "moov/udta/meta", "moov/udta/stem", "free"]);
        assert_eq!(stem_payload(&out), Some(&b"{\"new\":true}"[..]));
        let meta = walk(&out).unwrap().into_iter().find(|x| x.path == "moov/udta/meta").unwrap();
        assert_eq!(&out[meta.body()], &[1; 20]);
        // Again: still one stem box.
        let again = insert_stem_box(&out, b"{}").unwrap();
        assert_eq!(paths(&again).iter().filter(|p| p.ends_with("/stem")).count(), 1);
        assert_eq!(stem_payload(&again), Some(&b"{}"[..]));
    }

    #[test]
    fn a_second_moov_or_udta_is_refused() {
        let mut two = file(&[]);
        two.extend_from_slice(&bx(b"moov", &bx(b"mvhd", &[0; 8])));
        assert!(insert_stem_box(&two, b"{}").unwrap_err().contains("more than one of its `moov`"));
        let udtas = file(&[bx(b"udta", &[]), bx(b"udta", &[])].concat());
        assert!(insert_stem_box(&udtas, b"{}").unwrap_err().contains("`udta` boxes"));
    }

    #[test]
    fn moov_before_mdat_is_refused() {
        let b = [bx(b"ftyp", b"isom"), bx(b"moov", &bx(b"mvhd", &[0; 8])), bx(b"mdat", &[7; 50])].concat();
        assert!(insert_stem_box(&b, b"{}").unwrap_err().contains("before `mdat`"));
    }

    #[test]
    fn sizes_other_than_32_bit_are_refused() {
        let mut big = 1u32.to_be_bytes().to_vec();
        big.extend_from_slice(b"mdat");
        big.extend_from_slice(&24u64.to_be_bytes());
        big.extend_from_slice(&[7; 8]);
        let b = [bx(b"ftyp", b"isom"), big, bx(b"moov", &bx(b"mvhd", &[0; 8]))].concat();
        assert!(insert_stem_box(&b, b"{}").unwrap_err().contains("64-bit"));
        let mut to_end = file(&[]);
        to_end.extend_from_slice(&[0, 0, 0, 0, b'f', b'r', b'e', b'e']);
        assert!(walk(&to_end).unwrap_err().contains("end of the file"));
        // A box claiming more than the file holds.
        let mut short = file(&[]);
        short.truncate(short.len() - 2);
        assert!(insert_stem_box(&short, b"{}").is_err());
    }
}
