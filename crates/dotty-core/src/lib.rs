//! UI-free conversion core: format registry, sniffing, presets, output
//! naming, and the job queue.

pub mod edit;
pub mod format;
pub mod jobs;
pub mod output;
pub mod registry;

pub use edit::{Caption, CaptionFont, Captions, Drawing, EditOp, Edits, Mask, Region, Stroke};
pub use format::{Format, Kind};
pub use jobs::{JobEvent, JobId, JobRequest, JobRunner};
pub use registry::{ConvertCtx, Converter, Registry};

use std::collections::BTreeMap;
use std::io::{Seek, Write};
use std::path::PathBuf;

/// An output sink encoders can write to and patch afterwards (headers that record lengths).
pub trait WriteSeek: Write + Seek {}
impl<T: Write + Seek + ?Sized> WriteSeek for T {}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unrecognized file type")]
    UnknownFormat,
    #[error("no converter from {from} to {to}")]
    NoRoute { from: Format, to: Format },
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Codec(String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Quality preset shared by all lossy encoders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    Low,
    Medium,
    #[default]
    High,
    Lossless,
}

impl Preset {
    pub const ALL: [Preset; 4] = [Preset::Low, Preset::Medium, Preset::High, Preset::Lossless];

    pub fn label(self) -> &'static str {
        match self {
            Preset::Low => "Low",
            Preset::Medium => "Medium",
            Preset::High => "High",
            Preset::Lossless => "Lossless",
        }
    }
}

/// A setting for one output format where the format can work in more than one way
/// (WebP lossy or lossless, WAV bit depth, …). Encoders declare these; the UI offers
/// them for every target format in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Choice {
    /// Stable id, unique within one format; used for storage.
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    /// `(id, label)` pairs; the first is the default.
    pub values: &'static [(&'static str, &'static str)],
    /// Only relevant when the source holds several images (animation frames or pages).
    pub multi_image_only: bool,
}

impl Choice {
    pub fn default_value(&self) -> &'static str {
        self.values[0].0
    }
}

/// User-facing conversion options.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub preset: Preset,
    /// Drop EXIF / audio tags instead of carrying them over.
    pub strip_metadata: bool,
    /// Write outputs here instead of next to the source.
    pub dest_dir: Option<PathBuf>,
    /// When an animation goes to a format that holds one frame, write every frame as its
    /// own file instead of keeping only the first.
    pub split_frames: bool,
    /// Picked values of format [`Choice`]s, by target format and choice key.
    pub choices: BTreeMap<(Format, String), String>,
}

impl Options {
    /// The value picked for `choice` when writing `format`, or its default.
    pub fn choice(&self, format: Format, choice: &Choice) -> &'static str {
        let picked = self.choices.get(&(format, choice.key.to_string()));
        choice
            .values
            .iter()
            .find(|(id, _)| picked.is_some_and(|p| p == id))
            .map_or(choice.default_value(), |(id, _)| id)
    }

    pub fn set_choice(&mut self, format: Format, choice: &Choice, value: &str) {
        self.choices.insert((format, choice.key.to_string()), value.to_string());
    }
}
