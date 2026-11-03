// No console window behind the app on Windows (debug builds keep it for logs).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio_editor;
mod crop_box;
mod editor;
mod frame_picker;
mod model_viewer;
mod player;
mod range_bar;
mod video_editor;
mod video_worker;

use audio_editor::AudioEditor;
use editor::{Editor, Outcome};
use frame_picker::FramePicker;
use model_viewer::ModelViewer;
use video_editor::VideoEditor;
use eframe::egui;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use dotty_core::{
    BundleMode, Choice, Edits, Format, JobEvent, JobId, JobRequest, JobRunner, Kind, Options, Preset, Registry,
};

const KEY_PRESET: &str = "preset";
const KEY_STRIP: &str = "strip_metadata";
const KEY_DEST: &str = "dest_dir";
const KEY_SPLIT: &str = "split_frames";
const KEY_BUNDLE: &str = "bundle";
const KEY_CHOICES: &str = "format_choices";

/// Format choices as `ext:key=value;…` for storage.
fn choices_to_string(options: &Options) -> String {
    options.choices.iter().map(|((f, k), v)| format!("{}:{k}={v}", f.extension())).collect::<Vec<_>>().join(";")
}

fn choices_from_string(s: &str, options: &mut Options) {
    for item in s.split(';') {
        let Some((ext, rest)) = item.split_once(':') else { continue };
        let Some((key, value)) = rest.split_once('=') else { continue };
        if let Some(f) = Format::ALL.into_iter().find(|f| f.extension() == ext) {
            options.choices.insert((f, key.to_string()), value.to_string());
        }
    }
}

#[derive(Clone, PartialEq)]
enum Status {
    Ready,
    Queued,
    Running(f32),
    Done(PathBuf, Vec<String>),
    Failed(String),
    Cancelled,
}

impl Status {
    fn busy(&self) -> bool {
        matches!(self, Status::Queued | Status::Running(_))
    }
    /// Whether pressing Convert should (re)run this entry.
    fn pending(&self) -> bool {
        matches!(self, Status::Ready | Status::Failed(_) | Status::Cancelled)
    }
}

struct Entry {
    id: JobId,
    path: PathBuf,
    from: Format,
    /// The file holds several images (animation frames, pages, icon sizes).
    multi: bool,
    to: Option<Format>,
    status: Status,
    /// Changes made in the editor, applied when converting.
    edits: Option<Arc<Edits>>,
    /// What the edited image (or audio waveform) looks like, for the row.
    preview: Option<egui::TextureHandle>,
    /// Length of the edited audio, in seconds.
    edited_length: Option<f64>,
}

impl Entry {
    fn editable(&self) -> bool {
        self.tool().is_some()
    }

    /// Which editor "Edit" opens for this entry's source and target.
    fn tool(&self) -> Option<Tool> {
        let to = self.to?;
        match (self.from.kind(), to.kind()) {
            (Kind::Video, Kind::Audio) | (Kind::Audio, _) => Some(Tool::Audio),
            (Kind::Video, Kind::Image) => Some(Tool::Frames),
            (Kind::Video, Kind::Video) | (Kind::Image, Kind::Video) => Some(Tool::Video),
            (Kind::Image, _) => Some(Tool::Image),
            (Kind::Model, _) => Some(Tool::Model),
            _ => None,
        }
    }

    /// The picture chosen for a song's video, if any.
    fn picture_file(&self) -> Option<&PathBuf> {
        self.edits.as_ref()?.video()?.picture_file.as_ref()
    }

    /// Whether the edits made parts of the image transparent.
    fn erased(&self) -> bool {
        self.edits.as_ref().is_some_and(|e| e.erases())
    }
}

struct App {
    registry: Arc<Registry>,
    runner: JobRunner,
    entries: Vec<Entry>,
    next_id: JobId,
    options: Options,
    notice: Option<String>,
    editor: Option<ActiveEditor>,
    /// Shows transparency behind edited images.
    checker: egui::TextureHandle,
    /// Whether each newly added file holds several images, found on a background thread
    /// (it means decoding a little of each file, which would stall the window for big drops).
    probes: (mpsc::Sender<Probe>, mpsc::Receiver<Probe>),
    ctx: egui::Context,
    /// Entries waiting for the answer to "folder or zip?" (shown as a prompt).
    ask_bundle: Option<Vec<JobId>>,
    remember_bundle: bool,
}

/// A file found to hold several images.
type Probe = (JobId, bool);

/// The editors "Edit" can open.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Tool {
    Image,
    Audio,
    /// Picking a video's frame(s) for pictures.
    Frames,
    Video,
    /// Looking at a 3D model (nothing to edit).
    Model,
}

enum ActiveEditor {
    Image(Editor),
    Audio(AudioEditor),
    /// Picking a video's frame(s) for an image target.
    Frames(Box<FramePicker>),
    /// Editing a video (or a picture that becomes one).
    Video(Box<VideoEditor>),
    /// Editing the frames picked from a video; the pick is saved with the picture edits.
    VideoPicture(Editor, dotty_core::FrameSelection),
    /// The 3D model viewer.
    Model(Box<ModelViewer>),
}

/// What an editor produced, plus the video edits already on the entry (a song's chosen
/// picture lives there while the audio editor works on the sound).
fn with_video_edits(existing: Option<&Edits>, edited: Option<Edits>) -> Option<Edits> {
    let keep: Vec<dotty_core::EditOp> = existing.map(|e| e.ops.iter().filter(|op| matches!(op, dotty_core::EditOp::Video(_))).cloned().collect()).unwrap_or_default();
    match edited {
        Some(mut e) => {
            e.ops.retain(|op| !matches!(op, dotty_core::EditOp::Video(_)));
            e.ops.extend(keep);
            Some(e)
        }
        None => (!keep.is_empty()).then_some(Edits { ops: keep }),
    }
}

fn bundle_key(b: Option<BundleMode>) -> &'static str {
    match b {
        Some(BundleMode::Folder) => "folder",
        Some(BundleMode::Zip) => "zip",
        None => "",
    }
}

fn preset_from_str(s: &str) -> Preset {
    Preset::ALL.into_iter().find(|p| p.label() == s).unwrap_or_default()
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut options = Options::default();
        if let Some(storage) = cc.storage {
            if let Some(p) = storage.get_string(KEY_PRESET) {
                options.preset = preset_from_str(&p);
            }
            options.strip_metadata = storage.get_string(KEY_STRIP).is_some_and(|s| s == "true");
            options.split_frames = storage.get_string(KEY_SPLIT).is_some_and(|s| s == "true");
            options.bundle = match storage.get_string(KEY_BUNDLE).as_deref() {
                Some("folder") => Some(BundleMode::Folder),
                Some("zip") => Some(BundleMode::Zip),
                _ => None,
            };
            if let Some(s) = storage.get_string(KEY_CHOICES) {
                choices_from_string(&s, &mut options);
            }
            options.dest_dir = storage.get_string(KEY_DEST).filter(|d| !d.is_empty()).map(PathBuf::from).filter(|d| d.is_dir());
        }
        let mut app = Self::build(cc.egui_ctx.clone(), options);
        // Files passed on the command line (or via "Open with") are added straight away.
        app.add_files(std::env::args_os().skip(1).map(PathBuf::from));
        app
    }

    fn build(ctx: egui::Context, options: Options) -> Self {
        let mut registry = Registry::new();
        codecs_image::register(&mut registry);
        codecs_audio::register(&mut registry);
        codecs_video::register(&mut registry);
        codecs_data::register(&mut registry);
        codecs_font::register(&mut registry);
        codecs_archive::register(&mut registry);
        codecs_doc::register(&mut registry);
        codecs_model::register(&mut registry);
        let registry = Arc::new(registry);

        // Large files are memory-hungry (some encoders hold the whole stream), so don't
        // run one job per core.
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        let checker = editor::checker_texture(&ctx);
        let wake = ctx.clone();
        let runner = JobRunner::new(registry.clone(), (cores / 2).clamp(1, 8), move || wake.request_repaint());
        let probes = mpsc::channel();
        Self { registry, runner, entries: Vec::new(), next_id: 0, options, notice: None, editor: None, checker, probes, ctx, ask_bundle: None, remember_bundle: false }
    }

    fn add_files(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        let mut skipped = 0;
        let mut probe = Vec::new();
        for path in paths {
            if self.entries.iter().any(|e| e.path == path) {
                continue;
            }
            let Some(from) = Format::detect(&path) else {
                skipped += 1;
                continue;
            };
            let to = self.default_target(from);
            self.next_id += 1;
            if from.multi_image_input() {
                probe.push((self.next_id, path.clone(), from));
            }
            self.entries.push(Entry { id: self.next_id, path, from, multi: false, to, status: Status::Ready, edits: None, preview: None, edited_length: None });
        }
        if !probe.is_empty() {
            let (registry, tx, ctx) = (self.registry.clone(), self.probes.0.clone(), self.ctx.clone());
            std::thread::spawn(move || {
                for (id, path, from) in probe {
                    if registry.multi_image(&path, from) && tx.send((id, true)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            });
        }
        self.notice = (skipped > 0).then(|| format!("{skipped} file(s) skipped: unsupported type"));
    }

    /// The target a newly added file starts with: the first reachable format, except that a
    /// video most often becomes another video (WebM plays everywhere; MP4 for WebM files).
    fn default_target(&self, from: Format) -> Option<Format> {
        let targets = self.registry.targets(from);
        if from.kind() == Kind::Video
            && let Some(t) = [Format::Webm, Format::Mp4, Format::Mkv].into_iter().find(|t| targets.contains(t))
        {
            return Some(t);
        }
        // 3D printing wants STL; everything else is happy with GLB.
        if from.kind() == Kind::Model {
            let pick = if from == Format::Stl { Format::Glb } else { Format::Stl };
            if targets.contains(&pick) {
                return Some(pick);
            }
        }
        // A PDF is what people mostly want from a document source.
        if from.kind() == Kind::Document && targets.contains(&Format::Pdf) {
            return Some(Format::Pdf);
        }
        // Opening an archive is what people mostly want from one.
        if from.kind() == Kind::Archive && targets.contains(&Format::Folder) {
            return Some(Format::Folder);
        }
        targets.into_iter().next()
    }

    fn pump_events(&mut self) {
        while let Ok((id, multi)) = self.probes.1.try_recv() {
            if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                e.multi = multi;
            }
        }
        while let Ok(ev) = self.runner.events().try_recv() {
            let (id, status) = match ev {
                JobEvent::Started(id) => (id, Status::Running(0.0)),
                JobEvent::Progress(id, p) => (id, Status::Running(p)),
                JobEvent::Done(id, p, notes) => (id, Status::Done(p, notes)),
                JobEvent::Failed(id, m) => (id, Status::Failed(m)),
                JobEvent::Cancelled(id) => (id, Status::Cancelled),
            };
            if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                e.status = status;
            }
        }
    }

    fn pending_count(&self) -> usize {
        self.entries.iter().filter(|e| e.to.is_some() && e.status.pending()).count()
    }

    fn busy(&self) -> bool {
        self.entries.iter().any(|e| e.status.busy())
    }

    fn convert_all(&mut self) {
        let all = self.entries.iter().map(|e| e.id).collect();
        self.start(all);
    }

    /// Whether converting this entry would make many files (a folder or a zip of them).
    fn makes_many(&self, e: &Entry) -> bool {
        e.status.pending() && e.to.is_some_and(|to| self.registry.many_files(&e.path, e.from, to, &self.options))
    }

    /// Queue the entries with these ids; first asks "folder or zip?" when any of them makes
    /// many files and no standing answer is set.
    fn start(&mut self, ids: Vec<JobId>) {
        self.start_with(ids, None);
    }

    fn start_with(&mut self, ids: Vec<JobId>, answer: Option<BundleMode>) {
        let asks = self.options.bundle.is_none() && answer.is_none() && self.entries.iter().any(|e| ids.contains(&e.id) && self.makes_many(e));
        if asks {
            self.ask_bundle = Some(ids);
            return;
        }
        for i in 0..self.entries.len() {
            if ids.contains(&self.entries[i].id) {
                self.convert(i, answer);
            }
        }
    }

    /// Queue one entry if it has a target and isn't already done or in flight.
    fn convert(&mut self, i: usize, answer: Option<BundleMode>) {
        let e = &mut self.entries[i];
        let Some(to) = e.to else { return };
        if !e.status.pending() {
            return;
        }
        e.status = Status::Queued;
        let edits = e.edits.clone();
        let mut options = self.options.clone();
        options.bundle = options.bundle.or(answer);
        self.runner.submit(JobRequest { id: e.id, src: e.path.clone(), from: e.from, to, options, edits });
    }

    /// Targets offered for an entry: its own format too once it's edited.
    fn targets(&self, e: &Entry) -> Vec<Format> {
        if e.edits.is_some() { self.registry.edited_targets(e.from) } else { self.registry.targets(e.from) }
    }

    fn open_editor(&mut self, ctx: &egui::Context, i: usize) {
        let e = &self.entries[i];
        self.editor = Some(match e.tool() {
            Some(Tool::Model) => ActiveEditor::Model(Box::new(ModelViewer::open(ctx, e.path.clone(), e.from))),
            Some(Tool::Video) => ActiveEditor::Video(Box::new(VideoEditor::open(ctx, e.id, e.path.clone(), e.from, e.edits.clone()))),
            Some(Tool::Audio) => {
                // Normalizing in the editor measures the layout the target will be written in.
                let output_channels = e.to.and_then(|to| match self.options.choice(to, &codecs_audio::CHANNELS) {
                    "mono" => Some(1),
                    "stereo" => Some(2),
                    _ => None,
                });
                ActiveEditor::Audio(AudioEditor::open(ctx, e.id, e.path.clone(), e.from, e.edits.clone(), output_channels))
            }
            Some(Tool::Frames) => {
                let to = e.to.unwrap_or(Format::Png);
                ActiveEditor::Frames(Box::new(FramePicker::open(ctx, e.id, e.path.clone(), e.from, to, self.options.clone(), e.edits.clone())))
            }
            _ => ActiveEditor::Image(Editor::open(ctx, e.id, e.path.clone(), e.from, self.options.clone(), e.edits.clone())),
        });
    }

    /// Store what the editor produced (`None` clears the edits).
    fn set_edits(&mut self, ctx: &egui::Context, id: JobId, edits: Option<Edits>, thumb: Option<egui::ColorImage>) {
        let Some(i) = self.entries.iter().position(|e| e.id == id) else { return };
        let same_format = self.registry.find(self.entries[i].from, self.entries[i].from).is_some();
        let e = &mut self.entries[i];
        let before = e.edits.clone();
        match edits {
            Some(edits) => {
                e.edits = Some(Arc::new(edits));
                e.preview = thumb.map(|t| ctx.load_texture(format!("preview-{id}"), t, egui::TextureOptions::LINEAR));
                // Removing a background is pointless in a format that can't store it.
                if e.erased() && !e.to.is_some_and(Format::keeps_alpha) {
                    e.to = Some(if e.from.keeps_alpha() && same_format { e.from } else { Format::Png });
                }
            }
            None => {
                e.edits = None;
                e.preview = None;
                e.edited_length = None;
                if e.to == Some(e.from) {
                    e.to = self.registry.targets(e.from).into_iter().next();
                }
            }
        }
        if e.edits != before && !e.status.busy() {
            e.status = Status::Ready;
        }
    }

    /// "Folder or zip?" for results made of many files.
    fn bundle_prompt(&mut self, ctx: &egui::Context) {
        if self.ask_bundle.is_none() {
            return;
        }
        let mut pick = None;
        let mut cancel = false;
        egui::Modal::new(egui::Id::unique("bundle-prompt")).show(ctx, |ui| {
            ui.set_width(340.0);
            ui.heading("Many files");
            ui.label("This makes one file for each page or frame. Put them in:");
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                for m in [BundleMode::Folder, BundleMode::Zip] {
                    if ui.button(m.label()).clicked() {
                        pick = Some(m);
                    }
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
            ui.checkbox(&mut self.remember_bundle, "Remember my choice").on_hover_text("Change it later with \"Many files\" below the list");
        });
        if cancel || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.ask_bundle = None;
        } else if let Some(mode) = pick {
            if self.remember_bundle {
                self.options.bundle = Some(mode);
            }
            if let Some(ids) = self.ask_bundle.take() {
                self.start_with(ids, Some(mode));
            }
        }
    }

    fn show_editor(&mut self, ctx: &egui::Context) {
        match &mut self.editor {
            None => {}
            Some(ActiveEditor::Video(editor)) => match editor.show(ctx) {
                video_editor::Outcome::Open => {}
                video_editor::Outcome::Cancelled => self.editor = None,
                video_editor::Outcome::Done { edits, thumb } => {
                    let id = editor.entry;
                    self.editor = None;
                    // (A song's sound edits are carried through the editor, which shows its range.)
                    self.set_edits(ctx, id, edits, thumb);
                }
            },
            Some(ActiveEditor::Model(viewer)) => {
                if matches!(viewer.show(ctx), model_viewer::Outcome::Closed) {
                    self.editor = None;
                }
            }
            Some(ActiveEditor::Frames(picker)) => match picker.show(ctx) {
                frame_picker::Outcome::Open => {}
                frame_picker::Outcome::Cancelled => self.editor = None,
                frame_picker::Outcome::Done { edits, thumb } => {
                    let id = picker.entry;
                    self.editor = None;
                    self.set_edits(ctx, id, edits, thumb);
                }
                frame_picker::Outcome::EditPicture { sel } => {
                    let id = picker.entry;
                    let Some(e) = self.entries.iter().find(|e| e.id == id) else { return };
                    let (path, from, to, options) = (e.path.clone(), e.from, e.to.unwrap_or(Format::Png), self.options.clone());
                    let title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let editor = Editor::open_with(ctx, id, title, e.edits.clone(), move || frame_picker::load_for_editor(path, from, to, sel, options));
                    self.editor = Some(ActiveEditor::VideoPicture(editor, sel));
                }
            },
            Some(ActiveEditor::VideoPicture(editor, sel)) => match editor.show(ctx, &self.checker) {
                Outcome::Open => {}
                Outcome::Cancelled => self.editor = None,
                Outcome::Done { edits, thumb } => {
                    let (id, sel) = (editor.entry, *sel);
                    self.editor = None;
                    // The pick, then the picture edits that apply to the picked frames.
                    let mut ops = vec![dotty_core::EditOp::Frames(sel)];
                    ops.extend(edits.map(|e| e.ops).unwrap_or_default());
                    self.set_edits(ctx, id, Some(Edits { ops }), thumb);
                }
            },
            Some(ActiveEditor::Image(editor)) => match editor.show(ctx, &self.checker) {
                Outcome::Open => {}
                Outcome::Cancelled => self.editor = None,
                Outcome::Done { edits, thumb } => {
                    let id = editor.entry;
                    self.editor = None;
                    self.set_edits(ctx, id, edits, thumb);
                }
            },
            Some(ActiveEditor::Audio(editor)) => match editor.show(ctx) {
                audio_editor::Outcome::Open => {}
                audio_editor::Outcome::Cancelled => self.editor = None,
                audio_editor::Outcome::Done { edits, thumb, length } => {
                    let id = editor.entry;
                    self.editor = None;
                    let edited = edits.is_some();
                    let existing = self.entries.iter().find(|e| e.id == id).and_then(|e| e.edits.clone());
                    let edits = with_video_edits(existing.as_deref(), edits);
                    self.set_edits(ctx, id, edits, thumb);
                    if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                        e.edited_length = edited.then_some(length);
                    }
                }
            },
        }
    }

    /// Does the quality preset change the output of anything about to be converted?
    fn preset_matters(&self) -> bool {
        self.entries.is_empty() || self.entries.iter().any(|e| e.to.is_some_and(Format::uses_quality))
    }

    /// Could a file in the list hold several images that its target can't keep together?
    fn many_files_matter(&self) -> bool {
        self.entries.iter().any(|e| e.status.pending() && e.to.is_some_and(|to| self.registry.many_files(&e.path, e.from, to, &self.options)))
    }

    fn frames_matter(&self) -> bool {
        self.entries
            .iter()
            .any(|e| e.multi && e.to.is_some_and(|t| !Format::keeps_all_images(e.from, t)))
    }

    /// Settings worth showing: `(format, is_source, choices)` for every output format in use
    /// and every input format that has reading settings.
    fn relevant_choices(&self) -> Vec<(Format, bool, Vec<Choice>)> {
        let mut out = Vec::new();
        for f in Format::ALL {
            let targeting: Vec<&Entry> = self.entries.iter().filter(|e| e.to == Some(f)).collect();
            let multi = targeting.iter().any(|e| e.multi);
            let sources: Vec<Format> = targeting.iter().map(|e| e.from).collect();
            let target: Vec<Choice> =
                self.registry.choices_from(f, &sources).into_iter().filter(|c| !targeting.is_empty() && (multi || !c.multi_image_only)).collect();
            if !target.is_empty() {
                out.push((f, false, target));
            }
            if self.entries.iter().any(|e| e.from == f && e.to.is_some()) {
                let source = self.registry.source_choices(f);
                if !source.is_empty() {
                    out.push((f, true, source));
                }
            }
        }
        out
    }

    /// One row per format with settings; changing one makes finished files convertible again.
    fn format_settings(&mut self, ui: &mut egui::Ui) {
        for (format, is_source, choices) in self.relevant_choices() {
            ui.horizontal_wrapped(|ui| {
                ui.strong(if is_source { format!("{format} input:") } else { format!("{format} output:") });
                for choice in choices {
                    let current = self.options.choice(format, &choice);
                    let label = choice.values.iter().find(|v| v.0 == current).map_or(current, |v| v.1);
                    ui.label(choice.label).on_hover_text(choice.help);
                    let mut picked = None;
                    egui::ComboBox::from_id_salt(("choice", format, choice.key, is_source))
                        .selected_text(label)
                        .show_ui(ui, |ui| {
                            for (id, text) in choice.values {
                                if ui.selectable_label(*id == current, *text).clicked() {
                                    picked = Some(*id);
                                }
                            }
                        })
                        .response
                        .on_hover_text(choice.help);
                    if let Some(id) = picked.filter(|id| *id != current) {
                        self.options.set_choice(format, &choice, id);
                        for e in &mut self.entries {
                            let affected = if is_source { e.from == format } else { e.to == Some(format) };
                            if affected && matches!(e.status, Status::Done(..)) {
                                e.status = Status::Ready;
                            }
                        }
                    }
                }
            });
        }
    }

    fn drop_zone(&mut self, ui: &mut egui::Ui) {
        let hovering = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
        let height = if self.entries.is_empty() { 120.0 } else { 64.0 };
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::click());
        let visuals = ui.visuals();
        let color = if hovering {
            visuals.selection.stroke.color
        } else {
            visuals.widgets.noninteractive.fg_stroke.color
        };
        if hovering {
            ui.painter().rect_filled(rect.shrink(2.0), 8.0, visuals.selection.bg_fill.gamma_multiply(0.35));
        }
        ui.painter().rect_stroke(
            rect.shrink(2.0),
            8.0,
            egui::Stroke::new(if hovering { 3.0 } else { 1.5 }, color),
            egui::StrokeKind::Inside,
        );
        let label = if hovering { "Release to add files" } else { "Drop files here, or click to browse" };
        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, label, egui::FontId::proportional(18.0), color);

        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
            && let Some(files) = rfd::FileDialog::new().set_title("Select files to convert").pick_files()
        {
            self.add_files(files);
        }
    }

    fn supported_formats(&self, ui: &mut egui::Ui) {
        for kind in Kind::ALL {
            let name = kind.title();
            let list: Vec<String> = Format::ALL
                .into_iter()
                .filter(|f| f.kind() == kind && !self.registry.targets(*f).is_empty())
                .map(|f| f.to_string())
                .collect();
            if !list.is_empty() {
                ui.weak(format!("{name}: {}", list.join(", ")));
            }
        }
    }

    /// "All images → [format]" shortcuts for every kind present in the list.
    fn bulk_targets(&mut self, ui: &mut egui::Ui) {
        for kind in Kind::ALL {
            let label = kind.plural();
            let members: Vec<usize> = (0..self.entries.len())
                .filter(|&i| self.entries[i].from.kind() == kind && !self.entries[i].status.busy())
                .collect();
            if members.len() < 2 {
                continue;
            }
            // Targets that every selected file can reach.
            let common: Vec<Format> = Format::ALL
                .into_iter()
                .filter(|&t| members.iter().all(|&i| self.registry.targets(self.entries[i].from).contains(&t)))
                .collect();
            if common.is_empty() {
                continue;
            }
            let current = members.iter().map(|&i| self.entries[i].to).reduce(|a, b| if a == b { a } else { None }).flatten();
            let mut chosen = None;
            ui.horizontal(|ui| {
                ui.label(format!("All {label} →"));
                egui::ComboBox::from_id_salt(("bulk", label))
                    .selected_text(current.map_or("mixed".to_string(), |t| t.to_string()))
                    .width(80.0)
                    .show_ui(ui, |ui| {
                        for t in &common {
                            if ui.selectable_label(current == Some(*t), t.to_string()).clicked() {
                                chosen = Some(*t);
                            }
                        }
                    });
            });
            if let Some(t) = chosen {
                for &i in &members {
                    let e = &mut self.entries[i];
                    if e.to != Some(t) {
                        e.to = Some(t);
                        if !e.status.busy() {
                            e.status = Status::Ready;
                        }
                    }
                }
            }
        }
    }

    fn file_list(&mut self, ui: &mut egui::Ui) {
        let mut remove = None;
        let mut go = None;
        let mut edit = None;
        let mut discard = None;
        let mut edit_picture = None;
        let targets: Vec<Vec<Format>> = self.entries.iter().map(|e| self.targets(e)).collect();
        let checker = &self.checker;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (i, e) in self.entries.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    let name = e.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    ui.add_sized([240.0, 20.0], egui::Label::new(name).truncate()).on_hover_text(e.path.display().to_string());
                    ui.label(e.from.to_string());
                    ui.label("→");

                    let targets = &targets[i];
                    let editable = !e.status.busy();
                    let before = e.to;
                    ui.add_enabled_ui(editable && !targets.is_empty(), |ui| {
                        let text = e.to.map_or("n/a".to_string(), |t| t.to_string());
                        egui::ComboBox::from_id_salt(e.id).selected_text(text).width(72.0).show_ui(ui, |ui| {
                            for &t in targets {
                                let label = if t == e.from { format!("{t} (same)") } else { t.to_string() };
                                ui.selectable_value(&mut e.to, Some(t), label);
                            }
                        });
                    });
                    if e.to != before && editable {
                        e.status = Status::Ready; // a new target means convert again
                        // A video's soundtrack edits mean nothing for an image target and the
                        // other way round: drop them when the kind of target changes.
                        if e.from.kind() == Kind::Video && before.map(Format::kind) != e.to.map(Format::kind) {
                            (e.edits, e.preview, e.edited_length) = (None, None, None);
                        }
                    }
                    if e.editable() {
                        let tool = e.tool();
                        let audio = tool == Some(Tool::Audio);
                        let video = e.from.kind() == Kind::Video;
                        let backing = (!audio).then_some(checker);
                        if let Some(tex) = &e.preview {
                            let side = if audio { 44.0 } else { 22.0 };
                            let resp = thumbnail(ui, backing, tex, side)
                                .on_hover_ui(|ui| {
                                    thumbnail(ui, backing, tex, if audio { audio_editor::THUMB[0] as f32 } else { editor::THUMB_SIDE as f32 });
                                    if let Some(len) = e.edited_length {
                                        ui.label(format!("Length after editing: {}", audio_editor::fmt_time(len)));
                                    }
                                    ui.weak("Right-click to discard the edits");
                                });
                            resp.context_menu(|ui| {
                                if ui.add_enabled(editable, egui::Button::new("Discard edits")).clicked() {
                                    discard = Some(e.id);
                                }
                            });
                        }
                        let model = tool == Some(Tool::Model);
                        let label = if model { "Preview" } else if e.edits.is_some() { "Edit •" } else { "Edit" };
                        let help = if model {
                            "Look at the model: drag to turn it, scroll to zoom"
                        } else if tool == Some(Tool::Video) {
                            "Trim, crop, rotate, change speed, fade, add text or change the sound before converting"
                        } else if video && !audio {
                            "Pick the frame (or the stretch for an animation), then edit the picture if you like"
                        } else if video {
                            "Edit the soundtrack before converting: trim, fade, change speed or pitch, filter, add effects or change the volume"
                        } else if audio {
                            "Trim, fade, change speed or pitch, filter, reverse, add echo or reverb, or change the volume before converting"
                        } else {
                            "Remove the background, draw, add text or crop before converting"
                        };
                        if ui.add_enabled(editable, egui::Button::new(label)).on_hover_text(help).clicked() {
                            edit = Some(i);
                        }
                        // A song becoming a video: its picture, text, fades (its sound is "Edit").
                        if e.from.kind() == Kind::Audio && e.to.is_some_and(|t| t.kind() == Kind::Video) {
                            let chosen = e.picture_file().is_some() || e.edits.as_ref().and_then(|x| x.video()).is_some();
                            let label = if chosen { "🎬 Picture •" } else { "🎬 Picture" };
                            let tip = "Choose the picture shown with the song (else its cover art), and add text, fades or a crop";
                            if ui.add_enabled(editable, egui::Button::new(label)).on_hover_text(tip).clicked() {
                                edit_picture = Some(i);
                            }
                        }
                    }
                    if e.to.is_some() && e.status.pending() && ui.button("Go").on_hover_text("Convert this file").clicked() {
                        go = Some(i);
                    }

                    // Visible but never blocking: no confirmation dialog.
                    if let Some(to) = e.to
                        && e.from.is_lossy()
                        && to.is_lossy()
                    {
                        ui.colored_label(ui.visuals().warn_fg_color, "⚠ lossy → lossy")
                            .on_hover_text("Both formats are lossy, so re-encoding discards more detail. A lossless target avoids this.");
                    }
                    if e.erased() && e.to.is_some_and(|t| !t.keeps_alpha()) {
                        ui.colored_label(ui.visuals().warn_fg_color, "⚠ no transparency")
                            .on_hover_text("This format can't store transparency, so erased areas will be filled with white. PNG or WebP keeps them.");
                    }

                    status_widgets(ui, &e.status);

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if editable && ui.small_button("✕").on_hover_text("Remove from list").clicked() {
                            remove = Some(i);
                        }
                    });
                });
            }
        });
        if let Some(i) = go {
            let id = self.entries[i].id;
            self.start(vec![id]);
        }
        if let Some(i) = edit {
            self.open_editor(ui.ctx(), i);
        }
        if let Some(id) = discard {
            self.set_edits(ui.ctx(), id, None, None);
        }
        if let Some(i) = edit_picture {
            let e = &self.entries[i];
            self.editor = Some(ActiveEditor::Video(Box::new(VideoEditor::open(ui.ctx(), e.id, e.path.clone(), e.from, e.edits.clone()))));
        }
        if let Some(i) = remove {
            self.entries.remove(i);
        }
    }

    fn summary(&self) -> String {
        let count = |f: fn(&Status) -> bool| self.entries.iter().filter(|e| f(&e.status)).count();
        let done = count(|s| matches!(s, Status::Done(..)));
        let failed = count(|s| matches!(s, Status::Failed(_)));
        let pending = self.pending_count();
        let mut parts = Vec::new();
        if done > 0 {
            parts.push(format!("{done} done"));
        }
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        if pending > 0 && done + failed > 0 {
            parts.push(format!("{pending} waiting"));
        }
        parts.join(" · ")
    }
}

/// An image over the transparency checkerboard, scaled to `side` points on its longest side.
/// `checker` goes behind images that may be transparent; audio waveforms go on the row itself.
fn thumbnail(ui: &mut egui::Ui, checker: Option<&egui::TextureHandle>, tex: &egui::TextureHandle, side: f32) -> egui::Response {
    let size = tex.size_vec2();
    let size = size * (side / size.x.max(size.y));
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if let Some(checker) = checker {
        editor::paint_checker(ui.painter(), checker, rect);
    }
    ui.painter().image(tex.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
    resp
}

fn status_widgets(ui: &mut egui::Ui, status: &Status) {
    match status {
        Status::Ready => {}
        Status::Queued => {
            ui.weak("queued");
        }
        Status::Running(p) => {
            ui.add(egui::ProgressBar::new(*p).desired_width(120.0));
        }
        Status::Done(out, notes) => {
            ui.colored_label(egui::Color32::from_rgb(80, 180, 100), "✔ done");
            if ui.small_button("show").on_hover_text(out.display().to_string()).clicked() {
                reveal(out);
            }
            if let Some(first) = notes.first() {
                let extra = if notes.len() > 1 { format!(" (+{})", notes.len() - 1) } else { String::new() };
                ui.add(egui::Label::new(egui::RichText::new(format!("ℹ {first}{extra}")).weak()).truncate())
                    .on_hover_text(notes.join("\n"));
            }
        }
        Status::Failed(m) => {
            ui.add(egui::Label::new(egui::RichText::new(format!("✖ {m}")).color(ui.visuals().error_fg_color)).truncate())
                .on_hover_text(m);
        }
        Status::Cancelled => {
            ui.weak("cancelled");
        }
    }
}

fn reveal(path: &std::path::Path) {
    let dir = path.parent().unwrap_or(path);
    let _ = open_dir(dir);
}

#[cfg(target_os = "windows")]
fn open_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::process::Command::new("explorer").arg(dir).spawn().map(|_| ())
}
#[cfg(not(target_os = "windows"))]
fn open_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::process::Command::new("xdg-open").arg(dir).spawn().map(|_| ())
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_events();
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if !dropped.is_empty() {
            self.add_files(dropped);
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        storage.set_string(KEY_PRESET, self.options.preset.label().to_string());
        storage.set_string(KEY_STRIP, self.options.strip_metadata.to_string());
        storage.set_string(KEY_SPLIT, self.options.split_frames.to_string());
        storage.set_string(KEY_BUNDLE, bundle_key(self.options.bundle).to_string());
        storage.set_string(KEY_CHOICES, choices_to_string(&self.options));
        storage.set_string(KEY_DEST, self.options.dest_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default());
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::bottom("controls").show(ui, |ui| {
            ui.add_space(6.0);
            self.format_settings(ui);
            ui.horizontal_wrapped(|ui| {
                if self.preset_matters() {
                    ui.label("Quality:");
                    egui::ComboBox::from_id_salt("preset").selected_text(self.options.preset.label()).show_ui(ui, |ui| {
                        for p in Preset::ALL {
                            ui.selectable_value(&mut self.options.preset, p, p.label());
                        }
                    })
                    .response
                    .on_hover_text("Lossless keeps pixels/samples exact where the format allows, and uses the highest setting for lossy formats.");
                }
                ui.checkbox(&mut self.options.strip_metadata, "Strip metadata")
                    .on_hover_text("Drop EXIF, ICC profiles and audio tags instead of copying them to the output.");
                if self.frames_matter() {
                    ui.checkbox(&mut self.options.split_frames, "Export every frame").on_hover_text(
                        "When an animation, multi-page file or multi-size icon goes to a format that can't hold all of it, save each image as its own file in a \"name-frames\" folder instead of keeping only the first.",
                    );
                }

                if self.many_files_matter() {
                    ui.label("Many files:");
                    egui::ComboBox::from_id_salt("bundle")
                        .selected_text(self.options.bundle.map_or("Ask each time", BundleMode::label))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.options.bundle, None, "Ask each time");
                            for m in [BundleMode::Folder, BundleMode::Zip] {
                                ui.selectable_value(&mut self.options.bundle, Some(m), m.label());
                            }
                        })
                        .response
                        .on_hover_text("When one file turns into many (a page or frame per image), put them in a folder or in a zip file.");
                }

                let dest = self.options.dest_dir.as_ref().map(|d| d.display().to_string());
                if ui.button(dest.as_deref().map_or("Output: next to original".into(), |d| format!("Output: {d}"))).clicked() {
                    self.options.dest_dir = rfd::FileDialog::new().set_title("Output folder").pick_folder().or(self.options.dest_dir.take());
                }
                if self.options.dest_dir.is_some() && ui.small_button("reset").clicked() {
                    self.options.dest_dir = None;
                }
            });
            ui.horizontal(|ui| {
                let pending = self.pending_count();
                let label = match pending {
                    0 | 1 => "Convert all".to_string(),
                    n => format!("Convert all ({n})"),
                };
                // The queue accepts new jobs while others run, so this stays usable mid-batch.
                if ui.add_enabled(pending > 0, egui::Button::new(label)).clicked() {
                    self.convert_all();
                }
                if self.busy() && ui.button("Cancel").clicked() {
                    self.runner.cancel_all();
                }
                if !self.busy() {
                    if self.entries.iter().any(|e| matches!(e.status, Status::Done(..))) && ui.button("Clear finished").clicked() {
                        self.entries.retain(|e| !matches!(e.status, Status::Done(..)));
                    }
                    if !self.entries.is_empty() && ui.button("Clear all").clicked() {
                        self.entries.clear();
                    }
                }
                ui.weak(self.summary());
                if let Some(n) = &self.notice {
                    ui.colored_label(ui.visuals().warn_fg_color, n);
                }
            });
            ui.add_space(6.0);
        });

        let ctx = ui.ctx().clone();
        self.show_editor(&ctx);
        self.bundle_prompt(&ctx);

        egui::CentralPanel::default().show(ui, |ui| {
            self.drop_zone(ui);
            ui.add_space(6.0);
            if self.entries.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(8.0);
                    self.supported_formats(ui);
                    ui.add_space(4.0);
                    ui.weak("Everything is converted on this computer. Nothing is uploaded.");
                });
            } else {
                self.bulk_targets(ui);
                ui.separator();
                self.file_list(ui);
            }
        });
    }
}

/// winit 0.30 has no drag-and-drop on Wayland (the cursor shows "forbidden"), so
/// when XWayland is available use X11, which supports dropping files from file managers.
#[cfg(target_os = "linux")]
fn prefer_x11(builder: &mut eframe::EventLoopBuilder<eframe::UserEvent>) {
    use winit::platform::x11::EventLoopBuilderExtX11;
    if std::env::var_os("DISPLAY").is_some() {
        builder.with_x11();
    }
}

#[cfg(not(target_os = "linux"))]
fn prefer_x11(_: &mut eframe::EventLoopBuilder<eframe::UserEvent>) {}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([860.0, 540.0])
            .with_min_inner_size([620.0, 360.0])
            .with_drag_and_drop(true),
        event_loop_builder: Some(Box::new(prefer_x11)),
        ..Default::default()
    };
    eframe::run_native("Dotty", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use dotty_core::{EditOp, Mask, Region};

    fn app() -> App {
        App::build(egui::Context::default(), Options::default())
    }

    fn wait_idle(app: &mut App) {
        let start = Instant::now();
        while app.busy() && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
            app.pump_events();
        }
        app.pump_events();
    }

    #[test]
    fn adds_supported_files_skips_others_and_converts() {
        let dir = std::env::temp_dir().join(format!("tf-app-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("a.png");
        std::fs::write(&png, include_bytes!("../tests/tiny.png")).unwrap();
        let junk = dir.join("notes.xyz");
        std::fs::write(&junk, "hello").unwrap();

        let mut app = app();
        app.add_files([png.clone(), junk, png.clone()]); // duplicate and unsupported ignored
        assert_eq!(app.entries.len(), 1);
        assert!(app.notice.as_deref().is_some_and(|n| n.contains("skipped")));
        assert_eq!(app.pending_count(), 1);
        assert!(app.preset_matters());

        // PNG to BMP: the quality preset is irrelevant, so the control hides.
        app.entries[0].to = Some(Format::Bmp);
        assert!(!app.preset_matters());
        app.entries[0].to = Some(Format::Jpeg);
        assert!(app.preset_matters());

        app.convert_all();
        wait_idle(&mut app);
        assert!(matches!(app.entries[0].status, Status::Done(..)), "not done");
        assert!(dir.join("a.jpg").exists());
        assert_eq!(app.pending_count(), 0, "finished files are not converted again");
        assert_eq!(app.summary(), "1 done");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn go_converts_only_that_row() {
        let dir = std::env::temp_dir().join(format!("tf-app-go-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.png"), dir.join("b.png"));
        std::fs::write(&a, include_bytes!("../tests/tiny.png")).unwrap();
        std::fs::write(&b, include_bytes!("../tests/tiny.png")).unwrap();
        let mut app = app();
        app.add_files([a, b]);
        app.entries[0].to = Some(Format::Bmp);
        app.entries[1].to = Some(Format::Bmp);
        app.convert(1, None);
        wait_idle(&mut app);
        assert!(app.entries[0].status == Status::Ready, "untouched row stays ready");
        assert!(matches!(app.entries[1].status, Status::Done(..)));
        assert!(dir.join("b.bmp").exists() && !dir.join("a.bmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn choices_persist_and_only_show_for_formats_in_use() {
        let dir = std::env::temp_dir().join(format!("tf-app-choices-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("a.png");
        std::fs::write(&png, include_bytes!("../tests/tiny.png")).unwrap();
        let mut app = app();
        app.add_files([png]);
        app.entries[0].to = Some(Format::Webp);
        let shown = app.relevant_choices();
        assert_eq!(shown.len(), 1);
        let (f, is_source, choices) = &shown[0];
        assert_eq!((*f, *is_source), (Format::Webp, false));
        // A still PNG source: no "Animation" setting, just compression.
        assert_eq!(choices.iter().map(|c| c.key).collect::<Vec<_>>(), ["compression"]);

        app.options.set_choice(Format::Webp, &choices[0], "lossless");
        let mut restored = Options::default();
        choices_from_string(&choices_to_string(&app.options), &mut restored);
        assert_eq!(restored.choice(Format::Webp, &choices[0]), "lossless");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn frame_option_only_offered_for_animatable_sources_to_still_formats() {
        let dir = std::env::temp_dir().join(format!("tf-app-frames-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("a.png");
        std::fs::write(&png, include_bytes!("../tests/tiny.png")).unwrap();
        let mut app = app();
        app.add_files([png]);
        assert!(!app.frames_matter());
        app.entries[0].multi = true; // stand-in for an animated GIF
        app.entries[0].from = Format::Gif;
        app.entries[0].to = Some(Format::Jpeg);
        assert!(app.frames_matter());
        app.entries[0].from = Format::Webp;
        app.entries[0].to = Some(Format::Gif); // GIF keeps the animation
        assert!(!app.frames_matter());
        app.entries[0].from = Format::Tiff;
        app.entries[0].to = Some(Format::Gif); // pages can't animate
        assert!(app.frames_matter());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failed_files_can_be_retried() {
        let dir = std::env::temp_dir().join(format!("tf-app-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.png");
        std::fs::write(&bad, b"\x89PNG\r\n\x1a\nnot really").unwrap();
        let mut app = app();
        app.add_files([bad]);
        app.entries[0].to = Some(Format::Jpeg);
        app.convert_all();
        wait_idle(&mut app);
        assert!(matches!(app.entries[0].status, Status::Failed(_)));
        assert_eq!(app.pending_count(), 1, "a failed file can be retried");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edits_offer_the_same_format_and_avoid_targets_without_alpha() {
        let dir = std::env::temp_dir().join(format!("tf-app-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("a.png");
        std::fs::write(&png, include_bytes!("../tests/tiny.png")).unwrap();
        let mut app = app();
        let ctx = egui::Context::default();
        app.add_files([png]);
        let id = app.entries[0].id;
        app.entries[0].to = Some(Format::Jpeg);
        assert!(!app.targets(&app.entries[0]).contains(&Format::Png));

        let (w, h) = image_size(&dir.join("a.png"));
        let mut keep = vec![255u8; (w * h) as usize];
        keep[0] = 0;
        let erase = Edits { ops: vec![EditOp::Erase { frame: 0, mask: Mask::new(w, h, keep) }] };
        app.set_edits(&ctx, id, Some(erase), Some(egui::ColorImage::new([1, 1], vec![egui::Color32::RED])));
        let e = &app.entries[0];
        assert_eq!(e.to, Some(Format::Png), "JPEG can't hold the transparency, so PNG → PNG");
        assert!(e.preview.is_some() && e.erased());
        assert_eq!(app.targets(e)[0], Format::Png);

        app.convert_all();
        wait_idle(&mut app);
        let Status::Done(out, _) = &app.entries[0].status else { panic!("not done") };
        assert_eq!(out, &dir.join("a (1).png"), "never overwrites the source");

        // Discarding the edits takes the same-format target away again.
        app.set_edits(&ctx, id, None, None);
        assert!(app.entries[0].edits.is_none() && app.entries[0].to != Some(Format::Png));
        assert!(app.entries[0].status == Status::Ready);

        // A crop alone keeps a JPEG target: nothing became transparent.
        app.entries[0].to = Some(Format::Jpeg);
        let crop = Region { x: 0, y: 0, width: 1, height: 1, of_width: w, of_height: h };
        app.set_edits(&ctx, id, Some(Edits { ops: vec![EditOp::Crop(crop)] }), None);
        assert_eq!(app.entries[0].to, Some(Format::Jpeg));
        assert!(!app.entries[0].erased());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn image_size(path: &std::path::Path) -> (u32, u32) {
        let (_, size) = codecs_image::edit::decode_for_edit(path, Format::Png, &Options::default(), u32::MAX, u64::MAX).unwrap();
        size
    }

    #[test]
    fn animated_files_are_found_in_the_background() {
        let dir = std::env::temp_dir().join(format!("tf-app-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A 1x1 GIF with two frames.
        let frame: &[u8] = &[0x21, 0xF9, 4, 0, 10, 0, 0, 0, 0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 2, 0x44, 1, 0];
        let mut gif = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff".to_vec();
        gif.extend_from_slice(b"\x21\xff\x0bNETSCAPE2.0\x03\x01\x00\x00\x00");
        gif.extend_from_slice(frame);
        gif.extend_from_slice(frame);
        gif.push(0x3B);
        let path = dir.join("anim.gif");
        std::fs::write(&path, &gif).unwrap();

        let mut app = app();
        app.add_files([path]);
        assert_eq!(app.entries.len(), 1, "added straight away, before the probe finishes");
        let start = Instant::now();
        while !app.entries[0].multi && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
            app.pump_events();
        }
        assert!(app.entries[0].multi, "the probe found both frames");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_video_converts_its_soundtrack_and_edits_it_as_audio() {
        let dir = std::env::temp_dir().join(format!("tf-app-video-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("clip.mkv");
        std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/../codecs-audio/tests/fixtures/video_two_tracks.mkv"), &video).unwrap();

        let mut app = app();
        app.add_files([video]);
        let e = &app.entries[0];
        assert_eq!((e.from, e.from.kind()), (Format::Mkv, Kind::Video));
        assert_eq!(e.to, Some(Format::Webm), "a video becomes WebM by default");
        let targets = app.targets(e);
        assert!(targets.contains(&Format::Mp4) && targets.contains(&Format::Mp3) && targets.contains(&Format::Png));
        assert!(!targets.contains(&Format::Mkv), "not its own format until it's edited");
        assert_eq!(e.tool(), Some(Tool::Video), "Edit opens the video editor");
        app.entries[0].to = Some(Format::Mp3);
        assert_eq!(app.entries[0].tool(), Some(Tool::Audio), "Edit opens the audio editor on the soundtrack");

        app.convert_all();
        wait_idle(&mut app);
        assert!(matches!(app.entries[0].status, Status::Done(..)), "soundtrack conversion finished");
        assert!(dir.join("clip.mp3").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_video_can_become_a_picture() {
        let dir = std::env::temp_dir().join(format!("tf-app-video-picture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("clip.mkv");
        std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/../codecs-audio/tests/fixtures/video_two_tracks.mkv"), &video).unwrap();
        let mut app = app();
        app.add_files([video]);
        assert!(app.targets(&app.entries[0]).contains(&Format::Png));
        app.entries[0].to = Some(Format::Png);
        assert_eq!(app.entries[0].tool(), Some(Tool::Frames), "Edit opens the frame picker");
        app.convert_all();
        wait_idle(&mut app);
        if let Status::Failed(m) = &app.entries[0].status {
            panic!("failed: {m}");
        }
        assert!(matches!(app.entries[0].status, Status::Done(..)));
        assert_eq!(codecs_video::image::image_dimensions(dir.join("clip.png")).unwrap(), (64, 64));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_songs_picture_edits_and_sound_edits_live_side_by_side() {
        let dir = std::env::temp_dir().join(format!("tf-app-songpic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let song = dir.join("song.mp3");
        std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/../codecs-video/tests/fixtures/song_with_cover.mp3"), &song).unwrap();
        let pic = dir.join("pic.png");
        codecs_video::image::RgbImage::from_pixel(40, 30, codecs_video::image::Rgb([9, 99, 199])).save(&pic).unwrap();

        let mut app = app();
        app.add_files([song]);
        app.entries[0].to = Some(Format::Webm);
        assert_eq!(app.entries[0].tool(), Some(Tool::Audio), "Edit is still the sound editor for a song");
        let picture = Edits { ops: vec![dotty_core::EditOp::Video(dotty_core::VideoEdits { picture_file: Some(pic.clone()), ..Default::default() })] };
        let sound = Edits { ops: vec![dotty_core::EditOp::Audio(dotty_core::AudioEdits { volume: dotty_core::Volume::Gain(-3.0), ..Default::default() })] };

        // Each editor's result keeps what the other made.
        let both = Edits { ops: picture.ops.iter().chain(sound.ops.iter()).cloned().collect() };
        assert!(both.audio().is_some() && both.video().unwrap().picture_file.as_ref() == Some(&pic));
        let both_again = with_video_edits(Some(&picture), Some(sound.clone())).unwrap();
        assert!(both_again.audio().is_some() && both_again.video().is_some());
        assert!(with_video_edits(Some(&picture), None).unwrap().video().is_some(), "clearing the sound edits keeps the picture");
        assert!(with_video_edits(None, None).is_none());

        app.entries[0].edits = Some(Arc::new(both));
        app.convert_all();
        wait_idle(&mut app);
        if let Status::Failed(m) = &app.entries[0].status {
            panic!("failed: {m}");
        }
        assert_eq!(codecs_video::VideoReader::open(&dir.join("song.webm"), Format::Webm, 1).unwrap().info().display_size(), (40, 30), "the chosen picture, not the 120×90 cover");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn animated_gif(path: &std::path::Path) {
        use codecs_video::image;
        let mut enc = image::codecs::gif::GifEncoder::new(std::fs::File::create(path).unwrap());
        for shade in [10u8, 120, 240] {
            let buf = image::RgbaImage::from_pixel(8, 8, image::Rgba([shade, 0, 0, 255]));
            enc.encode_frame(image::Frame::from_parts(buf, 0, 0, image::Delay::from_numer_denom_ms(100, 1))).unwrap();
        }
    }

    #[test]
    fn many_files_ask_for_a_folder_or_zip_unless_a_standing_choice_is_set() {
        let dir = std::env::temp_dir().join(format!("tf-app-bundle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let gif = dir.join("anim.gif");
        animated_gif(&gif);
        let plain = dir.join("plain.png");
        std::fs::write(&plain, include_bytes!("../tests/tiny.png")).unwrap();

        // One file out: no question.
        let mut a = app();
        a.options.split_frames = true;
        a.add_files([plain.clone()]);
        a.entries[0].to = Some(Format::Bmp);
        a.convert_all();
        assert!(a.ask_bundle.is_none());
        wait_idle(&mut a);
        assert!(dir.join("plain.bmp").exists());

        // Many files out: the prompt comes first and nothing is queued yet.
        let mut a = app();
        a.options.split_frames = true;
        a.add_files([gif.clone()]);
        a.entries[0].to = Some(Format::Bmp);
        assert!(a.many_files_matter());
        a.convert_all();
        assert!(a.ask_bundle.is_some() && !a.busy());
        assert!(!dir.join("anim-frames").exists());

        // Answering "zip" runs it once, without changing the standing choice.
        let ids = a.ask_bundle.take().unwrap();
        a.start_with(ids, Some(BundleMode::Zip));
        wait_idle(&mut a);
        assert!(a.options.bundle.is_none());
        assert!(dir.join("anim-frames.zip").exists() && !dir.join("anim-frames").exists());

        // A standing choice skips the question.
        let mut a = app();
        a.options.split_frames = true;
        a.options.bundle = Some(BundleMode::Folder);
        a.add_files([gif]);
        a.entries[0].to = Some(Format::Bmp);
        a.convert_all();
        assert!(a.ask_bundle.is_none());
        wait_idle(&mut a);
        assert!(dir.join("anim-frames/anim-001.bmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn data_subtitles_and_fonts_convert_from_the_list() {
        let dir = std::env::temp_dir().join(format!("tf-app-text-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("d.json"), r#"{"a": [1, 2], "b": {"c": "x"}}"#).unwrap();
        std::fs::write(dir.join("s.srt"), "1\n00:00:01,000 --> 00:00:02,000\nHi\n").unwrap();
        std::fs::write(dir.join("f.ttf"), include_bytes!("../../codecs-font/tests/fixtures/egui-icons.ttf")).unwrap();
        let mut a = app();
        a.add_files([dir.join("d.json"), dir.join("s.srt"), dir.join("f.ttf")]);
        assert_eq!(a.entries.iter().map(|e| e.from).collect::<Vec<_>>(), [Format::Json, Format::Srt, Format::Ttf]);
        assert!(a.entries.iter().all(|e| e.to.is_some()), "every kind gets a default target");
        assert!(a.entries.iter().all(|e| !e.editable()), "no editor for these");
        a.entries[0].to = Some(Format::Yaml);
        a.entries[1].to = Some(Format::Vtt);
        a.entries[2].to = Some(Format::Woff2);
        a.convert_all();
        wait_idle(&mut a);
        for e in &a.entries {
            assert!(matches!(e.status, Status::Done(..)), "{} not done", e.path.display());
        }
        assert!(std::fs::read_to_string(dir.join("d.yaml")).unwrap().contains("c: x"));
        assert!(std::fs::read_to_string(dir.join("s.vtt")).unwrap().starts_with("WEBVTT"));
        assert!(std::fs::read(dir.join("f.woff2")).unwrap().starts_with(b"wOF2"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn archives_extract_by_default_and_a_picture_becomes_a_favicon_pack() {
        let dir = std::env::temp_dir().join(format!("tf-app-archive-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let zip = dir.join("bundle.zip");
        {
            use std::io::Write;
            let mut w = zip::ZipWriter::new(std::fs::File::create(&zip).unwrap());
            w.start_file("inside/a.txt", zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(b"hello").unwrap();
            w.finish().unwrap();
        }
        let logo = dir.join("logo.png");
        std::fs::write(&logo, include_bytes!("../tests/tiny.png")).unwrap();
        let mut a = app();
        a.add_files([zip, logo]);
        assert_eq!(a.entries[0].to, Some(Format::Folder), "extract by default");
        assert!(a.targets(&a.entries[0]).contains(&Format::SevenZ));
        assert!(!a.entries[0].editable());
        a.entries[1].to = Some(Format::Favicon);
        a.options.bundle = Some(BundleMode::Folder);
        a.convert_all();
        wait_idle(&mut a);
        assert!(std::fs::read_to_string(dir.join("bundle/inside/a.txt")).unwrap() == "hello");
        assert!(dir.join("logo-favicons/favicon.ico").exists());
        assert!(a.entries.iter().all(|e| matches!(e.status, Status::Done(..))));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn markdown_defaults_to_pdf_and_pictures_can_become_pdfs() {
        let dir = std::env::temp_dir().join(format!("tf-app-doc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("readme.md"), "# Title\n\nSome *text*.\n").unwrap();
        std::fs::write(dir.join("scan.png"), include_bytes!("../tests/tiny.png")).unwrap();
        let mut a = app();
        a.add_files([dir.join("readme.md"), dir.join("scan.png")]);
        assert_eq!((a.entries[0].from, a.entries[0].to), (Format::Md, Some(Format::Pdf)));
        assert!(a.targets(&a.entries[0]).contains(&Format::Html));
        assert!(a.targets(&a.entries[1]).contains(&Format::Pdf), "any picture can become a PDF");
        assert_ne!(a.entries[1].to, Some(Format::Pdf), "but a picture's default stays a picture");
        a.entries[1].to = Some(Format::Pdf);
        a.convert_all();
        wait_idle(&mut a);
        assert!(a.entries.iter().all(|e| matches!(e.status, Status::Done(..))));
        assert!(std::fs::read(dir.join("readme.pdf")).unwrap().starts_with(b"%PDF-"));
        assert!(std::fs::read(dir.join("scan.pdf")).unwrap().starts_with(b"%PDF-"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_pdf_offers_pictures_and_text_and_asks_before_making_a_page_each() {
        let dir = std::env::temp_dir().join(format!("tf-app-pdf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let long: String = (0..6).map(|i| format!("# Part {i}\n\n{}\n\n", "Some words to fill a page with text. ".repeat(60))).collect();
        std::fs::write(dir.join("book.md"), long).unwrap();
        let mut a = app();
        a.add_files([dir.join("book.md")]);
        a.convert_all();
        wait_idle(&mut a);
        assert!(dir.join("book.pdf").exists());

        let mut a = app();
        a.add_files([dir.join("book.pdf")]);
        assert_eq!(a.entries[0].from, Format::Pdf);
        let targets = a.targets(&a.entries[0]);
        assert!(targets.contains(&Format::Png) && targets.contains(&Format::Txt) && !targets.contains(&Format::Favicon));
        assert_eq!(a.entries[0].to, Some(Format::Png), "pages as pictures by default");
        a.convert_all();
        assert!(a.ask_bundle.is_some() && !a.busy(), "several pages: folder or zip?");
        let ids = a.ask_bundle.take().unwrap();
        a.start_with(ids, Some(BundleMode::Folder));
        wait_idle(&mut a);
        assert!(matches!(a.entries[0].status, Status::Done(..)));
        assert!(dir.join("book-pages/book-01.png").exists());

        // The text of the same PDF.
        a.entries[0].status = Status::Ready;
        a.entries[0].to = Some(Format::Txt);
        a.convert_all();
        wait_idle(&mut a);
        assert!(std::fs::read_to_string(dir.join("book.txt")).unwrap().contains("Part 3"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_raw_photo_can_be_edited_and_converted_like_any_picture() {
        let dir = std::env::temp_dir().join(format!("tf-app-raw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A TIFF-style file with an 800×600 JPEG inside, named like a Nikon RAW.
        let mut jpeg = Vec::new();
        codecs_video::image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80).encode_image(&codecs_video::image::RgbImage::from_pixel(800, 600, codecs_video::image::Rgb([200, 100, 50]))).unwrap();
        let mut raw = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        raw.extend_from_slice(&jpeg);
        std::fs::write(dir.join("DSC_0001.NEF"), raw).unwrap();
        let mut a = app();
        a.add_files([dir.join("DSC_0001.NEF")]);
        assert_eq!(a.entries[0].from, Format::Raw);
        assert!(a.entries[0].to.is_some_and(|t| t.kind() == Kind::Image), "a picture target by default");
        assert!(a.entries[0].editable(), "the picture editor works on it");
        let targets = a.targets(&a.entries[0]);
        assert!(targets.contains(&Format::Jpeg) && targets.contains(&Format::Pdf) && targets.contains(&Format::Webp));
        a.entries[0].to = Some(Format::Jpeg);
        a.convert_all();
        wait_idle(&mut a);
        assert!(matches!(a.entries[0].status, Status::Done(..)));
        assert_eq!(codecs_video::image::image_dimensions(dir.join("DSC_0001.jpg")).unwrap(), (800, 600));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_3d_model_converts_previews_and_can_become_a_picture() {
        let dir = std::env::temp_dir().join(format!("tf-app-model-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let obj = "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 0 0 1\nv 1 0 1\nv 1 1 1\nv 0 1 1\nf 1 3 2\nf 1 4 3\nf 5 6 7\nf 5 7 8\nf 1 2 6\nf 1 6 5\nf 2 3 7\nf 2 7 6\nf 3 4 8\nf 3 8 7\nf 4 1 5\nf 4 5 8\n";
        std::fs::write(dir.join("part.obj"), obj).unwrap();
        let mut a = app();
        a.add_files([dir.join("part.obj")]);
        assert_eq!(a.entries[0].from, Format::Obj);
        assert_eq!(a.entries[0].to, Some(Format::Stl), "3D printing is the likely goal");
        assert_eq!(a.entries[0].tool(), Some(Tool::Model));
        let targets = a.targets(&a.entries[0]);
        assert!(targets.contains(&Format::Glb) && targets.contains(&Format::Ply) && targets.contains(&Format::Png), "other models and pictures");
        assert!(!a.many_files_matter(), "one file out: no folder/zip question");

        a.convert_all();
        wait_idle(&mut a);
        assert!(matches!(a.entries[0].status, Status::Done(..)));
        let stl = std::fs::read(dir.join("part.stl")).unwrap();
        assert_eq!(stl.len(), 84 + 12 * 50, "binary STL of 12 triangles");

        // The STL goes on to GLB by default, then to a picture.
        let mut a = app();
        a.add_files([dir.join("part.stl")]);
        assert_eq!(a.entries[0].to, Some(Format::Glb));
        a.entries[0].to = Some(Format::Png);
        let keys = |a: &App| a.relevant_choices().into_iter().filter(|(f, src, _)| *f == Format::Png && !src).flat_map(|(_, _, c)| c.into_iter().map(|c| c.key)).collect::<Vec<_>>();
        assert!(keys(&a).contains(&"model_view") && keys(&a).contains(&"model_size"), "a model drawn as a picture offers view and size");
        a.convert_all();
        wait_idle(&mut a);
        assert!(matches!(a.entries[0].status, Status::Done(..)));
        assert_eq!(codecs_video::image::image_dimensions(dir.join("part.png")).unwrap(), (1024, 768));
        // A photo going to PNG does not.
        std::fs::write(dir.join("photo.bmp"), include_bytes!("../tests/tiny.png")).ok();
        let mut b = app();
        std::fs::write(dir.join("photo.png"), include_bytes!("../tests/tiny.png")).unwrap();
        b.add_files([dir.join("photo.png")]);
        b.entries[0].to = Some(Format::Png);
        assert!(!keys(&b).iter().any(|k| k.starts_with("model_")), "no 3D settings for a photo: {:?}", keys(&b));
        std::fs::remove_dir_all(&dir).ok();
    }
}
