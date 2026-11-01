use std::fmt;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Image,
    Audio,
    Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Format {
    Png,
    Jpeg,
    Gif,
    Bmp,
    Tiff,
    Ico,
    Webp,
    Avif,
    Jxl,
    Tga,
    Qoi,
    Pnm,
    Hdr,
    Exr,
    Dds,
    Svg,
    Psd,
    Wav,
    Flac,
    Mp3,
    Ogg,
    Opus,
    Aiff,
    Caf,
    M4a,
    Mka,
    Mp2,
    Mp4,
    Mov,
    Mkv,
    Webm,
    Avi,
}

impl Format {
    pub const ALL: [Format; 32] = [
        Format::Png,
        Format::Jpeg,
        Format::Gif,
        Format::Bmp,
        Format::Tiff,
        Format::Ico,
        Format::Webp,
        Format::Avif,
        Format::Jxl,
        Format::Tga,
        Format::Qoi,
        Format::Pnm,
        Format::Hdr,
        Format::Exr,
        Format::Dds,
        Format::Svg,
        Format::Psd,
        Format::Wav,
        Format::Flac,
        Format::Mp3,
        Format::Ogg,
        Format::Opus,
        Format::Aiff,
        Format::Caf,
        Format::M4a,
        Format::Mka,
        Format::Mp2,
        Format::Mp4,
        Format::Mov,
        Format::Mkv,
        Format::Webm,
        Format::Avi,
    ];

    pub fn kind(self) -> Kind {
        use Format::*;
        match self {
            Png | Jpeg | Gif | Bmp | Tiff | Ico | Webp | Avif | Jxl | Tga | Qoi | Pnm | Hdr | Exr | Dds | Svg | Psd => {
                Kind::Image
            }
            Wav | Flac | Mp3 | Ogg | Opus | Aiff | Caf | M4a | Mka | Mp2 => Kind::Audio,
            Mp4 | Mov | Mkv | Webm | Avi => Kind::Video,
        }
    }

    /// Canonical extension used for output files.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpeg => "jpg",
            Format::Gif => "gif",
            Format::Bmp => "bmp",
            Format::Tiff => "tiff",
            Format::Ico => "ico",
            Format::Webp => "webp",
            Format::Avif => "avif",
            Format::Jxl => "jxl",
            Format::Tga => "tga",
            Format::Qoi => "qoi",
            Format::Pnm => "ppm",
            Format::Hdr => "hdr",
            Format::Exr => "exr",
            Format::Dds => "dds",
            Format::Svg => "svg",
            Format::Psd => "psd",
            Format::Wav => "wav",
            Format::Flac => "flac",
            Format::Mp3 => "mp3",
            Format::Ogg => "ogg",
            Format::Opus => "opus",
            Format::Aiff => "aiff",
            Format::Caf => "caf",
            Format::M4a => "m4a",
            Format::Mka => "mka",
            Format::Mp2 => "mp2",
            Format::Mp4 => "mp4",
            Format::Mov => "mov",
            Format::Mkv => "mkv",
            Format::Webm => "webm",
            Format::Avi => "avi",
        }
    }

    /// Whether the quality preset changes anything for this format as an output.
    pub fn uses_quality(self) -> bool {
        matches!(
            self,
            Format::Png | Format::Jpeg | Format::Gif | Format::Webp | Format::Avif | Format::Mp3 | Format::Ogg | Format::Opus
        )
    }

    /// Files of this format may hold several images: animation frames, pages or icon sizes.
    pub fn multi_image_input(self) -> bool {
        matches!(self, Format::Gif | Format::Webp | Format::Png | Format::Tiff | Format::Ico | Format::Jxl | Format::Avif)
    }

    /// Several images in this format are separate pictures (pages, icon sizes), not an animation.
    pub fn paged_input(self) -> bool {
        matches!(self, Format::Tiff | Format::Ico)
    }

    /// We write every frame of an animation in this format.
    pub fn animated_output(self) -> bool {
        matches!(self, Format::Gif | Format::Webp | Format::Png)
    }

    /// We write several separate pages in this format.
    pub fn paged_output(self) -> bool {
        matches!(self, Format::Tiff)
    }

    /// Does converting a multi-image `from` file to `to` keep every image? Animations fit
    /// animated or paged outputs; pages only fit paged ones.
    pub fn keeps_all_images(from: Format, to: Format) -> bool {
        if from.paged_input() { to.paged_output() } else { to.animated_output() || to.paged_output() }
    }

    /// Whether this format keeps transparency when we write it.
    pub fn keeps_alpha(self) -> bool {
        use Format::*;
        matches!(self, Png | Gif | Bmp | Tiff | Ico | Webp | Avif | Jxl | Tga | Qoi | Exr)
    }

    pub fn is_lossy(self) -> bool {
        // Videos almost always carry lossy video and sound.
        matches!(
            self,
            Format::Jpeg
                | Format::Avif
                | Format::Dds
                | Format::Mp3
                | Format::Ogg
                | Format::Opus
                | Format::Mp2
                | Format::Mp4
                | Format::Mov
                | Format::Mkv
                | Format::Webm
                | Format::Avi
        )
    }

    fn from_extension(ext: &str) -> Option<Format> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "png" | "apng" => Format::Png,
            "jpg" | "jpeg" | "jpe" | "jfif" => Format::Jpeg,
            "gif" => Format::Gif,
            "bmp" | "dib" => Format::Bmp,
            "tif" | "tiff" => Format::Tiff,
            "ico" => Format::Ico,
            "webp" => Format::Webp,
            "avif" => Format::Avif,
            "jxl" => Format::Jxl,
            "tga" | "icb" | "vda" | "vst" => Format::Tga,
            "qoi" => Format::Qoi,
            "pnm" | "ppm" | "pgm" | "pbm" | "pam" => Format::Pnm,
            "hdr" | "rgbe" => Format::Hdr,
            "exr" => Format::Exr,
            "dds" => Format::Dds,
            "svg" | "svgz" => Format::Svg,
            "psd" => Format::Psd,
            "wav" | "wave" => Format::Wav,
            "flac" => Format::Flac,
            "mp3" => Format::Mp3,
            "ogg" | "oga" => Format::Ogg,
            "opus" => Format::Opus,
            "aif" | "aiff" | "aifc" => Format::Aiff,
            "caf" => Format::Caf,
            "m4a" | "m4b" | "alac" => Format::M4a,
            "mka" | "weba" => Format::Mka,
            "mp4" | "m4v" => Format::Mp4,
            "mov" | "qt" => Format::Mov,
            "mkv" => Format::Mkv,
            "webm" => Format::Webm,
            "avi" => Format::Avi,
            "mp2" | "mp1" | "mpa" | "m2a" => Format::Mp2,
            _ => return None,
        })
    }

    /// Identify a format from the first bytes of a file.
    pub fn sniff(head: &[u8]) -> Option<Format> {
        let starts = |p: &[u8]| head.starts_with(p);
        let at = |o: usize, p: &[u8]| head.get(o..o + p.len()) == Some(p);
        let text = String::from_utf8_lossy(head);
        if starts(b"\x89PNG\r\n\x1a\n") {
            Some(Format::Png)
        } else if starts(&[0xFF, 0xD8, 0xFF]) {
            Some(Format::Jpeg)
        } else if starts(b"GIF87a") || starts(b"GIF89a") {
            Some(Format::Gif)
        } else if starts(b"BM") {
            Some(Format::Bmp)
        } else if starts(b"II*\0") || starts(b"MM\0*") {
            Some(Format::Tiff)
        } else if starts(&[0, 0, 1, 0]) {
            Some(Format::Ico)
        } else if starts(b"RIFF") && at(8, b"WEBP") {
            Some(Format::Webp)
        } else if at(4, b"ftyp") && (at(8, b"avif") || at(8, b"avis")) {
            Some(Format::Avif)
        } else if at(4, b"ftyp") && (at(8, b"M4A ") || at(8, b"M4B ")) {
            Some(Format::M4a)
        } else if at(4, b"ftyp") && at(8, b"qt  ") {
            Some(Format::Mov)
        } else if at(4, b"ftyp") && !HEIF_BRANDS.iter().any(|b| at(8, b)) {
            // Any other ISO media file: audio or video, `probe::refine` looks at the tracks.
            Some(Format::Mp4)
        } else if starts(&[0xFF, 0x0A]) || starts(b"\0\0\0\x0cJXL \r\n\x87\n") {
            Some(Format::Jxl)
        } else if starts(b"qoif") {
            Some(Format::Qoi)
        } else if head.len() >= 3 && head[0] == b'P' && (b'1'..=b'7').contains(&head[1]) && head[2].is_ascii_whitespace() {
            Some(Format::Pnm)
        } else if starts(b"#?RADIANCE") || starts(b"#?RGBE") {
            Some(Format::Hdr)
        } else if starts(&[0x76, 0x2F, 0x31, 0x01]) {
            Some(Format::Exr)
        } else if starts(b"DDS ") {
            Some(Format::Dds)
        } else if starts(b"8BPS") {
            Some(Format::Psd)
        } else if text.trim_start_matches('\u{feff}').trim_start().starts_with("<svg") {
            Some(Format::Svg)
        } else if starts(b"RIFF") && at(8, b"AVI ") {
            Some(Format::Avi)
        } else if starts(b"RIFF") && at(8, b"WAVE") {
            Some(Format::Wav)
        } else if starts(b"FORM") && (at(8, b"AIFF") || at(8, b"AIFC")) {
            Some(Format::Aiff)
        } else if starts(b"caff") {
            Some(Format::Caf)
        } else if starts(&[0x1A, 0x45, 0xDF, 0xA3]) {
            // Matroska or WebM, audio or video: `probe::refine` looks at the tracks.
            Some(Format::Mkv)
        } else if starts(b"fLaC") {
            Some(Format::Flac)
        } else if starts(b"OggS") {
            // Opus streams carry an "OpusHead" packet in the first page.
            if head.windows(8).any(|w| w == b"OpusHead") {
                Some(Format::Opus)
            } else {
                Some(Format::Ogg)
            }
        } else if starts(b"ID3") {
            Some(Format::Mp3)
        } else if head.len() >= 2 && head[0] == 0xFF && head[1] & 0xE0 == 0xE0 {
            // MPEG audio frame sync; the layer bits tell Layer III from I/II.
            if (head[1] >> 1) & 3 == 1 { Some(Format::Mp3) } else { Some(Format::Mp2) }
        } else {
            None
        }
    }

    /// Detect from content first, falling back to the file extension. Containers that hold
    /// either audio or video (MP4/MOV, Matroska/WebM) are looked inside to tell which.
    pub fn detect(path: &Path) -> Option<Format> {
        let mut head = [0u8; 64];
        let n = std::fs::File::open(path)
            .and_then(|mut f| f.read(&mut head))
            .unwrap_or(0);
        let f = Format::sniff(&head[..n]).or_else(|| path.extension()?.to_str().and_then(Format::from_extension))?;
        Some(crate::probe::refine(path, f))
    }
}

/// ISO media brands of HEIF/HEIC still images, which aren't supported.
const HEIF_BRANDS: [&[u8]; 7] = [b"heic", b"heix", b"hevc", b"heim", b"heis", b"mif1", b"msf1"];

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.extension().to_ascii_uppercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_common_signatures() {
        assert_eq!(Format::sniff(b"\x89PNG\r\n\x1a\n...."), Some(Format::Png));
        assert_eq!(Format::sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(Format::Webp));
        assert_eq!(Format::sniff(b"RIFF\0\0\0\0WAVEfmt "), Some(Format::Wav));
        assert_eq!(Format::sniff(b"OggS\0\x02....OpusHead"), Some(Format::Opus));
        assert_eq!(Format::sniff(b"OggS\0\x02....\x01vorbis"), Some(Format::Ogg));
        assert_eq!(Format::sniff(b"ID3\x04"), Some(Format::Mp3));
        assert_eq!(Format::sniff(b"\0\0\0\x1cftypavif"), Some(Format::Avif));
        assert_eq!(Format::sniff(b"\0\0\0\x1cftypM4A "), Some(Format::M4a));
        assert_eq!(Format::sniff(b"\0\0\0\x1cftypisom"), Some(Format::Mp4));
        assert_eq!(Format::sniff(b"\0\0\0\x14ftypqt  "), Some(Format::Mov));
        assert_eq!(Format::sniff(b"\0\0\0\x18ftypheic"), None);
        assert_eq!(Format::sniff(&[0x1A, 0x45, 0xDF, 0xA3, 0x9F]), Some(Format::Mkv));
        assert_eq!(Format::sniff(b"RIFF\0\0\0\0AVI LIST"), Some(Format::Avi));
        assert_eq!(Format::sniff(b"FORM\0\0\0\0AIFFCOMM"), Some(Format::Aiff));
        assert_eq!(Format::sniff(b"P6\n4 4\n255\n"), Some(Format::Pnm));
        assert_eq!(Format::sniff(b"  <svg xmlns="), Some(Format::Svg));
        assert_eq!(Format::sniff(&[0xFF, 0xFD, 0x90]), Some(Format::Mp2));
        assert_eq!(Format::sniff(&[0xFF, 0xFB, 0x90]), Some(Format::Mp3));
        assert_eq!(Format::sniff(b"nope"), None);
    }

    #[test]
    fn every_format_round_trips_its_extension() {
        for f in Format::ALL {
            assert_eq!(Format::from_extension(f.extension()), Some(f), "{f}");
        }
    }
}
