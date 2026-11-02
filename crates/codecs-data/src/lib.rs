//! Data files (JSON, YAML, TOML, XML, CSV) and subtitles (SRT, VTT, ASS): text formats
//! converted through a common tree or cue list. See [`data`] and [`subtitle`].

pub mod data;
pub mod subtitle;
pub mod text;

use dotty_core::{Choice, ConvertCtx, Converter, Error, Format, Registry, Result};
use serde_json::Value;
use std::path::Path;

pub fn register(registry: &mut Registry) {
    registry.register(Box::new(DataConverter));
    registry.register(Box::new(SubtitleConverter));
}

const DATA: [Format; 5] = [Format::Json, Format::Yaml, Format::Toml, Format::Xml, Format::Csv];
const SUBTITLES: [Format; 3] = [Format::Srt, Format::Vtt, Format::Ass];

pub const JSON_STYLE: Choice = Choice {
    key: "json_style",
    label: "JSON layout",
    help: "Indented for reading, or compact on one line",
    values: &[("pretty", "Indented"), ("compact", "Compact")],
    multi_image_only: false,
};

pub const CSV_DELIMITER: Choice = Choice {
    key: "csv_delimiter",
    label: "CSV separator",
    help: "What goes between cells",
    values: &[("comma", "Comma"), ("semicolon", "Semicolon"), ("tab", "Tab")],
    multi_image_only: false,
};

pub const CSV_TYPES: Choice = Choice {
    key: "csv_types",
    label: "CSV values",
    help: "Turn plain numbers and true/false into numbers and booleans, or keep every cell as text",
    values: &[("guess", "Numbers and booleans"), ("text", "All text")],
    multi_image_only: false,
};

pub struct DataConverter;

impl Converter for DataConverter {
    /// Includes `(f, f)`: tidying a file in its own format (indent, key layout).
    fn routes(&self) -> Vec<(Format, Format)> {
        DATA.iter().flat_map(|&a| DATA.iter().map(move |&b| (a, b))).collect()
    }

    fn choices(&self, to: Format) -> Vec<Choice> {
        match to {
            Format::Json => vec![JSON_STYLE],
            Format::Csv => vec![CSV_DELIMITER],
            _ => Vec::new(),
        }
    }

    fn source_choices(&self, from: Format) -> Vec<Choice> {
        if from == Format::Csv { vec![CSV_TYPES] } else { Vec::new() }
    }

    fn convert(&self, src: &Path, dst: &Path, from: Format, to: Format, ctx: &ConvertCtx) -> Result<()> {
        let text = text::read(src, ctx)?;
        (ctx.progress)(0.3);
        let value: Value = match from {
            Format::Json => data::parse_json(&text)?,
            Format::Yaml => data::parse_yaml(&text, ctx)?,
            Format::Toml => data::parse_toml(&text)?,
            Format::Xml => data::parse_xml(&text)?,
            Format::Csv => data::parse_csv(&text, ctx.options.choice(Format::Csv, &CSV_TYPES) == "guess")?,
            _ => return Err(Error::NoRoute { from, to }),
        };
        ctx.check_cancelled()?;
        let out = match to {
            Format::Json => data::write_json(&value, ctx.options.choice(Format::Json, &JSON_STYLE) == "pretty"),
            Format::Yaml => data::write_yaml(&value)?,
            Format::Toml => data::write_toml(&value, ctx)?,
            Format::Xml => data::write_xml(&value, ctx),
            Format::Csv => {
                let delim = match ctx.options.choice(Format::Csv, &CSV_DELIMITER) {
                    "semicolon" => b';',
                    "tab" => b'\t',
                    _ => b',',
                };
                data::write_csv(&value, delim, ctx)?
            }
            _ => return Err(Error::NoRoute { from, to }),
        };
        text::write(dst, &out)?;
        (ctx.progress)(1.0);
        Ok(())
    }
}

pub struct SubtitleConverter;

impl Converter for SubtitleConverter {
    fn routes(&self) -> Vec<(Format, Format)> {
        SUBTITLES.iter().flat_map(|&a| SUBTITLES.iter().filter(move |&&b| b != a).map(move |&b| (a, b))).collect()
    }

    fn convert(&self, src: &Path, dst: &Path, from: Format, to: Format, ctx: &ConvertCtx) -> Result<()> {
        let text = text::read(src, ctx)?;
        let cues = match from {
            Format::Srt => subtitle::parse_srt(&text)?,
            Format::Vtt => subtitle::parse_vtt(&text)?,
            Format::Ass => subtitle::parse_ass(&text, ctx)?,
            _ => return Err(Error::NoRoute { from, to }),
        };
        ctx.check_cancelled()?;
        let out = match to {
            Format::Srt => subtitle::write_srt(&cues),
            Format::Vtt => subtitle::write_vtt(&cues),
            Format::Ass => subtitle::write_ass(&cues),
            _ => return Err(Error::NoRoute { from, to }),
        };
        text::write(dst, &out)?;
        (ctx.progress)(1.0);
        Ok(())
    }
}
