use crate::{Choice, Edits, Error, Format, Options, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Passed to converters so they can report progress, honor cancellation, and attach
/// short human-readable notes about anything lossy or surprising that happened
/// ("only the first frame was converted").
pub struct ConvertCtx<'a> {
    pub options: &'a Options,
    pub cancel: &'a AtomicBool,
    pub progress: &'a dyn Fn(f32),
    /// Changes made in the editor, applied after decoding.
    pub edits: Option<&'a Edits>,
    notes: Mutex<Vec<String>>,
    output: Mutex<Option<PathBuf>>,
}

impl<'a> ConvertCtx<'a> {
    pub fn new(options: &'a Options, cancel: &'a AtomicBool, progress: &'a dyn Fn(f32)) -> Self {
        Self { options, cancel, progress, edits: None, notes: Mutex::new(Vec::new()), output: Mutex::new(None) }
    }

    pub fn with_edits(mut self, edits: Option<&'a Edits>) -> Self {
        self.edits = edits.filter(|e| !e.is_empty());
        self
    }

    pub fn check_cancelled(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Record a note for the user. Repeats are ignored.
    pub fn note(&self, msg: impl Into<String>) {
        let msg = msg.into();
        let mut notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        if !notes.contains(&msg) {
            notes.push(msg);
        }
    }

    pub fn take_notes(&self) -> Vec<String> {
        std::mem::take(&mut *self.notes.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Report that the result went somewhere other than the requested `dst` (e.g. one
    /// file per frame). A converter that does this cleans up after itself on failure.
    pub fn set_output(&self, path: PathBuf) {
        *self.output.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
    }

    pub fn take_output(&self) -> Option<PathBuf> {
        self.output.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Converts files between the formats it lists in `routes`.
///
/// Each media kind implements this once as a pipeline (decoder → kind-specific
/// intermediate → encoder), so any decodable format reaches any encodable one.
pub trait Converter: Send + Sync {
    fn routes(&self) -> Vec<(Format, Format)>;
    fn convert(&self, src: &Path, dst: &Path, from: Format, to: Format, ctx: &ConvertCtx) -> Result<()>;
    /// Settings offered for writing `to`.
    fn choices(&self, _to: Format) -> Vec<Choice> {
        Vec::new()
    }
    /// Settings offered for reading `from` (e.g. the size an SVG is rendered at).
    fn source_choices(&self, _from: Format) -> Vec<Choice> {
        Vec::new()
    }
    /// Quick check (headers only, where possible) whether `path` holds several images.
    fn multi_image(&self, _path: &Path, _from: Format) -> bool {
        false
    }
}

/// `(from, to)` pairs a converter handles.
type Routes = Vec<(Format, Format)>;

/// The registered converters, with their routes worked out once (the UI asks every frame).
#[derive(Default)]
pub struct Registry {
    converters: Vec<(Box<dyn Converter>, Routes)>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, c: Box<dyn Converter>) {
        let routes = c.routes();
        self.converters.push((c, routes));
    }

    pub fn find(&self, from: Format, to: Format) -> Option<&dyn Converter> {
        self.converters.iter().find(|(_, r)| r.contains(&(from, to))).map(|(c, _)| c.as_ref())
    }

    fn reading(&self, from: Format) -> Option<&dyn Converter> {
        self.converters.iter().find(|(_, r)| r.iter().any(|r| r.0 == from)).map(|(c, _)| c.as_ref())
    }

    /// Settings offered for writing `to`, from whichever converters can produce it.
    pub fn choices(&self, to: Format) -> Vec<Choice> {
        let mut out: Vec<Choice> = Vec::new();
        for (c, routes) in &self.converters {
            if routes.iter().any(|r| r.1 == to) {
                for choice in c.choices(to) {
                    if !out.iter().any(|o| o.key == choice.key) {
                        out.push(choice);
                    }
                }
            }
        }
        out
    }

    /// Whether the file holds several images (animation frames, pages, icon sizes).
    pub fn multi_image(&self, path: &Path, from: Format) -> bool {
        from.multi_image_input() && self.reading(from).is_some_and(|c| c.multi_image(path, from))
    }

    /// Settings offered for reading `from`.
    pub fn source_choices(&self, from: Format) -> Vec<Choice> {
        self.reading(from).map(|c| c.source_choices(from)).unwrap_or_default()
    }

    /// Targets reachable from `from`, in canonical order, excluding itself.
    pub fn targets(&self, from: Format) -> Vec<Format> {
        Format::ALL
            .into_iter()
            .filter(|&to| to != from && self.find(from, to).is_some())
            .collect()
    }

    /// Targets for an edited file: re-saving in its own format is useful too, so it comes first.
    pub fn edited_targets(&self, from: Format) -> Vec<Format> {
        let same = self.find(from, from).map(|_| from);
        same.into_iter().chain(self.targets(from)).collect()
    }
}
