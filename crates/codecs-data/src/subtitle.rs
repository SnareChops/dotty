//! SubRip (.srt), WebVTT (.vtt) and Advanced SubStation (.ass/.ssa) subtitles through a list
//! of cues. A cue's text keeps only line breaks and `<b>`, `<i>`, `<u>` (with `&`, `<`, `>`
//! escaped as entities, the way WebVTT does); fonts, colours and positions are dropped.

use dotty_core::{ConvertCtx, Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub start: u64,
    pub end: u64,
    pub text: String,
}

fn bad(what: &str, why: impl std::fmt::Display) -> Error {
    Error::Codec(format!("not a valid {what} file: {why}"))
}

/// `h:mm:ss,mmm`, `mm:ss.mmm`, or ASS `h:mm:ss.cc` (the fraction's length sets its unit).
fn parse_time(s: &str) -> Option<u64> {
    let s = s.trim();
    let (clock, frac) = s.split_once(['.', ',']).map_or((s, ""), |(a, b)| (a, b));
    let parts: Vec<u64> = clock.split(':').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
    let secs = match parts[..] {
        [m, s] => m * 60 + s,
        [h, m, s] => h * 3600 + m * 60 + s,
        _ => return None,
    };
    let ms = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take_while(char::is_ascii_digit).take(3).collect();
        let n: u64 = digits.parse().ok()?;
        n * 10u64.pow(3 - digits.len() as u32)
    };
    Some(secs * 1000 + ms)
}

fn fmt_time(ms: u64, comma: bool) -> String {
    format!("{:02}:{:02}:{:02}{}{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, if comma { ',' } else { '.' }, ms % 1000)
}

fn ass_time(ms: u64) -> String {
    let cs = (ms + 5) / 10;
    format!("{}:{:02}:{:02}.{:02}", cs / 360_000, cs / 6000 % 60, cs / 100 % 60, cs % 100)
}

/// Keep `<b> <i> <u>` (any case), turn `<br>` into a line break, drop every other tag
/// (fonts, colours, voices, karaoke timestamps, ruby…), and escape what's left.
fn clean_markup(raw: &str) -> String {
    clean_tags(raw).trim().to_string()
}

fn clean_tags(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(j) = after.find('>') else {
            out.push_str("&lt;");
            rest = after;
            continue;
        };
        let tag = after[..j].trim().to_ascii_lowercase();
        let name = tag.trim_start_matches('/');
        match name {
            "b" | "i" | "u" => out.push_str(&format!("<{}{name}>", if tag.starts_with('/') { "/" } else { "" })),
            "br" | "br/" | "br /" => out.push('\n'),
            _ => {}
        }
        rest = &after[j + 1..];
    }
    out.push_str(rest);
    // Text that was already escaped stays so; a bare `&` is escaped.
    let mut fixed = String::with_capacity(out.len());
    for (i, c) in out.char_indices() {
        if c == '&' && !is_entity(&out[i..]) {
            fixed.push_str("&amp;");
        } else {
            fixed.push(c);
        }
    }
    fixed
}

fn is_entity(s: &str) -> bool {
    s.find(';').is_some_and(|j| (2..=8).contains(&j) && s[1..j].chars().all(|c| c.is_ascii_alphanumeric() || c == '#'))
}

/// Cue text without markup or entities.
pub fn plain(text: &str) -> String {
    unescape(&strip_tags(text))
}

fn strip_tags(text: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&nbsp;", " ").replace("&lrm;", "").replace("&rlm;", "").replace("&amp;", "&")
}

/// Cue text as SRT wants it: tags kept, entities turned back into characters.
fn srt_text(text: &str) -> String {
    // Unescape only outside tags: our tags are the only `<` left after cleaning.
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find('<') {
        let Some(j) = rest[i..].find('>') else { break };
        out.push_str(&unescape(&rest[..i]));
        out.push_str(&rest[i..=i + j]);
        rest = &rest[i + j + 1..];
    }
    out.push_str(&unescape(rest));
    out
}

fn blocks(text: &str) -> Vec<Vec<&str>> {
    let mut all = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !cur.is_empty() {
                all.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(line.trim_end());
        }
    }
    if !cur.is_empty() {
        all.push(cur);
    }
    all
}

fn arrow(line: &str) -> Option<(u64, u64)> {
    let (a, b) = line.split_once("-->")?;
    let end = b.split_whitespace().next()?;
    Some((parse_time(a)?, parse_time(end)?))
}

pub fn parse_srt(text: &str) -> Result<Vec<Cue>> {
    let mut cues = Vec::new();
    for block in blocks(text.trim_start_matches('\u{feff}')) {
        let Some(at) = block.iter().position(|l| l.contains("-->")) else { continue };
        let Some((start, end)) = arrow(block[at]) else { continue };
        // Players use `{\an8}`-style position codes here; they aren't text.
        let body = block[at + 1..].join("\n");
        let mut clean = String::new();
        let mut depth = 0;
        for c in body.chars() {
            match c {
                '{' => depth += 1,
                '}' if depth > 0 => depth -= 1,
                c if depth == 0 => clean.push(c),
                _ => {}
            }
        }
        cues.push(Cue { start, end, text: clean_markup(&clean) });
    }
    if cues.is_empty() { Err(bad("SubRip", "no timed captions found")) } else { Ok(cues) }
}

pub fn parse_vtt(text: &str) -> Result<Vec<Cue>> {
    let text = text.trim_start_matches('\u{feff}');
    if !text.starts_with("WEBVTT") {
        return Err(bad("WebVTT", "it doesn't start with WEBVTT"));
    }
    let mut cues = Vec::new();
    for block in blocks(text) {
        let first = block[0];
        if first.starts_with("WEBVTT") || first.starts_with("NOTE") || first.starts_with("STYLE") || first.starts_with("REGION") {
            continue;
        }
        let Some(at) = block.iter().position(|l| l.contains("-->")) else { continue };
        let Some((start, end)) = arrow(block[at]) else { continue };
        cues.push(Cue { start, end, text: clean_markup(&block[at + 1..].join("\n")) });
    }
    if cues.is_empty() { Err(bad("WebVTT", "no captions found")) } else { Ok(cues) }
}

pub fn parse_ass(text: &str, ctx: &ConvertCtx) -> Result<Vec<Cue>> {
    let mut in_events = false;
    let mut columns: Vec<String> = Vec::new();
    let mut cues = Vec::new();
    let mut styled = false;
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_events = line.eq_ignore_ascii_case("[events]");
            continue;
        }
        if !in_events {
            continue;
        }
        if let Some(f) = line.strip_prefix("Format:") {
            columns = f.split(',').map(|c| c.trim().to_ascii_lowercase()).collect();
        } else if let Some(d) = line.strip_prefix("Dialogue:") {
            let cols = if columns.is_empty() { vec!["layer".into(), "start".into(), "end".into(), "style".into(), "name".into(), "marginl".into(), "marginr".into(), "marginv".into(), "effect".into(), "text".into()] } else { columns.clone() };
            let fields: Vec<&str> = d.trim_start().splitn(cols.len(), ',').collect();
            let get = |name: &str| cols.iter().position(|c| c == name).and_then(|i| fields.get(i)).copied();
            let (Some(start), Some(end), Some(raw)) = (get("start").and_then(parse_time), get("end").and_then(parse_time), get("text")) else { continue };
            let (text, had_codes) = ass_text(raw);
            styled |= had_codes;
            cues.push(Cue { start, end, text });
        }
    }
    if cues.is_empty() {
        return Err(bad("SubStation", "no dialogue lines found"));
    }
    cues.sort_by_key(|c| (c.start, c.end));
    if styled {
        ctx.note("Positions, fonts, colours and effects of the SubStation file were dropped (bold, italic and underline were kept)");
    }
    Ok(cues)
}

/// ASS text with `{\i1}`-style overrides turned into tags; the flag says something else
/// (position, colour, karaoke…) was dropped.
fn ass_text(raw: &str) -> (String, bool) {
    let mut out = String::new();
    let mut dropped = false;
    let mut rest = raw;
    while let Some(i) = rest.find('{') {
        out.push_str(&plain_ass(&rest[..i]));
        let Some(j) = rest[i..].find('}') else {
            rest = &rest[i + 1..];
            continue;
        };
        for tag in rest[i + 1..i + j].split('\\').filter(|t| !t.trim().is_empty()) {
            let (name, on) = (tag.chars().next().unwrap_or(' '), tag[1..].trim());
            match (name, on) {
                ('b' | 'i' | 'u', "1") => out.push_str(&format!("<{name}>")),
                ('b' | 'i' | 'u', "0") => out.push_str(&format!("</{name}>")),
                _ => dropped = true,
            }
        }
        rest = &rest[i + j + 1..];
    }
    out.push_str(&plain_ass(rest));
    (out, dropped)
}

fn plain_ass(s: &str) -> String {
    let s = s.replace("\\N", "\n").replace("\\n", "\n").replace("\\h", " ");
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn write_srt(cues: &[Cue]) -> String {
    let mut out = String::new();
    for (i, c) in cues.iter().enumerate() {
        out.push_str(&format!("{}\n{} --> {}\n{}\n\n", i + 1, fmt_time(c.start, true), fmt_time(c.end, true), srt_text(&c.text)));
    }
    out
}

pub fn write_vtt(cues: &[Cue]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for c in cues {
        out.push_str(&format!("{} --> {}\n{}\n\n", fmt_time(c.start, false), fmt_time(c.end, false), c.text));
    }
    out
}

pub fn write_ass(cues: &[Cue]) -> String {
    let mut out = String::from(
        "[Script Info]\nScriptType: v4.00+\nPlayResX: 1920\nPlayResY: 1080\nWrapStyle: 0\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\nStyle: Default,Arial,64,&H00FFFFFF,&H000000FF,&H00000000,&H80000000,0,0,0,0,100,100,0,0,1,2,1,2,40,40,50,1\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
    );
    for c in cues {
        let mut text = srt_text(&c.text).replace('\n', "\\N");
        for t in ['b', 'i', 'u'] {
            text = text.replace(&format!("<{t}>"), &format!("{{\\{t}1}}")).replace(&format!("</{t}>"), &format!("{{\\{t}0}}"));
        }
        out.push_str(&format!("Dialogue: 0,{},{},Default,,0,0,0,,{text}\n", ass_time(c.start), ass_time(c.end)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn ctx_notes<R>(f: impl FnOnce(&ConvertCtx) -> R) -> (R, Vec<String>) {
        let (opts, cancel, progress) = (dotty_core::Options::default(), AtomicBool::new(false), |_: f32| {});
        let ctx = ConvertCtx::new(&opts, &cancel, &progress);
        let r = f(&ctx);
        (r, ctx.take_notes())
    }

    const SRT: &str = "\u{feff}1\r\n00:00:01,000 --> 00:00:02,500\r\n<i>Hello</i> & <font color=\"red\">world</font>\r\n\r\n2\r\n00:01:02,5 --> 01:00:03,042\r\n{\\an8}Line one\r\nLine <two>\r\n";

    #[test]
    fn srt_is_read_leniently_and_written_back_the_same() {
        let cues = parse_srt(SRT).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!((cues[0].start, cues[0].end), (1000, 2500));
        assert_eq!(cues[0].text, "<i>Hello</i> &amp; world", "font tag dropped, & escaped");
        assert_eq!((cues[1].start, cues[1].end), (62_500, 3_603_042), "short fractions and hours");
        assert_eq!(cues[1].text, "Line one\nLine", "position code and unknown tag dropped");
        let out = write_srt(&cues);
        assert!(out.starts_with("1\n00:00:01,000 --> 00:00:02,500\n<i>Hello</i> & world\n\n2\n"), "{out}");
        assert_eq!(parse_srt(&out).unwrap(), cues);
        assert!(parse_srt("nothing here").is_err());
    }

    #[test]
    fn vtt_round_trips_with_ids_notes_settings_and_voices() {
        let vtt = "WEBVTT - title\n\nNOTE a comment\n\nSTYLE\n::cue { color: red }\n\nintro\n00:01.000 --> 00:02.000 align:start\n<v Ann>Hi <b>there</b> <00:01.500>you</v>\n\n01:00:00.000 --> 01:00:01.000\nA &amp; B\n";
        let cues = parse_vtt(vtt).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "Hi <b>there</b> you");
        assert_eq!(cues[1].start, 3_600_000);
        assert_eq!(cues[1].text, "A &amp; B", "entities stay entities");
        let out = write_vtt(&cues);
        assert!(out.starts_with("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n"));
        assert_eq!(parse_vtt(&out).unwrap(), cues);
        assert!(parse_vtt(SRT).is_err());
        // To SRT, entities become characters.
        assert!(write_srt(&cues).contains("A & B"));
    }

    #[test]
    fn ass_reads_dialogue_and_styles_and_writes_a_playable_file() {
        let ass = "[Script Info]\nTitle: x\n\n[V4+ Styles]\nFormat: Name\nStyle: Default\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:03.00,0:00:04.50,Default,,0,0,0,,{\\i1}Hey{\\i0}, you\\Nthere {\\pos(10,10)}<ok>\nComment: 0,0:00:00.00,0:00:01.00,Default,,0,0,0,,not a caption\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,First, with comma\n";
        let (cues, notes) = ctx_notes(|c| parse_ass(ass, c).unwrap());
        assert_eq!(cues.len(), 2);
        assert_eq!((cues[0].start, cues[0].text.as_str()), (1000, "First, with comma"), "sorted by start");
        assert_eq!(cues[1].text, "<i>Hey</i>, you\nthere &lt;ok&gt;");
        assert_eq!((cues[1].start, cues[1].end), (3000, 4500));
        assert!(notes.iter().any(|n| n.contains("dropped")));
        let out = write_ass(&cues);
        assert!(out.contains("Dialogue: 0,0:00:03.00,0:00:04.50,Default,,0,0,0,,{\\i1}Hey{\\i0}, you\\Nthere <ok>"), "{out}");
        assert_eq!(ctx_notes(|c| parse_ass(&out, c).unwrap()).0, cues);
        assert!(ctx_notes(|c| parse_ass("[Events]\n", c)).0.is_err());
    }

    #[test]
    fn times_parse_and_print() {
        assert_eq!(parse_time("1:02:03.04"), Some(3_723_040));
        assert_eq!(parse_time("02:03,5"), Some(123_500));
        assert_eq!(parse_time("x"), None);
        assert_eq!(fmt_time(3_723_456, true), "01:02:03,456");
        assert_eq!(ass_time(3_723_456), "1:02:03.46");
        assert_eq!(plain("<b>a</b> &amp; b"), "a & b");
    }
}
