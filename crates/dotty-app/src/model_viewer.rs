//! The 3D model viewer: "Preview" on a model's row. Drag to turn the model, scroll to zoom,
//! double-click to start again. It only looks (nothing is edited), drawn by the same software
//! renderer that makes "3D model → picture".

use codecs_model::mesh::Mesh;
use codecs_model::render::{self, Camera, Options as RenderOptions};
use dotty_core::Format;
use eframe::egui;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};

/// Triangles drawn while the model is being turned.
const MOVING_TRIANGLES: usize = 60_000;
/// Triangles drawn when it is still.
const STILL_TRIANGLES: usize = 400_000;

enum State {
    Loading(mpsc::Receiver<Result<codecs_model::Loaded, String>>),
    Ready { mesh: Arc<Mesh>, notes: Vec<String> },
    Failed(String),
}

pub struct ModelViewer {
    title: String,
    state: State,
    cam: Camera,
    wire: bool,
    texture: Option<egui::TextureHandle>,
    /// Size and settings the picture was last drawn for.
    drawn_for: Option<(egui::Vec2, Camera, bool, bool)>,
    /// Whether the last picture used the quick (sampled) setting.
    quick: bool,
    /// Triangles in the last picture, of the model's total.
    drawn: (usize, usize),
    last_move: f64,
}

pub enum Outcome {
    Open,
    Closed,
}

impl ModelViewer {
    pub fn open(ctx: &egui::Context, path: PathBuf, from: Format) -> Self {
        let title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let (tx, rx) = mpsc::channel();
        let wake = ctx.clone();
        std::thread::spawn(move || {
            let r = codecs_model::read(&path, from).map_err(|e| e.to_string());
            let _ = tx.send(r);
            wake.request_repaint();
        });
        Self { title, state: State::Loading(rx), cam: Camera::ISO, wire: false, texture: None, drawn_for: None, quick: false, drawn: (0, 0), last_move: f64::NEG_INFINITY }
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Outcome {
        if let State::Loading(rx) = &self.state
            && let Ok(r) = rx.try_recv()
        {
            self.state = match r {
                Ok(l) => State::Ready { mesh: Arc::new(l.mesh), notes: l.notes },
                Err(e) => State::Failed(e),
            };
        }
        let screen = ctx.content_rect();
        let size = (screen.size() - egui::vec2(48.0, 48.0)).max(egui::vec2(480.0, 360.0));
        let mut outcome = Outcome::Open;
        egui::Modal::new(egui::Id::unique("model-viewer")).show(ctx, |ui| {
            ui.set_min_size(size);
            ui.set_max_size(size);
            outcome = self.ui(ui);
        });
        outcome
    }

    fn ui(&mut self, ui: &mut egui::Ui) -> Outcome {
        let mut outcome = Outcome::Open;
        ui.horizontal(|ui| {
            ui.strong(&self.title);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Close").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    outcome = Outcome::Closed;
                }
            });
        });
        match &self.state {
            State::Loading(_) => {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| {
                    ui.spinner();
                    ui.label("Reading the model…");
                });
                return outcome;
            }
            State::Failed(e) => {
                ui.colored_label(ui.visuals().error_fg_color, format!("✖ Couldn't open this model: {e}"));
                return outcome;
            }
            State::Ready { .. } => {}
        }
        let State::Ready { mesh, notes } = &self.state else { return outcome };
        let (mesh, notes) = (mesh.clone(), notes.clone());
        ui.horizontal_wrapped(|ui| {
            for (label, view) in [("3/4", "iso"), ("Front", "front"), ("Back", "back"), ("Left", "left"), ("Right", "right"), ("Top", "top")] {
                if ui.small_button(label).clicked() {
                    self.cam = Camera::named(view);
                }
            }
            ui.separator();
            ui.checkbox(&mut self.wire, "Wireframe");
            ui.separator();
            let s = mesh.size();
            ui.weak(format!("{} triangles · {} points · {:.3} × {:.3} × {:.3} (the file's own units)", mesh.triangles.len(), mesh.positions.len(), s[0], s[1], s[2]));
        });
        for n in &notes {
            ui.colored_label(ui.visuals().warn_fg_color, format!("⚠ {n}"));
        }
        if mesh.triangles.is_empty() && !mesh.positions.is_empty() {
            ui.weak("This model is a set of points without surfaces.");
        }
        let avail = ui.available_size() - egui::vec2(0.0, 22.0);
        let (rect, resp) = ui.allocate_exact_size(avail.max(egui::vec2(200.0, 150.0)), egui::Sense::click_and_drag());
        let now = ui.input(|i| i.time);
        let mut moved = false;
        if resp.dragged() {
            let d = resp.drag_delta();
            self.cam.yaw += d.x * 0.01;
            self.cam.pitch = (self.cam.pitch + d.y * 0.01).clamp(-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2);
            moved = true;
        }
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                self.cam.zoom = (self.cam.zoom * (scroll * 0.004).exp()).clamp(0.2, 12.0);
                moved = true;
            }
        }
        if resp.double_clicked() {
            self.cam = Camera::ISO;
            moved = true;
        }
        if moved {
            self.last_move = now;
        }
        // While moving draw a sample of the triangles; once still, draw them all (smoothed).
        let still = now - self.last_move > 0.25;
        let ppp = ui.ctx().pixels_per_point();
        let target = ((rect.size() * ppp).min(egui::vec2(1600.0, 1200.0))).max(egui::vec2(64.0, 48.0));
        let key = (target, self.cam, self.wire, still);
        if self.drawn_for != Some(key) {
            let opts = RenderOptions {
                wireframe: self.wire,
                max_triangles: if still { STILL_TRIANGLES } else { MOVING_TRIANGLES },
                supersample: if still && mesh.triangles.len() < 150_000 { 2 } else { 1 },
                ..Default::default()
            };
            let r = render::render(&mesh, &self.cam, target.x as u32, target.y as u32, &opts);
            self.drawn = (r.drawn, r.total);
            self.quick = !still;
            let color = egui::ColorImage::from_rgba_unmultiplied([r.image.width() as usize, r.image.height() as usize], r.image.as_raw());
            self.texture = Some(ui.ctx().load_texture("model-preview", color, egui::TextureOptions::LINEAR));
            self.drawn_for = Some(key);
        }
        if let Some(tex) = &self.texture {
            ui.painter().image(tex.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
        }
        if !still {
            // Come back and draw it properly.
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(280));
        }
        ui.horizontal(|ui| {
            ui.weak("Drag to turn · scroll to zoom · double-click to reset");
            if self.drawn.0 < self.drawn.1 && !self.quick {
                ui.weak(format!("· showing {} of {} triangles", self.drawn.0, self.drawn.1));
            }
        });
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cube_file() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tf-app-viewer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("cube.obj");
        std::fs::write(&p, "v 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nv 0 0 1\nv 1 0 1\nv 1 1 1\nv 0 1 1\nf 1 3 2\nf 1 4 3\nf 5 6 7\nf 5 7 8\nf 1 2 6\nf 1 6 5\nf 2 3 7\nf 2 7 6\nf 3 4 8\nf 3 8 7\nf 4 1 5\nf 4 5 8\n").unwrap();
        p
    }

    #[test]
    fn the_viewer_loads_a_model_draws_it_and_survives_bad_files() {
        let ctx = egui::Context::default();
        let mut v = ModelViewer::open(&ctx, cube_file(), Format::Obj);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut frames = 0;
        while v.texture.is_none() && std::time::Instant::now() < deadline {
            let input = egui::RawInput { time: Some(frames as f64), screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 800.0))), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                assert!(matches!(v.show(ui.ctx()), Outcome::Open));
            });
            out.textures_delta.clear();
            frames += 1;
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(v.texture.is_some(), "the model was drawn");
        assert_eq!(v.drawn, (12, 12));
        assert!(matches!(v.state, State::Ready { .. }));

        let bad = std::env::temp_dir().join(format!("tf-app-viewer-{}/bad.stl", std::process::id()));
        std::fs::write(&bad, "nothing").unwrap();
        let mut v = ModelViewer::open(&ctx, bad, Format::Stl);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while matches!(v.state, State::Loading(_)) && std::time::Instant::now() < deadline {
            let input = egui::RawInput { time: Some(1.0), screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 800.0))), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                let _ = v.show(ui.ctx());
            });
            out.textures_delta.clear();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(matches!(&v.state, State::Failed(m) if m.contains("not a valid STL")));
    }
}
