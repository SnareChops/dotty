use std::fmt;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Image,
    Audio,
    Video,
    Document,
    Data,
    Archive,
    Font,
    Subtitle,
    Model,
}

impl Kind {
    /// In the order the app lists them.
    pub const ALL: [Kind; 9] = [Kind::Image, Kind::Audio, Kind::Video, Kind::Document, Kind::Data, Kind::Archive, Kind::Font, Kind::Subtitle, Kind::Model];

    /// Heading for the list of supported formats.
    pub fn title(self) -> &'static str {
        match self {
            Kind::Image => "Images",
            Kind::Audio => "Audio",
            Kind::Video => "Video",
            Kind::Document => "Documents",
            Kind::Data => "Data",
            Kind::Archive => "Archives",
            Kind::Font => "Fonts",
            Kind::Subtitle => "Subtitles",
            Kind::Model => "3D models",
        }
    }

    /// Plural noun for "All … →" shortcuts.
    pub fn plural(self) -> &'static str {
        match self {
            Kind::Image => "images",
            Kind::Audio => "audio files",
            Kind::Video => "videos",
            Kind::Document => "documents",
            Kind::Data => "data files",
            Kind::Archive => "archives",
            Kind::Font => "fonts",
            Kind::Subtitle => "subtitle files",
            Kind::Model => "3D models",
        }
    }
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
    Json,
    Yaml,
    Toml,
    Xml,
    Csv,
    Srt,
    Vtt,
    Ass,
    Ttf,
    Otf,
    Woff,
    Woff2,
    Zip,
    Cbz,
    Tar,
    TarGz,
    Gz,
    SevenZ,
    Folder,
    Favicon,
    Md,
    Html,
    Pdf,
    Txt,
    Epub,
    Mobi,
    Raw,
    Stl,
    Obj,
    Ply,
    Gltf,
    Glb,
}

impl Format {
    pub const ALL: [Format; 64] = [
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
        Format::Json,
        Format::Yaml,
        Format::Toml,
        Format::Xml,
        Format::Csv,
        Format::Srt,
        Format::Vtt,
        Format::Ass,
        Format::Ttf,
        Format::Otf,
        Format::Woff,
        Format::Woff2,
        Format::Zip,
        Format::Cbz,
        Format::Tar,
        Format::TarGz,
        Format::Gz,
        Format::SevenZ,
        Format::Folder,
        Format::Favicon,
        Format::Md,
        Format::Html,
        Format::Pdf,
        Format::Txt,
        Format::Epub,
        Format::Mobi,
        Format::Raw,
        Format::Stl,
        Format::Obj,
        Format::Ply,
        Format::Gltf,
        Format::Glb,
    ];

    pub fn kind(self) -> Kind {
        use Format::*;
        match self {
            Png | Jpeg | Gif | Bmp | Tiff | Ico | Webp | Avif | Jxl | Tga | Qoi | Pnm | Hdr | Exr | Dds | Svg | Psd => {
                Kind::Image
            }
            Wav | Flac | Mp3 | Ogg | Opus | Aiff | Caf | M4a | Mka | Mp2 => Kind::Audio,
            Mp4 | Mov | Mkv | Webm | Avi => Kind::Video,
            Json | Yaml | Toml | Xml | Csv => Kind::Data,
            Srt | Vtt | Ass => Kind::Subtitle,
            Ttf | Otf | Woff | Woff2 => Kind::Font,
            Zip | Cbz | Tar | TarGz | Gz | SevenZ | Folder => Kind::Archive,
            Favicon | Raw => Kind::Image,
            Stl | Obj | Ply | Gltf | Glb => Kind::Model,
            Md | Html | Pdf | Txt | Epub | Mobi => Kind::Document,
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
            Format::Json => "json",
            Format::Yaml => "yaml",
            Format::Toml => "toml",
            Format::Xml => "xml",
            Format::Csv => "csv",
            Format::Srt => "srt",
            Format::Vtt => "vtt",
            Format::Ass => "ass",
            Format::Ttf => "ttf",
            Format::Otf => "otf",
            Format::Woff => "woff",
            Format::Woff2 => "woff2",
            Format::Zip => "zip",
            Format::Cbz => "cbz",
            Format::Tar => "tar",
            Format::TarGz => "tgz",
            Format::Gz => "gz",
            Format::SevenZ => "7z",
            Format::Folder => "folder",
            Format::Favicon => "favicon",
            Format::Md => "md",
            Format::Html => "html",
            Format::Pdf => "pdf",
            Format::Txt => "txt",
            Format::Epub => "epub",
            Format::Mobi => "mobi",
            Format::Raw => "raw",
            Format::Stl => "stl",
            Format::Obj => "obj",
            Format::Ply => "ply",
            Format::Gltf => "gltf",
            Format::Glb => "glb",
        }
    }

    /// Whether the quality preset changes anything for this format as an output.
    pub fn uses_quality(self) -> bool {
        matches!(
            self,
            Format::Png
                | Format::Jpeg
                | Format::Gif
                | Format::Webp
                | Format::Avif
                | Format::Mp3
                | Format::Ogg
                | Format::Opus
                | Format::Mp4
                | Format::Mkv
                | Format::Webm
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

    /// Results that aren't a file type of their own: "extract to a folder", "favicon pack".
    /// They can be chosen as a target but never recognised in a file.
    pub fn is_pseudo(self) -> bool {
        matches!(self, Format::Folder | Format::Favicon)
    }

    /// Camera RAW file extensions (lower case). Most are TIFF underneath, so only the file
    /// name tells them from a TIFF picture.
    pub const RAW_EXTENSIONS: [&'static str; 24] = [
        "raw", "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "dng", "orf", "rw2", "rwl", "raf", "pef", "srw", "3fr", "erf", "kdc", "mrw", "mef", "mos", "iiq", "dcr",
    ];

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
            "json" | "geojson" | "jsonc" => Format::Json,
            "yaml" | "yml" => Format::Yaml,
            "toml" => Format::Toml,
            "xml" | "plist" | "xsd" | "rss" | "atom" => Format::Xml,
            "csv" | "tsv" => Format::Csv,
            "srt" => Format::Srt,
            "vtt" => Format::Vtt,
            "ass" | "ssa" => Format::Ass,
            "ttf" => Format::Ttf,
            "otf" => Format::Otf,
            "woff" => Format::Woff,
            "woff2" => Format::Woff2,
            "zip" => Format::Zip,
            "cbz" => Format::Cbz,
            "tar" => Format::Tar,
            "tgz" => Format::TarGz,
            "gz" => Format::Gz,
            "7z" => Format::SevenZ,
            "md" | "markdown" | "mdown" | "mkd" => Format::Md,
            "html" | "htm" | "xhtml" => Format::Html,
            "pdf" => Format::Pdf,
            "txt" | "text" => Format::Txt,
            "stl" => Format::Stl,
            "obj" => Format::Obj,
            "ply" => Format::Ply,
            "gltf" => Format::Gltf,
            "glb" => Format::Glb,
            "epub" => Format::Epub,
            "mobi" | "azw" | "azw3" | "prc" => Format::Mobi,
            ext if Self::RAW_EXTENSIONS.contains(&ext) => Format::Raw,
            _ => return None,
        })
    }

    /// Identify a format from the first bytes of a file.
    pub fn sniff(head: &[u8]) -> Option<Format> {
        let starts = |p: &[u8]| head.starts_with(p);
        let at = |o: usize, p: &[u8]| head.get(o..o + p.len()) == Some(p);
        let text = String::from_utf8_lossy(head);
        let is_html = {
            let t = text.trim_start_matches('\u{feff}').trim_start().to_ascii_lowercase();
            t.starts_with("<!doctype html") || t.starts_with("<html")
        };
        if starts(b"\x89PNG\r\n\x1a\n") {
            Some(Format::Png)
        } else if starts(&[0xFF, 0xD8, 0xFF]) {
            Some(Format::Jpeg)
        } else if starts(b"GIF87a") || starts(b"GIF89a") {
            Some(Format::Gif)
        } else if starts(b"BM") {
            Some(Format::Bmp)
        } else if (starts(b"II*\0") && at(8, b"CR")) || starts(b"FUJIFILMCCD-RAW") || starts(b"IIRS") || starts(b"IIRO") || starts(b"MMOR") || starts(b"IIU\0") || (at(4, b"ftyp") && at(8, b"crx ")) {
            // Canon CR2 and CR3, Fujifilm RAF, Olympus ORF, Panasonic RW2.
            Some(Format::Raw)
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
        } else if is_html {
            Some(Format::Html)
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
        } else if starts(b"%PDF-") {
            Some(Format::Pdf)
        } else if starts(b"glTF") {
            Some(Format::Glb)
        } else if starts(b"ply\n") || starts(b"ply\r\n") {
            Some(Format::Ply)
        } else if starts(b"solid ") || starts(b"solid\n") || starts(b"solid\r") {
            Some(Format::Stl)
        } else if starts(b"PK\x03\x04") && at(30, b"mimetype") && at(38, b"application/epub+zip") {
            Some(Format::Epub)
        } else if at(60, b"BOOKMOBI") || at(60, b"TEXtREAd") {
            Some(Format::Mobi)
        } else if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") {
            Some(Format::Zip)
        } else if starts(&[0x1F, 0x8B]) {
            Some(Format::Gz)
        } else if starts(b"7z\xBC\xAF\x27\x1C") {
            Some(Format::SevenZ)
        } else if starts(b"wOFF") {
            Some(Format::Woff)
        } else if starts(b"wOF2") {
            Some(Format::Woff2)
        } else if starts(b"OTTO") {
            Some(Format::Otf)
        } else if starts(&[0, 1, 0, 0, 0]) || starts(b"true") {
            Some(Format::Ttf)
        } else if text.trim_start_matches('\u{feff}').starts_with("WEBVTT") {
            Some(Format::Vtt)
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
        assert_eq!(Format::sniff(b"wOF2\0\x01\0\0"), Some(Format::Woff2));
        assert_eq!(Format::sniff(b"OTTO\0\x0a"), Some(Format::Otf));
        assert_eq!(Format::sniff(b"\0\x01\0\0\0\x0a"), Some(Format::Ttf));
        assert_eq!(Format::sniff(b"\xef\xbb\xbfWEBVTT\n\n"), Some(Format::Vtt));
        assert_eq!(Format::sniff(b"PK\x03\x04rest"), Some(Format::Zip));
        assert_eq!(Format::sniff(&[0x1F, 0x8B, 8, 0]), Some(Format::Gz));
        assert_eq!(Format::sniff(b"7z\xBC\xAF\x27\x1C\0"), Some(Format::SevenZ));
        assert_eq!(Format::from_extension("folder"), None, "pseudo formats are never read from a name");
        assert_eq!(Format::sniff(b"%PDF-1.7\n"), Some(Format::Pdf));
        assert_eq!(Format::sniff(b"\xef\xbb\xbf  <!DOCTYPE HTML><html>"), Some(Format::Html));
        assert_eq!(Format::sniff(b"<html lang=en>"), Some(Format::Html));
        assert_eq!(Format::sniff(b"<svg xmlns="), Some(Format::Svg), "svg is not a web page");
        let mut epub = b"PK\x03\x04\x14\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x08\0\0\0mimetypeapplication/epub+zip".to_vec();
        epub.resize(64, 0);
        assert_eq!(Format::sniff(&epub), Some(Format::Epub), "an EPUB is a zip, but not just a zip");
        let mut mobi = vec![0u8; 60];
        mobi.extend_from_slice(b"BOOKMOBI");
        assert_eq!(Format::sniff(&mobi), Some(Format::Mobi));
        assert_eq!(Format::sniff(b"II*\0\x10\0\0\0CR\x02\0"), Some(Format::Raw), "Canon CR2");
        assert_eq!(Format::sniff(b"\0\0\0\x18ftypcrx \0\0\0\x01"), Some(Format::Raw), "Canon CR3, not an MP4");
        assert_eq!(Format::sniff(b"FUJIFILMCCD-RAW 0201"), Some(Format::Raw));
        assert_eq!(Format::sniff(b"IIRS\x08\0\0\0"), Some(Format::Raw));
        assert_eq!(Format::sniff(b"II*\0\x08\0\0\0\x19\0"), Some(Format::Tiff), "Nikon, Sony and DNG look like TIFF until the name says otherwise");
        assert_eq!(Format::from_extension("NEF"), Some(Format::Raw));
        assert_eq!(Format::sniff(b"glTF\x02\0\0\0"), Some(Format::Glb));
        assert_eq!(Format::sniff(b"ply\nformat ascii 1.0\n"), Some(Format::Ply));
        assert_eq!(Format::sniff(b"solid cube\nfacet normal"), Some(Format::Stl));
        assert_eq!(Format::sniff(b"nope"), None);
    }

    #[test]
    fn every_format_round_trips_its_extension() {
        for f in Format::ALL.into_iter().filter(|f| !f.is_pseudo()) {
            assert_eq!(Format::from_extension(f.extension()), Some(f), "{f}");
        }
    }
}
