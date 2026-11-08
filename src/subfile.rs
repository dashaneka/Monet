//! Subtitle files next to a video ("Movie.srt", "Movie.en.vtt", ...). Monet
//! reads them itself and draws the text over the picture.

use std::path::{Path, PathBuf};

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

const EXTENSIONS: [&str; 4] = ["srt", "vtt", "ass", "ssa"];
/// Column order of ASS/SSA events when a file doesn't say.
const DEFAULT_ASS_FORMAT: &str = "Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text";

struct Cue {
    start: f64,
    end: f64,
    /// Pango markup.
    text: String,
}

pub struct SubtitleFile {
    pub name: String,
    /// Sorted by start time.
    cues: Vec<Cue>,
    longest: f64,
}

impl SubtitleFile {
    pub fn load(path: &Path) -> Option<Self> {
        let text = decode(&std::fs::read(path).ok()?);
        let mut cues = match extension(path)?.as_str() {
            "ass" | "ssa" => parse_ass(&text),
            _ => parse_srt(&text),
        };
        cues.retain(|cue| cue.end > cue.start && !cue.text.trim().is_empty());
        if cues.is_empty() {
            return None;
        }
        cues.sort_by(|a, b| a.start.total_cmp(&b.start));
        let longest = cues.iter().map(|cue| cue.end - cue.start).fold(0.0, f64::max);
        Some(Self {
            name: path.file_name()?.to_string_lossy().into_owned(),
            cues,
            longest,
        })
    }

    /// The text showing at `time` (seconds), as Pango markup. Overlapping
    /// cues stack in start order.
    pub fn text_at(&self, time: f64) -> Option<String> {
        let end = self.cues.partition_point(|cue| cue.start <= time);
        let begin = self.cues[..end].partition_point(|cue| cue.start < time - self.longest);
        let lines: Vec<&str> = self.cues[begin..end]
            .iter()
            .filter(|cue| time < cue.end)
            .map(|cue| cue.text.as_str())
            .collect();
        (!lines.is_empty()).then(|| lines.join("\n"))
    }
}

/// Subtitle files named after the video: "Movie.srt" first, then
/// "Movie.en.srt" and the like in name order.
pub fn find(video: &gio::File) -> Vec<PathBuf> {
    let Some(path) = video.path() else {
        return Vec::new();
    };
    let (Some(stem), Some(dir)) = (path.file_stem().and_then(|s| s.to_str()), path.parent())
    else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(bool, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|candidate| extension(candidate).is_some_and(|ext| EXTENSIONS.contains(&ext.as_str())))
        .filter_map(|candidate| {
            let name = candidate.file_stem()?.to_str()?;
            let exact = name == stem;
            let tagged = name.strip_prefix(stem).is_some_and(|rest| rest.starts_with('.'));
            (exact || tagged).then_some((exact, candidate.clone()))
        })
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found.into_iter().map(|(_, path)| path).collect()
}

fn extension(path: &Path) -> Option<String> {
    Some(path.extension()?.to_str()?.to_ascii_lowercase())
}

/// UTF-8 or UTF-16 by their byte order marks; files that aren't valid UTF-8
/// are read as Latin-1, which older subtitles often are.
fn decode(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, u16::from_be_bytes);
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => bytes.iter().map(|&byte| byte as char).collect(),
    }
}

fn utf16(bytes: &[u8], read: fn([u8; 2]) -> u16) -> String {
    let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| read([pair[0], pair[1]])).collect();
    String::from_utf16_lossy(&units)
}

/// SubRip and WebVTT: a timing line ("00:01:02,500 --> 00:01:04,000"), then
/// text up to a blank line. Everything else (numbers, headers, notes) is skipped.
fn parse_srt(text: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some((start, end)) = parse_timing(line) else {
            continue;
        };
        let body: Vec<&str> = lines.by_ref().take_while(|line| !line.trim().is_empty()).collect();
        cues.push(Cue { start, end, text: to_markup(&body.join("\n")) });
    }
    cues
}

fn parse_timing(line: &str) -> Option<(f64, f64)> {
    let (start, end) = line.split_once("-->")?;
    Some((parse_time(start.trim())?, parse_time(end.split_whitespace().next()?)?))
}

/// "01:02:03,456", "01:02:03.456", "02:03.456" (WebVTT) or "1:02:03.45" (ASS).
fn parse_time(text: &str) -> Option<f64> {
    let parts: Vec<&str> = text.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let mut total = 0.0;
    for part in parts {
        let value: f64 = part.trim().replace(',', ".").parse().ok()?;
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        total = total * 60.0 + value;
    }
    Some(total)
}

/// Keeps <i>, <b> and <u>, drops other tags (fonts, colors, positions) and
/// escapes the rest, so the result is always valid Pango markup.
fn to_markup(text: &str) -> String {
    let mut markup = String::new();
    let mut open: Vec<String> = Vec::new();
    let mut rest = text;
    loop {
        let next = [rest.find('<'), rest.find("{\\")].into_iter().flatten().min();
        let Some(at) = next else {
            markup.push_str(&escape(rest));
            break;
        };
        markup.push_str(&escape(&rest[..at]));
        let closer = if rest[at..].starts_with('<') { '>' } else { '}' };
        let Some(len) = rest[at..].find(closer) else {
            markup.push_str(&escape(&rest[at..]));
            break;
        };
        let tag = rest[at + 1..at + len].trim().to_ascii_lowercase();
        rest = &rest[at + len + 1..];
        if closer != '>' {
            continue;
        }
        match tag.as_str() {
            "i" | "b" | "u" => {
                markup.push_str(&format!("<{tag}>"));
                open.push(tag);
            }
            _ => {
                if let Some(name) = tag.strip_prefix('/')
                    && open.last().is_some_and(|last| last == name)
                {
                    markup.push_str(&format!("</{name}>"));
                    open.pop();
                }
            }
        }
    }
    for tag in open.iter().rev() {
        markup.push_str(&format!("</{tag}>"));
    }
    markup
}

fn escape(text: &str) -> String {
    let text = text
        .replace("&nbsp;", "\u{a0}")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    glib::markup_escape_text(&text).to_string()
}

/// ASS/SSA: "Dialogue:" lines under [Events], with columns named by "Format:".
/// Styling is dropped; the text shows plain, like the rest.
fn parse_ass(text: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut in_events = false;
    let columns = |format: &str| -> Vec<String> {
        format.split(',').map(|name| name.trim().to_ascii_lowercase()).collect()
    };
    let mut format = columns(DEFAULT_ASS_FORMAT);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_events = line.eq_ignore_ascii_case("[events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(names) = line.strip_prefix("Format:") {
            format = columns(names);
        } else if let Some(values) = line.strip_prefix("Dialogue:") {
            let values: Vec<&str> = values.splitn(format.len(), ',').collect();
            let column = |name: &str| {
                format.iter().position(|n| n == name).and_then(|i| values.get(i)).copied()
            };
            let (Some(start), Some(end), Some(raw)) = (
                column("start").and_then(|t| parse_time(t.trim())),
                column("end").and_then(|t| parse_time(t.trim())),
                column("text"),
            ) else {
                continue;
            };
            if let Some(plain) = ass_text(raw) {
                cues.push(Cue { start, end, text: glib::markup_escape_text(&plain).to_string() });
            }
        }
    }
    cues
}

/// Strips {\...} override blocks and turns \N into line breaks. Drawings
/// ({\p1}) have no text to show, so they're skipped.
fn ass_text(raw: &str) -> Option<String> {
    let mut plain = String::new();
    let mut rest = raw;
    while let Some(open) = rest.find('{') {
        plain.push_str(&rest[..open]);
        let Some(len) = rest[open..].find('}') else {
            plain.push_str(&rest[open..]);
            rest = "";
            break;
        };
        let overrides = &rest[open + 1..open + len];
        let drawing = overrides.split('\\').any(|tag| {
            tag.strip_prefix('p')
                .and_then(|n| n.trim().parse::<u32>().ok())
                .is_some_and(|n| n > 0)
        });
        if drawing {
            return None;
        }
        rest = &rest[open + len + 1..];
    }
    plain.push_str(rest);
    Some(
        plain
            .replace("\\N", "\n")
            .replace("\\n", "\n")
            .replace("\\h", "\u{a0}")
            .trim()
            .to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(cues: &[Cue]) -> Vec<(f64, f64, &str)> {
        cues.iter().map(|c| (c.start, c.end, c.text.as_str())).collect()
    }

    #[test]
    fn srt() {
        let cues = parse_srt("1\r\n00:00:01,500 --> 00:00:03,000\r\nHello <i>there</i>\r\nworld & co\r\n\r\n2\r\n00:01:00,000 --> 00:01:02,250\r\n<font color=\"red\">Red</font> {\\an8}top\r\n");
        assert_eq!(
            texts(&cues),
            [(1.5, 3.0, "Hello <i>there</i>\nworld &amp; co"), (60.0, 62.25, "Red top")]
        );
    }

    #[test]
    fn vtt() {
        let cues = parse_srt("WEBVTT\n\nNOTE a comment\n\nintro\n01:02.000 --> 01:04.500 align:start\n<v Bob>Hi &amp; bye</v>\n<b>bold\n");
        assert_eq!(texts(&cues), [(62.0, 64.5, "Hi &amp; bye\n<b>bold</b>")]);
    }

    #[test]
    fn broken_tags_stay_valid_markup() {
        assert_eq!(to_markup("a </i> b <i>c <b>d</i> e"), "a  b <i>c <b>d e</b></i>");
        assert_eq!(to_markup("1 < 2"), "1 &lt; 2");
    }

    #[test]
    fn ass() {
        let file = "[Script Info]\nTitle: x\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.50,0:00:04.00,Default,,0,0,0,,{\\i1}Hello,{\\i0} you\\Nthere\nDialogue: 0,0:00:02.00,0:00:03.00,Default,,0,0,0,,{\\p1}m 0 0 l 10 10{\\p0}\nComment: 0,0:00:05.00,0:00:06.00,Default,,0,0,0,,hidden\n";
        assert_eq!(texts(&parse_ass(file)), [(1.5, 4.0, "Hello, you\nthere")]);
    }

    #[test]
    fn text_at_stacks_overlaps() {
        let file = SubtitleFile {
            name: String::new(),
            cues: vec![
                Cue { start: 0.0, end: 10.0, text: "long".into() },
                Cue { start: 2.0, end: 3.0, text: "short".into() },
            ],
            longest: 10.0,
        };
        assert_eq!(file.text_at(2.5).as_deref(), Some("long\nshort"));
        assert_eq!(file.text_at(5.0).as_deref(), Some("long"));
        assert_eq!(file.text_at(10.0), None);
    }

    #[test]
    fn encodings() {
        assert_eq!(decode(b"\xEF\xBB\xBFcaf\xC3\xA9"), "café");
        assert_eq!(decode(b"caf\xE9"), "café");
        assert_eq!(decode(b"\xFF\xFEh\x00i\x00"), "hi");
    }
}
