//! 3D models: STL, OBJ, PLY, glTF and GLB read into one triangle mesh and written as any of
//! them, or drawn as a picture. See [`mesh`], the format modules and [`render`].

pub mod base64;
pub mod gltf_io;
pub mod mesh;
pub mod obj;
pub mod ply;
pub mod render;
pub mod stl;

use codecs_image::{Frame, ImageConverter, ImageData};
use dotty_core::{Choice, ConvertCtx, Converter, Error, Format, Kind, Registry, Result};
use mesh::{Mesh, Up};
use std::path::Path;

pub fn register(registry: &mut Registry) {
    registry.register(Box::new(ModelConverter));
}

pub const MODEL_AXES: Choice = Choice {
    key: "model_axes",
    label: "Model axes",
    help: "STL and PLY point Z up (3D printing); OBJ and glTF point Y up. Turn the model when converting between them so it stands the same way, or keep the coordinates as they are.",
    values: &[("auto", "Stand it upright"), ("keep", "Keep coordinates")],
    multi_image_only: false,
};

pub const STL_STYLE: Choice = Choice {
    key: "stl_style",
    label: "STL type",
    help: "Binary is compact and what printers expect; ASCII is readable text but several times larger",
    values: &[("binary", "Binary"), ("ascii", "ASCII text")],
    multi_image_only: false,
};

pub const PLY_STYLE: Choice = Choice {
    key: "ply_style",
    label: "PLY type",
    help: "Binary is compact; ASCII is readable text but several times larger",
    values: &[("binary", "Binary"), ("ascii", "ASCII text")],
    multi_image_only: false,
};

pub const MODEL_VIEW: Choice = Choice {
    key: "model_view",
    label: "3D picture view",
    help: "From where the model is drawn when it becomes a picture",
    values: &[("iso", "Three-quarter"), ("front", "Front"), ("back", "Back"), ("left", "Left"), ("right", "Right"), ("top", "Top")],
    multi_image_only: false,
};

pub const MODEL_SIZE: Choice = Choice {
    key: "model_size",
    label: "3D picture size",
    help: "Width of the picture in pixels (4:3)",
    values: &[("1024", "1024 px"), ("512", "512 px"), ("2048", "2048 px")],
    multi_image_only: false,
};

pub struct ModelConverter;

const MODELS: [Format; 5] = [Format::Stl, Format::Obj, Format::Ply, Format::Gltf, Format::Glb];

/// The up axis a format's files use.
pub fn up_of(f: Format) -> Up {
    if matches!(f, Format::Stl | Format::Ply) { Up::Z } else { Up::Y }
}

pub struct Loaded {
    pub mesh: Mesh,
    pub notes: Vec<String>,
}

/// Read a model file (any of the five formats).
pub fn read(path: &Path, from: Format) -> Result<Loaded> {
    let bytes = std::fs::read(path).map_err(|source| Error::Io { path: path.into(), source })?;
    let dir = path.parent();
    let (mut mesh, mut notes) = match from {
        Format::Stl => (stl::read(&bytes)?, Vec::new()),
        Format::Obj => {
            let r = obj::read(&String::from_utf8_lossy(&bytes))?;
            (r.mesh, r.notes)
        }
        Format::Ply => (ply::read(&bytes)?, Vec::new()),
        Format::Gltf | Format::Glb => {
            let r = gltf_io::read(&bytes, dir)?;
            (r.mesh, r.notes)
        }
        _ => return Err(Error::UnknownFormat),
    };
    let dropped = mesh.clean();
    if dropped > 0 && !notes.iter().any(|n| n.contains("broken triangle")) {
        notes.push(format!("{dropped} empty or broken triangle(s) were left out"));
    }
    Ok(Loaded { mesh, notes })
}

impl Converter for ModelConverter {
    fn routes(&self) -> Vec<(Format, Format)> {
        let mut r: Vec<(Format, Format)> = MODELS.iter().flat_map(|&a| MODELS.iter().filter(move |&&b| b != a).map(move |&b| (a, b))).collect();
        let images = ImageConverter::default().writable();
        r.extend(MODELS.iter().flat_map(|&m| images.iter().map(move |&i| (m, i))));
        r
    }

    fn choices(&self, to: Format) -> Vec<Choice> {
        match to {
            Format::Stl => vec![STL_STYLE, MODEL_AXES],
            Format::Ply => vec![PLY_STYLE, MODEL_AXES],
            Format::Obj | Format::Gltf | Format::Glb => vec![MODEL_AXES],
            t if t.kind() == Kind::Image => vec![MODEL_VIEW, MODEL_SIZE],
            _ => Vec::new(),
        }
    }

    fn convert(&self, src: &Path, dst: &Path, from: Format, to: Format, ctx: &ConvertCtx) -> Result<()> {
        (ctx.progress)(0.0);
        let Loaded { mut mesh, notes } = read(src, from)?;
        for n in notes {
            ctx.note(n);
        }
        ctx.check_cancelled()?;
        (ctx.progress)(0.4);
        if to.kind() == Kind::Image {
            return to_picture(&mesh, dst, to, ctx);
        }
        let needs_faces = matches!(to, Format::Stl);
        if mesh.triangles.is_empty() && needs_faces {
            return Err(Error::Codec("this model has points but no surfaces, which STL can't hold".into()));
        }
        if ctx.options.choice(to, &MODEL_AXES) == "auto" && mesh.up != up_of(to) {
            ctx.note(if up_of(to) == Up::Y { "Turned a quarter turn so the model stands upright in this format (Z-up to Y-up)" } else { "Turned a quarter turn so the model stands upright in this format (Y-up to Z-up)" });
            mesh.set_up(up_of(to));
        }
        let bytes: Vec<u8> = match to {
            Format::Stl => {
                if mesh.has_colors() {
                    ctx.note("STL holds no colours");
                }
                stl::write(&mesh, ctx.options.choice(Format::Stl, &STL_STYLE) == "ascii")
            }
            Format::Obj => obj::write(&mesh).into_bytes(),
            Format::Ply => ply::write(&mesh, ctx.options.choice(Format::Ply, &PLY_STYLE) == "ascii"),
            Format::Gltf => gltf_io::write(&mesh, false),
            Format::Glb => gltf_io::write(&mesh, true),
            _ => return Err(Error::NoRoute { from, to }),
        };
        (ctx.progress)(0.9);
        std::fs::write(dst, bytes).map_err(|source| Error::Io { path: dst.into(), source })?;
        (ctx.progress)(1.0);
        Ok(())
    }
}

fn to_picture(mesh: &Mesh, dst: &Path, to: Format, ctx: &ConvertCtx) -> Result<()> {
    if mesh.positions.is_empty() {
        return Err(Error::Codec("this model is empty".into()));
    }
    let width: u32 = ctx.options.choice(to, &MODEL_SIZE).parse().unwrap_or(1024);
    let height = width * 3 / 4;
    let cam = render::Camera::named(ctx.options.choice(to, &MODEL_VIEW));
    let opts = render::Options { supersample: if width <= 1024 { 2 } else { 1 }, ..Default::default() };
    let r = render::render(mesh, &cam, width, height, &opts);
    ctx.check_cancelled()?;
    let data = ImageData { frames: vec![Frame { image: image::DynamicImage::ImageRgba8(r.image), delay: std::time::Duration::ZERO }], exif: None, icc: None, more_frames: false, pages: false, loops: 0 };
    let bytes = ImageConverter::default().encode_to_vec(data, Format::Png, to, ctx)?;
    std::fs::write(dst, bytes).map_err(|source| Error::Io { path: dst.into(), source })?;
    ctx.note(format!("{} triangles, drawn from one side (no lighting setup or textures)", r.total));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dotty_core::Options;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tf-model-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn convert(src: &Path, from: Format, to: Format, options: &Options) -> (PathBuf, Vec<String>) {
        let (cancel, progress) = (AtomicBool::new(false), |_: f32| {});
        let ctx = ConvertCtx::new(options, &cancel, &progress);
        let dst = src.with_extension(format!("out.{}", to.extension()));
        ModelConverter.convert(src, &dst, from, to, &ctx).unwrap_or_else(|e| panic!("{from} → {to}: {e}"));
        (dst, ctx.take_notes())
    }

    fn write_cube(dir: &Path, name: &str, to: Format) -> PathBuf {
        let m = mesh::cube();
        let bytes = match to {
            Format::Stl => stl::write(&m, false),
            Format::Ply => ply::write(&m, false),
            Format::Obj => obj::write(&m).into_bytes(),
            Format::Glb => gltf_io::write(&m, true),
            _ => gltf_io::write(&m, false),
        };
        let p = dir.join(name);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn every_format_converts_to_every_other_and_keeps_the_shape() {
        let dir = tmp("matrix");
        for from in MODELS {
            let src = write_cube(&dir, &format!("cube.{}", from.extension()), from);
            for to in MODELS.into_iter().filter(|t| *t != from) {
                let (out, _) = convert(&src, from, to, &Options::default());
                let back = read(&out, to).unwrap().mesh;
                assert_eq!(back.triangles.len(), 12, "{from} → {to}");
                let size = back.size();
                assert!(size.iter().all(|s| (s - 1.0).abs() < 1e-4), "{from} → {to}: {size:?}");
                assert_eq!(Format::detect(&out).or(Some(to)), Some(to));
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn z_up_models_are_stood_upright_for_y_up_formats_and_back() {
        let dir = tmp("axes");
        // A model 1 wide, 2 deep and 5 tall in the Z-up convention of an STL.
        let mut m = mesh::cube();
        m.positions.iter_mut().for_each(|p| *p = [p[0], p[1] * 2.0, p[2] * 5.0]);
        let stl_path = dir.join("tall.stl");
        std::fs::write(&stl_path, stl::write(&m, false)).unwrap();
        let (glb, notes) = convert(&stl_path, Format::Stl, Format::Glb, &Options::default());
        let size = read(&glb, Format::Glb).unwrap().mesh.size();
        assert!((size[1] - 5.0).abs() < 1e-4 && (size[2] - 2.0).abs() < 1e-4, "height is now Y: {size:?}");
        assert!(notes.iter().any(|n| n.contains("Z-up to Y-up")), "{notes:?}");
        // And without turning.
        let mut keep = Options::default();
        keep.set_choice(Format::Glb, &MODEL_AXES, "keep");
        let (kept, _) = convert(&stl_path, Format::Stl, Format::Glb, &keep);
        assert!((read(&kept, Format::Glb).unwrap().mesh.size()[2] - 5.0).abs() < 1e-4, "coordinates untouched");
        // Round trip through OBJ returns to the original orientation.
        let (obj, _) = convert(&stl_path, Format::Stl, Format::Obj, &Options::default());
        let (back, _) = convert(&obj, Format::Obj, Format::Stl, &Options::default());
        assert!((read(&back, Format::Stl).unwrap().mesh.size()[2] - 5.0).abs() < 1e-4);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ascii_options_a_picture_and_the_refusals() {
        let dir = tmp("misc");
        let src = write_cube(&dir, "c.glb", Format::Glb);
        let mut o = Options::default();
        o.set_choice(Format::Stl, &STL_STYLE, "ascii");
        let (stl_ascii, _) = convert(&src, Format::Glb, Format::Stl, &o);
        assert!(std::fs::read(&stl_ascii).unwrap().starts_with(b"solid"));
        // A colour model to STL says colours were dropped.
        let mut coloured = mesh::cube();
        coloured.colors = vec![[255, 0, 0, 255]; 8];
        coloured.up = Up::Y;
        let c = dir.join("red.glb");
        std::fs::write(&c, gltf_io::write(&coloured, true)).unwrap();
        assert!(convert(&c, Format::Glb, Format::Stl, &Options::default()).1.iter().any(|n| n.contains("no colours")));
        // As a picture, from the front, 512 px.
        let mut o = Options::default();
        o.set_choice(Format::Png, &MODEL_VIEW, "front");
        o.set_choice(Format::Png, &MODEL_SIZE, "512");
        let (png, notes) = convert(&c, Format::Glb, Format::Png, &o);
        let img = image::open(&png).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (512, 384));
        assert!(img.get_pixel(256, 192).0[0] > 120 && img.get_pixel(256, 192).0[1] < 80, "the red cube in the middle");
        assert!(notes.iter().any(|n| n.contains("12 triangles")));
        // Points only can't be an STL.
        let cloud = dir.join("cloud.obj");
        std::fs::write(&cloud, "v 0 0 0\nv 1 1 1\n").unwrap();
        let (cancel, progress) = (AtomicBool::new(false), |_: f32| {});
        let opts = Options::default();
        let ctx = ConvertCtx::new(&opts, &cancel, &progress);
        let err = ModelConverter.convert(&cloud, &dir.join("x.stl"), Format::Obj, Format::Stl, &ctx).unwrap_err().to_string();
        assert!(err.contains("no surfaces"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn routes_cover_models_and_pictures() {
        let r = ModelConverter.routes();
        for want in [(Format::Stl, Format::Glb), (Format::Glb, Format::Obj), (Format::Ply, Format::Png), (Format::Obj, Format::Jpeg)] {
            assert!(r.contains(&want), "{want:?}");
        }
        assert!(!r.contains(&(Format::Stl, Format::Stl)) && !r.contains(&(Format::Png, Format::Stl)));
    }
}
