//! The Rekordbox XML of an exported set: Pioneer's documented `DJ_PLAYLISTS` 1.0.0 format, with a
//! collection of the set's tracks (a TEMPO per bar and the cues) and one playlist in set order.

use super::Cue;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// One track of the collection.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RbTrack {
    pub path: PathBuf,
    pub name: String,
    pub artist: String,
    pub album: String,
    /// File size in bytes.
    pub size: u64,
    /// Length in seconds.
    pub total_secs: f64,
    /// The key as Rekordbox shows it, e.g. "8A".
    pub tonality: String,
    /// File seconds of each bar's downbeat, increasing.
    pub grid: Vec<f64>,
    pub cues: Vec<Cue>,
}

/// Hot cue colours (red, green, blue) for A..D: green, yellow, red, orange.
const HOT_CUE_RGB: [(u8, u8, u8); 4] = [(40, 226, 20), (180, 190, 4), (230, 40, 40), (224, 100, 27)];

/// The Rekordbox XML of a playlist `set_name` holding `tracks` in order. A track whose path has
/// no `file://` URL ([`file_url`]) is left out, since Rekordbox couldn't find it.
pub(crate) fn rekordbox_xml(set_name: &str, tracks: &[RbTrack]) -> String {
    let listed: Vec<(&RbTrack, String)> = tracks.iter().filter_map(|t| file_url(&t.path).map(|u| (t, u))).collect();
    let mut x = String::new();
    x.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<DJ_PLAYLISTS Version=\"1.0.0\">\n");
    let _ = writeln!(x, "  <PRODUCT Name=\"djforge\" Version=\"{}\" Company=\"\"/>", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(x, "  <COLLECTION Entries=\"{}\">", listed.len());
    for (i, (t, url)) in listed.iter().enumerate() {
        let _ = writeln!(
            x,
            "    <TRACK TrackID=\"{}\" Name=\"{}\" Artist=\"{}\" Album=\"{}\" Kind=\"{}\" Size=\"{}\" TotalTime=\"{}\" AverageBpm=\"{:.2}\" Tonality=\"{}\" Location=\"{}\">",
            i + 1,
            escape(&t.name),
            escape(&t.artist),
            escape(&t.album),
            kind(&t.path),
            t.size,
            whole_secs(t.total_secs),
            average_bpm(&t.grid),
            escape(&t.tonality),
            escape(url),
        );
        for (start, bpm) in tempos(&t.grid) {
            let _ = writeln!(x, "      <TEMPO Inizio=\"{start:.3}\" Bpm=\"{bpm:.2}\" Metro=\"4/4\" Battito=\"1\"/>");
        }
        for c in t.cues.iter().filter(|c| c.at.is_finite() && c.at >= 0.0) {
            let name = escape(c.name);
            match HOT_CUE_RGB.get(usize::from(c.slot)) {
                Some((r, g, b)) if !c.memory => {
                    let _ = writeln!(
                        x,
                        "      <POSITION_MARK Name=\"{name}\" Type=\"0\" Start=\"{:.3}\" Num=\"{}\" Red=\"{r}\" Green=\"{g}\" Blue=\"{b}\"/>",
                        c.at, c.slot
                    );
                }
                _ => {
                    let _ = writeln!(x, "      <POSITION_MARK Name=\"{name}\" Type=\"0\" Start=\"{:.3}\" Num=\"-1\"/>", c.at);
                }
            }
        }
        x.push_str("    </TRACK>\n");
    }
    x.push_str("  </COLLECTION>\n  <PLAYLISTS>\n    <NODE Type=\"0\" Name=\"ROOT\" Count=\"1\">\n");
    let _ = writeln!(x, "      <NODE Name=\"{}\" Type=\"1\" KeyType=\"0\" Entries=\"{}\">", escape(set_name), listed.len());
    for i in 0..listed.len() {
        let _ = writeln!(x, "        <TRACK Key=\"{}\"/>", i + 1);
    }
    x.push_str("      </NODE>\n    </NODE>\n  </PLAYLISTS>\n</DJ_PLAYLISTS>\n");
    x
}

/// `s` for an XML attribute value: `& < > " '` escaped, and characters XML can't hold (control
/// characters, U+FFFE, U+FFFF) removed.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if c < ' ' || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// Whole seconds of a length (0 for a non-finite or negative one).
fn whole_secs(secs: f64) -> u64 {
    if secs.is_finite() && secs > 0.0 { secs.round().min(u64::MAX as f64) as u64 } else { 0 }
}

/// Rekordbox's file kind from the extension.
fn kind(path: &Path) -> String {
    let ext = path.extension().and_then(|x| x.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    match ext.as_str() {
        "mp3" => "MP3 File".into(),
        "wav" => "WAV File".into(),
        "flac" => "FLAC File".into(),
        "aif" | "aiff" => "AIFF File".into(),
        "m4a" | "mp4" | "aac" => "M4A File".into(),
        "ogg" => "OGG File".into(),
        "" => "Audio File".into(),
        other => format!("{} File", escape(&other.to_ascii_uppercase())),
    }
}

/// The bar lengths (seconds) of `grid`, keeping only positive, finite ones.
fn bar_lengths(grid: &[f64]) -> impl Iterator<Item = f64> + '_ {
    grid.windows(2).filter_map(|w| match w {
        [a, b] => Some(b - a).filter(|d| d.is_finite() && *d > 0.0),
        _ => None,
    })
}

/// The average tempo (quarter-note bpm) of `grid`: 240 over its median bar length; 0 without a
/// whole bar.
fn average_bpm(grid: &[f64]) -> f64 {
    let mut bars: Vec<f64> = bar_lengths(grid).collect();
    bars.sort_by(f64::total_cmp);
    let n = bars.len();
    let median = match (bars.get(n.wrapping_sub(1) / 2), bars.get(n / 2)) {
        (Some(a), Some(b)) if n > 0 => (a + b) / 2.0,
        _ => return 0.0,
    };
    240.0 / median
}

/// One (start, bpm) per bar of `grid`: each bar's tempo from its length, the last bar the one
/// before's. Empty without a whole bar.
fn tempos(grid: &[f64]) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = grid.windows(2).filter_map(|w| Some((*w.first()?, 240.0 / (w.get(1)? - w.first()?)))).collect();
    if let (Some(&last), Some(&(_, bpm))) = (grid.last(), out.last()) {
        out.push((last, bpm));
    }
    out.retain(|(s, b)| s.is_finite() && *s >= 0.0 && b.is_finite() && *b > 0.0);
    out
}

/// `path` as a Rekordbox `Location` URL: `file://localhost/C:/...` for a drive path,
/// `file://server/share/...` for a UNC one (both also in their `\\?\` forms), and
/// `file://localhost/...` for an absolute Unix path; every byte of the UTF-8 path but unreserved
/// characters and the separators is percent-encoded. `None` for a relative path, a device path,
/// or one that isn't UTF-8.
pub(crate) fn file_url(path: &Path) -> Option<String> {
    let p = path.to_str()?.replace('\\', "/");
    let (host, drive, rest) =
        if let Some(unc) = p.strip_prefix("//?/UNC/").or_else(|| p.strip_prefix("//").filter(|r| !r.starts_with("?/") && !r.starts_with("./"))) {
            let (server, rest) = unc.split_once('/')?;
            (encode(server), String::new(), rest.to_string())
        } else {
            let local = p.strip_prefix("//?/").unwrap_or(&p);
            match local.as_bytes() {
                [d, b':', b'/', ..] if d.is_ascii_alphabetic() => {
                    ("localhost".to_string(), format!("{}:/", char::from(*d).to_ascii_uppercase()), local.get(3..)?.to_string())
                }
                [b'/', ..] if !p.starts_with("//") => ("localhost".to_string(), String::new(), local.get(1..)?.to_string()),
                _ => return None,
            }
        };
    if host.is_empty() || rest.is_empty() {
        return None;
    }
    Some(format!("file://{host}/{drive}{}", encode(&rest)))
}

/// `s` with every byte but unreserved characters (RFC 3986) and `/` percent-encoded.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use quick_xml::XmlVersion;
    use quick_xml::events::{BytesStart, Event};

    /// An element of the parsed XML.
    #[derive(Debug)]
    struct El {
        name: String,
        attrs: Vec<(String, String)>,
        kids: Vec<El>,
    }

    impl El {
        fn attr(&self, key: &str) -> &str {
            self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).unwrap_or_else(|| panic!("{} has no {key}", self.name))
        }

        fn kids(&self, name: &str) -> Vec<&El> {
            self.kids.iter().filter(|k| k.name == name).collect()
        }

        fn kid(&self, name: &str) -> &El {
            self.kids(name).into_iter().next().unwrap_or_else(|| panic!("{} has no {name}", self.name))
        }
    }

    fn element(e: &BytesStart) -> El {
        let name = String::from_utf8(e.name().as_ref().to_vec()).unwrap();
        let attrs = e
            .attributes()
            .map(|a| {
                let a = a.unwrap();
                (String::from_utf8(a.key.as_ref().to_vec()).unwrap(), a.normalized_value(XmlVersion::Explicit1_0).unwrap().into_owned())
            })
            .collect();
        El { name, attrs, kids: Vec::new() }
    }

    /// The root element of `xml`, parsed strictly (well-formed, one root).
    fn parse(xml: &str) -> El {
        let mut r = quick_xml::Reader::from_str(xml);
        r.config_mut().check_end_names = true;
        let mut stack: Vec<El> = vec![El { name: String::new(), attrs: Vec::new(), kids: Vec::new() }];
        loop {
            match r.read_event().unwrap() {
                Event::Start(e) => stack.push(element(&e)),
                Event::Empty(e) => stack.last_mut().unwrap().kids.push(element(&e)),
                Event::End(_) => {
                    let done = stack.pop().unwrap();
                    stack.last_mut().unwrap().kids.push(done);
                }
                Event::Eof => break,
                Event::Text(t) => assert!(t.iter().all(u8::is_ascii_whitespace), "stray text {t:?}"),
                Event::Decl(_) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        let mut top = stack.pop().unwrap();
        assert!(stack.is_empty() && top.kids.len() == 1, "one root element");
        top.kids.pop().unwrap()
    }

    fn cue(name: &'static str, slot: u8, at: f64, memory: bool) -> Cue {
        Cue { name, slot, at, memory }
    }

    /// Two tracks: one with a 128 bpm grid that speeds up for its last two bars, all four hot
    /// cues and two memory cues; one with an awkward name and no cues.
    fn tracks() -> Vec<RbTrack> {
        let mut grid: Vec<f64> = (0..8).map(|k| 0.25 + k as f64 * 1.875).collect();
        grid.push(grid[7] + 1.8);
        grid.push(grid[8] + 1.8);
        vec![
            RbTrack {
                path: PathBuf::from(r"C:\Music\Set\a b.mp3"),
                name: "A".into(),
                artist: "Artist".into(),
                album: String::new(),
                size: 1_234_567,
                total_secs: 312.6,
                tonality: "8A".into(),
                grid,
                cues: vec![
                    cue("Mix in", 0, 0.25, false),
                    cue("Mix in", 0, 0.25, true),
                    cue("Bass hit", 1, 7.75, false),
                    cue("Mix out", 2, 11.5, false),
                    cue("Mix out", 2, 11.5, true),
                    cue("Bass gone", 3, 13.375, false),
                ],
            },
            RbTrack {
                path: PathBuf::from(r"C:\Music\Set\b.flac"),
                name: "Tom & \"Jerry\" <live> it's\u{1}\t\n".into(),
                artist: String::new(),
                album: "Al'bum".into(),
                size: 10,
                total_secs: 1.0,
                tonality: String::new(),
                grid: vec![0.0, 2.0, 4.0],
                cues: Vec::new(),
            },
        ]
    }

    #[test]
    fn the_collection_and_playlist_parse_back() {
        let x = rekordbox_xml("loki & friends", &tracks());
        assert!(x.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
        let root = parse(&x);
        assert_eq!((root.name.as_str(), root.attr("Version")), ("DJ_PLAYLISTS", "1.0.0"));
        assert_eq!(root.kid("PRODUCT").attr("Name"), "djforge");
        let coll = root.kid("COLLECTION");
        assert_eq!(coll.attr("Entries"), "2");
        let t = coll.kids("TRACK");
        assert_eq!(t.len(), 2);
        let a = t[0];
        let want = [
            ("TrackID", "1"),
            ("Name", "A"),
            ("Artist", "Artist"),
            ("Album", ""),
            ("Kind", "MP3 File"),
            ("Size", "1234567"),
            ("TotalTime", "313"),
            ("AverageBpm", "128.00"),
            ("Tonality", "8A"),
            ("Location", "file://localhost/C:/Music/Set/a%20b.mp3"),
        ];
        for (k, v) in want {
            assert_eq!(a.attr(k), v, "{k}");
        }
        assert_eq!((t[1].attr("TrackID"), t[1].attr("Kind")), ("2", "FLAC File"));
        // The playlist under ROOT, in set order.
        let rootnode = root.kid("PLAYLISTS").kid("NODE");
        assert_eq!((rootnode.attr("Type"), rootnode.attr("Name"), rootnode.attr("Count")), ("0", "ROOT", "1"));
        let list = rootnode.kid("NODE");
        assert_eq!((list.attr("Name"), list.attr("Type"), list.attr("KeyType"), list.attr("Entries")), ("loki & friends", "1", "0", "2"));
        assert_eq!(list.kids("TRACK").iter().map(|k| k.attr("Key")).collect::<Vec<_>>(), ["1", "2"]);
    }

    #[test]
    fn there_is_a_tempo_per_bar() {
        let root = parse(&rekordbox_xml("set", &tracks()));
        let a = root.kid("COLLECTION").kids("TRACK")[0];
        let tempos = a.kids("TEMPO");
        assert_eq!(tempos.len(), tracks()[0].grid.len());
        let row = |i: usize| (tempos[i].attr("Inizio"), tempos[i].attr("Bpm"), tempos[i].attr("Metro"), tempos[i].attr("Battito"));
        assert_eq!(row(0), ("0.250", "128.00", "4/4", "1"));
        assert_eq!(row(1), ("2.125", "128.00", "4/4", "1"));
        // 240 / 1.8 s; the last bar keeps the one before's tempo.
        assert_eq!(row(8), ("15.175", "133.33", "4/4", "1"));
        assert_eq!(row(9), ("16.975", "133.33", "4/4", "1"));
        // The median bar sets the average.
        assert_eq!(a.attr("AverageBpm"), "128.00");
        let b = root.kid("COLLECTION").kids("TRACK")[1];
        assert_eq!(b.kids("TEMPO").len(), 3);
        assert_eq!(b.attr("AverageBpm"), "120.00");
    }

    #[test]
    fn cues_are_coloured_hot_cues_and_plain_memory_cues() {
        let root = parse(&rekordbox_xml("set", &tracks()));
        let marks = root.kid("COLLECTION").kids("TRACK")[0].kids("POSITION_MARK");
        let hot: Vec<_> = marks.iter().filter(|m| m.attr("Num") != "-1").collect();
        let want = [
            ("Mix in", "0.250", "0", ("40", "226", "20")),
            ("Bass hit", "7.750", "1", ("180", "190", "4")),
            ("Mix out", "11.500", "2", ("230", "40", "40")),
            ("Bass gone", "13.375", "3", ("224", "100", "27")),
        ];
        assert_eq!(hot.len(), 4);
        for (m, (name, start, num, (r, g, b))) in hot.iter().zip(want) {
            assert_eq!((m.attr("Name"), m.attr("Type"), m.attr("Start"), m.attr("Num")), (name, "0", start, num));
            assert_eq!((m.attr("Red"), m.attr("Green"), m.attr("Blue")), (r, g, b));
        }
        let memory: Vec<_> = marks.iter().filter(|m| m.attr("Num") == "-1").collect();
        assert_eq!(memory.iter().map(|m| (m.attr("Name"), m.attr("Start"))).collect::<Vec<_>>(), [("Mix in", "0.250"), ("Mix out", "11.500")]);
        assert!(memory.iter().all(|m| m.attrs.iter().all(|(k, _)| !["Red", "Green", "Blue"].contains(&k.as_str()))));
    }

    #[test]
    fn text_is_escaped_and_control_characters_dropped() {
        let x = rekordbox_xml("a<b>&\"c\"'d'", &tracks());
        assert!(x.contains("Name=\"Tom &amp; &quot;Jerry&quot; &lt;live&gt; it&apos;s\""), "{x}");
        let root = parse(&x);
        let b = root.kid("COLLECTION").kids("TRACK")[1];
        assert_eq!((b.attr("Name"), b.attr("Album")), ("Tom & \"Jerry\" <live> it's", "Al'bum"));
        assert_eq!(root.kid("PLAYLISTS").kid("NODE").kid("NODE").attr("Name"), "a<b>&\"c\"'d'");
    }

    #[test]
    fn a_track_without_a_url_is_left_out() {
        let mut t = tracks();
        t[0].path = PathBuf::from("Audio Files/a.wav");
        let root = parse(&rekordbox_xml("set", &t));
        let coll = root.kid("COLLECTION");
        assert_eq!((coll.attr("Entries"), coll.kids("TRACK").len()), ("1", 1));
        assert_eq!(coll.kids("TRACK")[0].attr("TrackID"), "1");
        assert_eq!(root.kid("PLAYLISTS").kid("NODE").kid("NODE").kids("TRACK").len(), 1);
    }

    #[test]
    fn file_urls_percent_encode_utf8_and_keep_the_drive() {
        let url = |p: &str| file_url(Path::new(p));
        assert_eq!(url(r"C:\Users\marty\Music\a b.mp3").as_deref(), Some("file://localhost/C:/Users/marty/Music/a%20b.mp3"));
        assert_eq!(url(r"c:\x\#1 100%.mp3").as_deref(), Some("file://localhost/C:/x/%231%20100%25.mp3"));
        // U+FF1F (fullwidth ?) and é as their UTF-8 bytes.
        assert_eq!(url("C:/x/why\u{FF1F} caf\u{e9}.mp3").as_deref(), Some("file://localhost/C:/x/why%EF%BC%9F%20caf%C3%A9.mp3"));
        assert_eq!(
            url(r"C:\x\a+b,c;d=e&f(g)[h]'i'~j_k-l.mp3").as_deref(),
            Some("file://localhost/C:/x/a%2Bb%2Cc%3Bd%3De%26f%28g%29%5Bh%5D%27i%27~j_k-l.mp3")
        );
        assert_eq!(url(r"\\?\C:\x\a.mp3").as_deref(), Some("file://localhost/C:/x/a.mp3"));
        // UNC paths name their server.
        assert_eq!(url(r"\\nas\music\a b.mp3").as_deref(), Some("file://nas/music/a%20b.mp3"));
        assert_eq!(url(r"\\?\UNC\nas\music\a.mp3").as_deref(), Some("file://nas/music/a.mp3"));
        assert_eq!(url("/home/m/a b.mp3").as_deref(), Some("file://localhost/home/m/a%20b.mp3"));
        for bad in ["a.mp3", "Audio Files/a.mp3", r"C:a.mp3", r"\\.\COM1", r"\\nas", r"\\?\Volume{x}\a.mp3", "", r"C:\"] {
            assert_eq!(url(bad), None, "{bad}");
        }
    }
}
