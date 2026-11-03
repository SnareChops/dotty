use super::*;
use image::codecs::{jpeg::JpegEncoder, png};
use image::{ExtendedColorType, ImageDecoder as _, ImageEncoder as _, ImageReader};
use std::sync::atomic::AtomicBool;
use std::fs::File;
use std::io::BufWriter;
use dotty_core::{Choice, Options, Preset};

/// Every format we can write.
fn outputs() -> Vec<Format> {
    ImageConverter::default().encoders.iter().map(|e| e.format()).collect()
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tf-img-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run_notes(src: &Path, dst: &Path, from: Format, to: Format, options: &Options) -> Vec<String> {
    let cancel = AtomicBool::new(false);
    let progress = |_: f32| {};
    let ctx = ConvertCtx::new(options, &cancel, &progress);
    ImageConverter::default().convert(src, dst, from, to, &ctx).unwrap();
    ctx.take_notes()
}

fn run(src: &Path, dst: &Path, from: Format, to: Format, options: &Options) -> Result<()> {
    let cancel = AtomicBool::new(false);
    let progress = |_: f32| {};
    ImageConverter::default().convert(src, dst, from, to, &ConvertCtx::new(options, &cancel, &progress))
}

fn gradient(w: u32, h: u32, alpha: u8) -> image::RgbaImage {
    image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x * 255 / w) as u8, (y * 255 / h) as u8, 128, alpha]))
}

#[test]
fn every_route_converts_and_decodes_back() {
    let dir = tmp("routes");
    let src = dir.join("src.png");
    gradient(40, 30, 255).save(&src).unwrap();
    let opts = Options::default();

    for to in outputs() {
        if to == Format::Png {
            continue;
        }
        let mid = dir.join(format!("mid.{}", to.extension()));
        run(&src, &mid, Format::Png, to, &opts).unwrap_or_else(|e| panic!("png→{to}: {e}"));
        assert_eq!(Format::detect(&mid), Some(to), "sniffed output of {to}");

        // Every produced file must be convertible onward (back to PNG).
        let back = dir.join(format!("back-{}.png", to.extension()));
        run(&mid, &back, to, Format::Png, &opts).unwrap_or_else(|e| panic!("{to}→png: {e}"));
        let img = image::open(&back).unwrap();
        assert_eq!((img.width(), img.height()), (40, 30), "{to} round trip size");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn lossless_formats_preserve_pixels() {
    let dir = tmp("lossless");
    let src = dir.join("src.png");
    let original = gradient(33, 17, 200);
    original.save(&src).unwrap();

    // WebP is only lossless at the Lossless preset.
    let opts = Options { preset: Preset::Lossless, ..Default::default() };
    for to in [Format::Bmp, Format::Tiff, Format::Webp] {
        let mid = dir.join(format!("m.{}", to.extension()));
        let back = dir.join(format!("b-{}.png", to.extension()));
        run(&src, &mid, Format::Png, to, &opts).unwrap();
        run(&mid, &back, to, Format::Png, &opts).unwrap();
        assert_eq!(image::open(&back).unwrap().to_rgba8(), original, "{to} should be lossless");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn jpeg_flattens_transparency_onto_white() {
    let dir = tmp("alpha");
    let src = dir.join("clear.png");
    gradient(8, 8, 0).save(&src).unwrap(); // fully transparent
    let dst = dir.join("clear.jpg");
    run(&src, &dst, Format::Png, Format::Jpeg, &Options::default()).unwrap();
    let px = image::open(&dst).unwrap().to_rgb8().get_pixel(4, 4).0;
    assert!(px.iter().all(|&c| c > 245), "expected near-white, got {px:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn ico_downscales_large_images() {
    let dir = tmp("ico");
    let src = dir.join("big.png");
    gradient(600, 300, 255).save(&src).unwrap();
    let dst = dir.join("big.ico");
    run(&src, &dst, Format::Png, Format::Ico, &Options::default()).unwrap();
    let img = image::open(&dst).unwrap();
    assert_eq!((img.width(), img.height()), (256, 128));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn exif_is_kept_unless_stripped_and_orientation_is_baked_in() {
    let dir = tmp("exif");
    let src = dir.join("rot.jpg");
    // 4×2 JPEG tagged "rotate 90° CW" (orientation 6) via a minimal EXIF block.
    let exif: Vec<u8> = vec![
        b'I', b'I', 42, 0, 8, 0, 0, 0, // TIFF header, IFD at 8
        1, 0, // one entry
        0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, // Orientation = 6
        0, 0, 0, 0, // next IFD
    ];
    {
        let mut enc = JpegEncoder::new_with_quality(BufWriter::new(File::create(&src).unwrap()), 95);
        enc.set_exif_metadata(exif).unwrap();
        let rgb = DynamicImage::ImageRgba8(gradient(4, 2, 255)).to_rgb8();
        enc.write_image(rgb.as_raw(), 4, 2, ExtendedColorType::Rgb8).unwrap();
    }

    let keep = dir.join("keep.png");
    run(&src, &keep, Format::Jpeg, Format::Png, &Options::default()).unwrap();
    let img = image::open(&keep).unwrap();
    assert_eq!((img.width(), img.height()), (2, 4), "orientation applied to pixels");
    let mut dec = ImageReader::open(&keep).unwrap().into_decoder().unwrap();
    let kept = dec.exif_metadata().unwrap().expect("EXIF carried to PNG");
    let mut check = kept.clone();
    assert_eq!(
        image::metadata::Orientation::remove_from_exif_chunk(&mut check),
        Some(image::metadata::Orientation::NoTransforms),
        "orientation tag reset"
    );

    let strip = dir.join("strip.png");
    run(&src, &strip, Format::Jpeg, Format::Png, &Options { strip_metadata: true, ..Default::default() }).unwrap();
    let mut dec = ImageReader::open(&strip).unwrap().into_decoder().unwrap();
    assert!(dec.exif_metadata().unwrap().is_none(), "EXIF stripped on request");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn corrupt_input_is_an_error_not_a_panic() {
    let dir = tmp("corrupt");
    let src = dir.join("bad.png");
    std::fs::write(&src, b"\x89PNG\r\n\x1a\ngarbage garbage garbage").unwrap();
    let dst = dir.join("bad.jpg");
    assert!(run(&src, &dst, Format::Png, Format::Jpeg, &Options::default()).is_err());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn lossy_webp_is_smaller_faithful_and_keeps_alpha() {
    let dir = tmp("webp-lossy");
    let src = dir.join("a.png");
    // Textured (so lossless is expensive), partly transparent image.
    let img = image::RgbaImage::from_fn(96, 64, |x, y| {
        let n = (x.wrapping_mul(2654435761u32).wrapping_add(y.wrapping_mul(40503)) >> 7) % 25;
        image::Rgba([((x * 2) as u8).saturating_add(n as u8), ((y * 3) as u8).saturating_add(n as u8), 128, if x < 48 { 255 } else { 90 }])
    });
    img.save(&src).unwrap();

    let size = |p: Preset| {
        let dst = dir.join(format!("{p:?}.webp"));
        run(&src, &dst, Format::Png, Format::Webp, &Options { preset: p, ..Default::default() }).unwrap();
        std::fs::metadata(&dst).unwrap().len()
    };
    let (low, high, lossless) = (size(Preset::Low), size(Preset::High), size(Preset::Lossless));
    assert!(low < high && high < lossless, "sizes should grow with quality: {low} {high} {lossless}");

    let back = image::open(dir.join("High.webp")).unwrap().to_rgba8();
    assert_eq!(back.dimensions(), (96, 64));
    let (mut err, mut aerr) = (0u64, 0u64);
    for (a, b) in img.pixels().zip(back.pixels()) {
        err += (0..3).map(|c| (a[c] as i32 - b[c] as i32).unsigned_abs() as u64).sum::<u64>();
        aerr += (a[3] as i32 - b[3] as i32).unsigned_abs() as u64;
    }
    let n = 96 * 64;
    assert!(err / (n * 3) < 12, "mean colour error {}", err / (n * 3));
    assert!(aerr / n < 6, "mean alpha error {}", aerr / n);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn lossy_webp_carries_exif_and_icc_with_and_without_alpha() {
    let dir = tmp("webp-meta");
    let exif: Vec<u8> = vec![b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0];
    for alpha in [false, true] {
        let src = dir.join(format!("m{alpha}.png"));
        {
            let mut enc = png::PngEncoder::new(BufWriter::new(File::create(&src).unwrap()));
            enc.set_exif_metadata(exif.clone()).unwrap();
            let img = image::RgbaImage::from_fn(32, 32, |x, y| image::Rgba([x as u8 * 8, y as u8 * 8, 100, if alpha && x > 16 { 80 } else { 255 }]));
            enc.write_image(img.as_raw(), 32, 32, ExtendedColorType::Rgba8).unwrap();
        }
        let dst = dir.join(format!("m{alpha}.webp"));
        run(&src, &dst, Format::Png, Format::Webp, &Options::default()).unwrap();
        let mut dec = ImageReader::open(&dst).unwrap().into_decoder().unwrap();
        assert!(dec.exif_metadata().unwrap().is_some(), "alpha={alpha}: EXIF kept");
        assert_eq!(dec.dimensions(), (32, 32));

        let stripped = dir.join(format!("s{alpha}.webp"));
        run(&src, &stripped, Format::Png, Format::Webp, &Options { strip_metadata: true, ..Default::default() }).unwrap();
        let mut dec = ImageReader::open(&stripped).unwrap().into_decoder().unwrap();
        assert!(dec.exif_metadata().unwrap().is_none(), "alpha={alpha}: EXIF stripped");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn oversized_webp_is_a_clear_error() {
    let dir = tmp("webp-big");
    let src = dir.join("wide.png");
    image::RgbaImage::from_fn(16400, 2, |x, _| image::Rgba([(x % 256) as u8, 0, 0, 255])).save(&src).unwrap();
    let dst = dir.join("wide.webp");
    for preset in [Preset::High, Preset::Lossless] {
        let err = run(&src, &dst, Format::Png, Format::Webp, &Options { preset, ..Default::default() }).unwrap_err();
        assert!(err.to_string().contains("16383"), "{err}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn notes_flag_lossy_surprises_but_stay_quiet_otherwise() {
    use image::codecs::gif::{GifEncoder, Repeat};
    let dir = tmp("notes");
    let o = Options::default();

    // Animated GIF → PNG: first frame only.
    let anim = dir.join("anim.gif");
    {
        let mut enc = GifEncoder::new(File::create(&anim).unwrap());
        enc.set_repeat(Repeat::Infinite).unwrap();
        for shade in [10u8, 200] {
            enc.encode_frame(image::Frame::new(image::RgbaImage::from_pixel(8, 8, image::Rgba([shade, 0, 0, 255])))).unwrap();
        }
    }
    let notes = run_notes(&anim, &dir.join("anim.bmp"), Format::Gif, Format::Bmp, &o);
    assert!(notes.iter().any(|n| n.contains("first frame")), "{notes:?}");
    // PNG can animate (APNG), so nothing is lost there.
    assert!(run_notes(&anim, &dir.join("anim.png"), Format::Gif, Format::Png, &o).is_empty());

    // A single-frame GIF is not animated.
    let still = dir.join("still.gif");
    image::RgbaImage::from_pixel(8, 8, image::Rgba([1, 2, 3, 255])).save(&still).unwrap();
    assert!(run_notes(&still, &dir.join("still.png"), Format::Gif, Format::Png, &o).is_empty());

    // Transparent PNG → JPEG mentions the white fill; opaque PNG → JPEG says nothing.
    let clear = dir.join("clear.png");
    gradient(8, 8, 100).save(&clear).unwrap();
    let notes = run_notes(&clear, &dir.join("clear.jpg"), Format::Png, Format::Jpeg, &o);
    assert!(notes.iter().any(|n| n.contains("white")), "{notes:?}");
    let solid = dir.join("solid.png");
    gradient(8, 8, 255).save(&solid).unwrap();
    assert!(run_notes(&solid, &dir.join("solid.jpg"), Format::Png, Format::Jpeg, &o).is_empty());

    // Big image → ICO is scaled.
    let big = dir.join("big.png");
    gradient(300, 100, 255).save(&big).unwrap();
    let notes = run_notes(&big, &dir.join("big.ico"), Format::Png, Format::Ico, &o);
    assert!(notes.iter().any(|n| n.contains("256")), "{notes:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// An animated GIF with one solid-colour 8×8 frame per shade, 100 ms each.
fn animated_gif(path: &Path, shades: &[u8]) {
    let mut enc = image::codecs::gif::GifEncoder::new(File::create(path).unwrap());
    enc.set_repeat(image::codecs::gif::Repeat::Infinite).unwrap();
    for &shade in shades {
        let buf = image::RgbaImage::from_pixel(8, 8, image::Rgba([shade, 0, 0, 255]));
        let delay = image::Delay::from_numer_denom_ms(100, 1);
        enc.encode_frame(image::Frame::from_parts(buf, 0, 0, delay)).unwrap();
    }
}

#[test]
fn split_frames_writes_one_file_per_frame() {
    let dir = tmp("split");
    let anim = dir.join("anim.gif");
    animated_gif(&anim, &[10, 120, 240]);
    let opts = Options { split_frames: true, ..Default::default() };
    let (cancel, progress) = (AtomicBool::new(false), |_: f32| {});
    let ctx = ConvertCtx::new(&opts, &cancel, &progress);
    let dst = dir.join("anim.bmp");
    ImageConverter::default().convert(&anim, &dst, Format::Gif, Format::Bmp, &ctx).unwrap();

    let frames = dir.join("anim-frames");
    assert!(!dst.exists(), "frames replace the single output");
    assert_eq!(ctx.take_output(), Some(frames.join("anim-001.bmp")));
    for (i, shade) in [10u8, 120, 240].into_iter().enumerate() {
        let px = image::open(frames.join(format!("anim-{:03}.bmp", i + 1))).unwrap().to_rgba8().get_pixel(4, 4).0;
        assert!(px[0].abs_diff(shade) <= 2, "frame {i}: {px:?}");
    }
    assert!(ctx.take_notes().iter().any(|n| n.contains("3 images")));

    // A still image ignores the option.
    let still = dir.join("still.gif");
    image::RgbaImage::from_pixel(8, 8, image::Rgba([1, 2, 3, 255])).save(&still).unwrap();
    let ctx = ConvertCtx::new(&opts, &cancel, &progress);
    ImageConverter::default().convert(&still, &dir.join("still.png"), Format::Gif, Format::Png, &ctx).unwrap();
    assert!(dir.join("still.png").exists() && ctx.take_output().is_none());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn gif_encoder_keeps_every_frame_and_its_timing() {
    let dir = tmp("anim-out");
    let src = dir.join("src.gif");
    animated_gif(&src, &[30, 90, 150, 210]);
    let (cancel, progress, opts) = (AtomicBool::new(false), |_: f32| {}, Options::default());
    let ctx = ConvertCtx::new(&opts, &cancel, &progress);
    let data = ImageCrateDecoder(Format::Gif).decode(&src, usize::MAX, &ctx).unwrap();
    assert_eq!(data.frames.len(), 4);
    assert_eq!(data.frames[1].delay, std::time::Duration::from_millis(100));

    let dst = dir.join("out.gif");
    let mut out = BufWriter::new(File::create(&dst).unwrap());
    GifEncoder.encode(&data, &ctx, &mut out).unwrap();
    drop(out);
    let back = ImageCrateDecoder(Format::Gif).decode(&dst, usize::MAX, &ctx).unwrap();
    assert_eq!(back.frames.len(), 4);
    assert!(back.frames.iter().all(|f| f.delay == std::time::Duration::from_millis(100)));
    assert!(back.frames[3].image.to_rgba8().get_pixel(0, 0)[0].abs_diff(210) <= 2);

    // Only the first frame is decoded when that's all the target needs.
    let first = ImageCrateDecoder(Format::Gif).decode(&src, 1, &ctx).unwrap();
    assert!(first.frames.len() == 1 && first.more_frames);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn encoder_capabilities_match_format_metadata() {
    // The UI decides whether to offer "Export every frame" from `Format`; keep it honest.
    let c = ImageConverter::default();
    for e in &c.encoders {
        assert_eq!(e.animated(), e.format().animated_output(), "{}", e.format());
        assert_eq!(e.pages(), e.format().paged_output(), "{}", e.format());
        assert_eq!(e.format().kind(), dotty_core::Kind::Image);
    }
}

// ---- Formats, choices and multi-image handling ----

fn ctx_with(opts: &Options) -> (AtomicBool, impl Fn(f32)) {
    let _ = opts;
    (AtomicBool::new(false), |_: f32| {})
}

fn psnr(a: &image::RgbaImage, b: &image::RgbaImage) -> f64 {
    assert_eq!(a.dimensions(), b.dimensions());
    let mse = a.as_raw().iter().zip(b.as_raw()).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum::<f64>()
        / a.as_raw().len() as f64;
    if mse == 0.0 { 99.0 } else { 10.0 * (255.0f64.powi(2) / mse).log10() }
}

fn decode_all(path: &Path, from: Format) -> ImageData {
    let opts = Options::default();
    let (cancel, progress) = ctx_with(&opts);
    let ctx = ConvertCtx::new(&opts, &cancel, &progress);
    let c = ImageConverter::default();
    c.decoders.iter().find(|d| d.format() == from).unwrap().decode(path, usize::MAX, &ctx).unwrap()
}

fn with_choice(format: Format, choice: Choice, value: &str) -> Options {
    let mut o = Options::default();
    o.set_choice(format, &choice, value);
    o
}

#[test]
fn lossless_outputs_round_trip_exactly() {
    let dir = tmp("exact");
    let src = dir.join("src.png");
    let original = gradient(37, 23, 255);
    original.save(&src).unwrap();
    let lossless_webp = with_choice(Format::Webp, crate::encode_multi::WEBP_COMPRESSION, "lossless");
    for (to, opts) in [
        (Format::Tga, Options::default()),
        (Format::Qoi, Options::default()),
        (Format::Pnm, Options::default()),
        (Format::Jxl, Options::default()),
        (Format::Exr, Options::default()),
        (Format::Tiff, with_choice(Format::Tiff, crate::encode_multi::TIFF_COMPRESSION, "deflate")),
        (Format::Tiff, with_choice(Format::Tiff, crate::encode_multi::TIFF_COMPRESSION, "none")),
        (Format::Webp, lossless_webp),
    ] {
        let mid = dir.join(format!("m.{}", to.extension()));
        run(&src, &mid, Format::Png, to, &opts).unwrap_or_else(|e| panic!("{to}: {e}"));
        let back = dir.join("back.png");
        run(&mid, &back, to, Format::Png, &Options::default()).unwrap();
        assert_eq!(image::open(&back).unwrap().to_rgba8(), original, "{to} should be lossless");
    }
    // Radiance HDR keeps 8 bits of mantissa: close, not exact.
    run(&src, &dir.join("m.hdr"), Format::Png, Format::Hdr, &Options::default()).unwrap();
    run(&dir.join("m.hdr"), &dir.join("hdr.png"), Format::Hdr, Format::Png, &Options::default()).unwrap();
    assert!(psnr(&image::open(dir.join("hdr.png")).unwrap().to_rgba8(), &original) > 40.0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn sixteen_bit_survives_png_jxl_tiff() {
    let dir = tmp("deep");
    let src = dir.join("deep.png");
    let img = image::ImageBuffer::from_fn(20, 10, |x, y| image::Rgb([x as u16 * 3000, y as u16 * 6000 + 7, 12345u16]));
    DynamicImage::ImageRgb16(img.clone()).save(&src).unwrap();
    for to in [Format::Jxl, Format::Tiff] {
        let mid = dir.join(format!("m.{}", to.extension()));
        run(&src, &mid, Format::Png, to, &Options::default()).unwrap();
        let back = dir.join("b.png");
        run(&mid, &back, to, Format::Png, &Options::default()).unwrap();
        assert_eq!(image::open(&back).unwrap().to_rgb16(), img, "{to} keeps 16 bits");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn decoders_match_reference_tools() {
    let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for (file, from, min_db) in [
        ("rgba.psd", Format::Psd, 55.0),
        ("cmyk.psd", Format::Psd, 50.0),
        ("dxt5.dds", Format::Dds, 45.0),
        ("alpha.avif", Format::Avif, 40.0),
        ("lossy.jxl", Format::Jxl, 40.0),
    ] {
        let ours = decode_all(&fx.join(file), from).first().to_rgba8();
        let reference = image::open(fx.join(format!("{file}.ref.png"))).unwrap().to_rgba8();
        let db = psnr(&ours, &reference);
        assert!(db >= min_db, "{file}: {db:.1} dB against the reference decoder");
    }
}

#[test]
fn jpeg_xl_made_from_jpeg_rebuilds_the_original_bytes() {
    let fx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let dir = tmp("rebuild");
    let out = dir.join("out.jpg");
    let notes = run_notes(&fx.join("from-jpeg.jxl"), &out, Format::Jxl, Format::Jpeg, &Options::default());
    assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(fx.join("photo.jpg")).unwrap());
    assert!(notes.iter().any(|n| n.contains("Rebuilt")), "{notes:?}");
    // Stripping metadata needs a real re-encode.
    let stripped = Options { strip_metadata: true, ..Default::default() };
    run(&fx.join("from-jpeg.jxl"), &out, Format::Jxl, Format::Jpeg, &stripped).unwrap();
    assert_ne!(std::fs::read(&out).unwrap(), std::fs::read(fx.join("photo.jpg")).unwrap());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn avif_output_is_faithful_and_keeps_alpha() {
    let dir = tmp("avif");
    let src = dir.join("a.png");
    let original = gradient(48, 32, 180);
    original.save(&src).unwrap();
    let fast = with_choice(Format::Avif, crate::encode::AVIF_SPEED, "fast");
    run(&src, &dir.join("a.avif"), Format::Png, Format::Avif, &fast).unwrap();
    let back = decode_all(&dir.join("a.avif"), Format::Avif).first().to_rgba8();
    assert!(psnr(&back, &original) > 30.0, "{}", psnr(&back, &original));
    assert!(back.pixels().all(|p| p[3].abs_diff(180) <= 3), "alpha kept");
    std::fs::remove_dir_all(&dir).ok();
}

/// An animated GIF with the given shades that plays `plays` times (0 = forever).
fn looping_gif(path: &Path, shades: &[u8], plays: u16) {
    let mut enc = image::codecs::gif::GifEncoder::new(File::create(path).unwrap());
    let repeat = if plays == 0 { image::codecs::gif::Repeat::Infinite } else { image::codecs::gif::Repeat::Finite(plays - 1) };
    enc.set_repeat(repeat).unwrap();
    for &shade in shades {
        let buf = image::RgbaImage::from_pixel(8, 8, image::Rgba([shade, 255 - shade, 0, 255]));
        enc.encode_frame(image::Frame::from_parts(buf, 0, 0, image::Delay::from_numer_denom_ms(120, 1))).unwrap();
    }
}

#[test]
fn animation_survives_webp_and_apng_with_timing_and_loop_count() {
    let dir = tmp("anim-chain");
    let gif = dir.join("a.gif");
    looping_gif(&gif, &[20, 140, 250], 3);
    assert_eq!(decode_all(&gif, Format::Gif).loops, 3);

    let o = Options::default();
    run(&gif, &dir.join("a.webp"), Format::Gif, Format::Webp, &o).unwrap();
    run(&dir.join("a.webp"), &dir.join("a.png"), Format::Webp, Format::Png, &o).unwrap();
    run(&dir.join("a.png"), &dir.join("b.gif"), Format::Png, Format::Gif, &o).unwrap();
    for (path, from) in [(dir.join("a.webp"), Format::Webp), (dir.join("a.png"), Format::Png), (dir.join("b.gif"), Format::Gif)] {
        let data = decode_all(&path, from);
        assert_eq!(data.frames.len(), 3, "{from}");
        assert_eq!(data.loops, 3, "{from} loop count");
        assert!(data.frames.iter().all(|f| f.delay == std::time::Duration::from_millis(120)), "{from} timing");
        let px = data.frames[2].image.to_rgba8().get_pixel(4, 4).0;
        assert!(px[0].abs_diff(250) <= 8 && px[1].abs_diff(5) <= 8, "{from}: {px:?}");
    }

    // "First frame only" makes a still image, without a warning (it was asked for).
    let still = with_choice(Format::Webp, ANIMATION, "first");
    let notes = run_notes(&gif, &dir.join("still.webp"), Format::Gif, Format::Webp, &still);
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(decode_all(&dir.join("still.webp"), Format::Webp).frames.len(), 1);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn tiff_pages_round_trip_and_never_become_an_animation() {
    let dir = tmp("pages");
    let (cancel, progress, opts) = (AtomicBool::new(false), |_: f32| {}, Options::default());
    let ctx = ConvertCtx::new(&opts, &cancel, &progress);
    let mut data = ImageData::still(DynamicImage::ImageRgb8(image::RgbImage::from_pixel(30, 20, image::Rgb([200, 10, 10]))));
    data.frames.push(Frame { image: DynamicImage::ImageLuma8(image::GrayImage::from_pixel(12, 40, image::Luma([90]))), delay: Default::default() });
    data.frames.push(Frame { image: DynamicImage::ImageRgba8(gradient(16, 16, 128)), delay: Default::default() });
    data.pages = true;
    let tiff = dir.join("p.tiff");
    let mut out = BufWriter::new(File::create(&tiff).unwrap());
    TiffEncoder.encode(&data, &ctx, &mut out).unwrap();
    drop(out);

    let back = decode_all(&tiff, Format::Tiff);
    assert!(back.pages);
    let sizes: Vec<_> = back.frames.iter().map(|f| (f.image.width(), f.image.height())).collect();
    assert_eq!(sizes, [(30, 20), (12, 40), (16, 16)]);
    assert_eq!(back.frames[2].image.to_rgba8(), gradient(16, 16, 128));

    // Pages can't animate: TIFF → GIF keeps the first page and says so.
    let notes = run_notes(&tiff, &dir.join("p.gif"), Format::Tiff, Format::Gif, &opts);
    assert!(notes.iter().any(|n| n.contains("first page")), "{notes:?}");
    // …and an animation becomes pages in a TIFF.
    let gif = dir.join("a.gif");
    looping_gif(&gif, &[0, 100, 200, 250], 0);
    run(&gif, &dir.join("a.tiff"), Format::Gif, Format::Tiff, &opts).unwrap();
    assert_eq!(decode_all(&dir.join("a.tiff"), Format::Tiff).frames.len(), 4);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn ico_holds_a_set_of_sizes_unless_asked_for_one() {
    let dir = tmp("icosizes");
    let src = dir.join("i.png");
    gradient(64, 64, 255).save(&src).unwrap();
    run(&src, &dir.join("set.ico"), Format::Png, Format::Ico, &Options::default()).unwrap();
    let sizes: Vec<u32> = decode_all(&dir.join("set.ico"), Format::Ico).frames.iter().map(|f| f.image.width()).collect();
    assert_eq!(sizes, [64, 48, 32, 24, 16]);
    let single = with_choice(Format::Ico, crate::encode::ICO_SIZES, "single");
    run(&src, &dir.join("one.ico"), Format::Png, Format::Ico, &single).unwrap();
    assert_eq!(decode_all(&dir.join("one.ico"), Format::Ico).frames.len(), 1);

    // Exporting every frame of an icon gives one file per size.
    let split = Options { split_frames: true, ..Default::default() };
    let notes = run_notes(&dir.join("set.ico"), &dir.join("set.png"), Format::Ico, Format::Png, &split);
    assert!(notes.iter().any(|n| n.contains("5 images")), "{notes:?}");
    assert_eq!(std::fs::read_dir(dir.join("set-frames")).unwrap().count(), 5);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn svg_renders_at_the_chosen_size() {
    let dir = tmp("svg");
    let svg = dir.join("s.svg");
    std::fs::write(&svg, r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><rect width="40" height="20" fill="#ff0000"/></svg>"##).unwrap();
    run(&svg, &dir.join("a.png"), Format::Svg, Format::Png, &Options::default()).unwrap();
    let a = image::open(dir.join("a.png")).unwrap().to_rgba8();
    assert_eq!(a.dimensions(), (40, 20));
    assert_eq!(a.get_pixel(20, 10).0, [255, 0, 0, 255]);
    let big = with_choice(Format::Svg, crate::decode_extra::SVG_SIZE, "1024");
    run(&svg, &dir.join("b.png"), Format::Svg, Format::Png, &big).unwrap();
    assert_eq!(image::image_dimensions(dir.join("b.png")).unwrap(), (1024, 512));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn hdr_highlights_are_compressed_or_clipped_on_request() {
    let dir = tmp("tonemap");
    let exr = dir.join("h.exr");
    // Left half in normal range, right half 8× brighter than white.
    let img = image::Rgb32FImage::from_fn(8, 4, |x, _| if x < 4 { image::Rgb([0.2, 0.2, 0.2]) } else { image::Rgb([8.0, 8.0, 8.0]) });
    DynamicImage::ImageRgb32F(img).save(&exr).unwrap();
    let to_png = |opts: &Options| {
        run(&exr, &dir.join("o.png"), Format::Exr, Format::Png, opts).unwrap();
        image::open(dir.join("o.png")).unwrap().to_rgb8()
    };
    let clip = to_png(&with_choice(Format::Exr, crate::color::HIGHLIGHTS, "clip"));
    let compress = to_png(&Options::default());
    assert_eq!(clip.get_pixel(6, 1).0, [255, 255, 255]);
    assert_eq!(clip.get_pixel(1, 1).0[0], 124, "0.2 linear is sRGB 124");
    assert!(compress.get_pixel(6, 1).0[0] >= 250, "brightest stays white");
    assert!(compress.get_pixel(1, 1).0[0] < 124, "midtones compressed: {:?}", compress.get_pixel(1, 1));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn every_choice_is_well_formed() {
    let c = ImageConverter::default();
    for f in Format::ALL {
        for choice in c.choices(f).into_iter().chain(c.source_choices(f)) {
            assert!(choice.values.len() >= 2, "{f} {}", choice.key);
            let mut ids: Vec<_> = choice.values.iter().map(|v| v.0).collect();
            ids.dedup();
            assert_eq!(ids.len(), choice.values.len(), "{f} {} has duplicate values", choice.key);
        }
    }
}

#[test]
fn multi_image_probe_finds_animations_pages_and_icon_sets() {
    let dir = tmp("probe");
    let c = ImageConverter::default();
    looping_gif(&dir.join("a.gif"), &[1, 2], 0);
    image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255])).save(dir.join("s.gif")).unwrap();
    gradient(40, 40, 255).save(dir.join("i.png")).unwrap();
    run(&dir.join("a.gif"), &dir.join("a.png"), Format::Gif, Format::Png, &Options::default()).unwrap();
    run(&dir.join("a.gif"), &dir.join("a.tiff"), Format::Gif, Format::Tiff, &Options::default()).unwrap();
    run(&dir.join("i.png"), &dir.join("i.ico"), Format::Png, Format::Ico, &Options::default()).unwrap();
    assert!(c.multi_image(&dir.join("a.gif"), Format::Gif));
    assert!(!c.multi_image(&dir.join("s.gif"), Format::Gif));
    assert!(c.multi_image(&dir.join("a.png"), Format::Png), "APNG");
    assert!(!c.multi_image(&dir.join("i.png"), Format::Png));
    assert!(c.multi_image(&dir.join("a.tiff"), Format::Tiff));
    assert!(c.multi_image(&dir.join("i.ico"), Format::Ico));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn edits_apply_in_the_pipeline_including_same_format() {
    let dir = tmp("edits");
    let src = dir.join("a.png");
    image::RgbImage::from_pixel(8, 8, image::Rgb([200, 100, 50])).save(&src).unwrap();
    let mut keep = vec![255u8; 64];
    keep[..8].fill(0); // remove the top row
    let edits = dotty_core::Edits {
        ops: vec![dotty_core::EditOp::Erase { frame: 0, mask: dotty_core::Mask::new(8, 8, keep) }],
    };
    let (cancel, progress, opts) = (AtomicBool::new(false), |_: f32| {}, Options::default());
    let ctx = ConvertCtx::new(&opts, &cancel, &progress).with_edits(Some(&edits));
    let dst = dir.join("a (1).png");
    ImageConverter::default().convert(&src, &dst, Format::Png, Format::Png, &ctx).unwrap();
    let out = image::open(&dst).unwrap().to_rgba8();
    assert_eq!(out.get_pixel(3, 0).0[3], 0);
    assert_eq!(out.get_pixel(3, 1).0, [200, 100, 50, 255]);

    // A format without transparency gets the removed area filled in, with a note.
    let ctx = ConvertCtx::new(&opts, &cancel, &progress).with_edits(Some(&edits));
    ImageConverter::default().convert(&src, &dir.join("a.jpg"), Format::Png, Format::Jpeg, &ctx).unwrap();
    assert!(!ctx.take_notes().is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn animation_keeps_a_different_erase_per_frame() {
    let dir = tmp("anim-edits");
    let gif = dir.join("a.gif");
    animated_gif(&gif, &[50, 150, 250]);

    // The editor sees every frame, within its pixel budget.
    let (frames, full) = crate::edit::decode_for_edit(&gif, Format::Gif, &Options::default(), 4096, 3 * 16).unwrap();
    assert_eq!((frames.len(), full), (3, (8, 8)));
    assert_eq!(frames[0].dimensions(), (4, 4), "3 frames in 48 pixels: 4x4 each");

    // Frame 0 loses its top-left quarter, frame 2 its bottom-right; frame 1 is untouched.
    let quarter = |at: (u32, u32)| {
        let data = (0..16).map(|i| if (i % 4 / 2, i / 4 / 2) == at { 0 } else { 255 }).collect();
        dotty_core::Mask::new(4, 4, data)
    };
    let edits = dotty_core::Edits {
        ops: vec![
            dotty_core::EditOp::Erase { frame: 0, mask: quarter((0, 0)) },
            dotty_core::EditOp::Erase { frame: 2, mask: quarter((1, 1)) },
        ],
    };
    let opts = with_choice(Format::Webp, ANIMATION, "all");
    let (cancel, progress) = (AtomicBool::new(false), |_: f32| {});
    let ctx = ConvertCtx::new(&opts, &cancel, &progress).with_edits(Some(&edits));
    let out = dir.join("a.webp");
    ImageConverter::default().convert(&gif, &out, Format::Gif, Format::Webp, &ctx).unwrap();

    let back = decode_all(&out, Format::Webp);
    assert_eq!(back.frames.len(), 3, "still animated");
    let alpha = |f: usize, x: u32, y: u32| back.frames[f].image.to_rgba8().get_pixel(x, y).0[3];
    assert_eq!((alpha(0, 1, 1), alpha(0, 6, 6)), (0, 255));
    assert_eq!((alpha(1, 1, 1), alpha(1, 6, 6)), (255, 255));
    assert_eq!((alpha(2, 1, 1), alpha(2, 6, 6)), (255, 0));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_favicon_pack_has_every_icon_in_a_folder_or_zip() {
    let dir = tmp("favicon");
    let src = dir.join("logo.png");
    // A wide picture: it is centred on a square, not stretched.
    image::RgbaImage::from_pixel(600, 300, image::Rgba([200, 30, 30, 255])).save(&src).unwrap();
    let (cancel, progress) = (AtomicBool::new(false), |_: f32| {});
    let conv = ImageConverter::default();
    assert!(conv.routes().contains(&(Format::Png, Format::Favicon)) && conv.routes().contains(&(Format::Svg, Format::Favicon)));

    let opts = Options::default();
    assert!(conv.many_files(&src, Format::Png, Format::Favicon, &opts), "the app asks folder or zip");
    let ctx = ConvertCtx::new(&opts, &cancel, &progress);
    let dst = dir.join("logo.favicon");
    conv.convert(&src, &dst, Format::Png, Format::Favicon, &ctx).unwrap();
    let folder = dir.join("logo-favicons");
    assert_eq!(ctx.take_output(), Some(folder.join("favicon.ico")));
    for name in ["favicon.ico", "favicon-16x16.png", "favicon-32x32.png", "apple-touch-icon.png", "android-chrome-192x192.png", "android-chrome-512x512.png", "site.webmanifest", "favicon-snippet.html"] {
        assert!(folder.join(name).exists(), "{name}");
    }
    let big = image::open(folder.join("android-chrome-512x512.png")).unwrap().to_rgba8();
    assert_eq!(big.dimensions(), (512, 512));
    assert_eq!(big.get_pixel(256, 256).0[3], 255, "the picture is in the middle");
    assert_eq!(big.get_pixel(256, 10).0[3], 0, "and the padding above it is transparent");
    let icons = decode_ico_sizes(&folder.join("favicon.ico"));
    assert_eq!(icons, vec![48, 32, 16]);
    assert!(ctx.take_notes().iter().any(|n| n.contains("favicon pack of 8 files")));

    // As a zip.
    let zip_opts = Options { bundle: Some(dotty_core::BundleMode::Zip), ..Default::default() };
    let ctx = ConvertCtx::new(&zip_opts, &cancel, &progress);
    conv.convert(&src, &dir.join("logo.favicon"), Format::Png, Format::Favicon, &ctx).unwrap();
    assert_eq!(ctx.take_output(), Some(dir.join("logo-favicons.zip")));
    std::fs::remove_dir_all(&dir).ok();
}

fn decode_ico_sizes(path: &Path) -> Vec<u32> {
    let bytes = std::fs::read(path).unwrap();
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let mut sizes: Vec<u32> = (0..count).map(|i| bytes[6 + i * 16] as u32).collect();
    sizes.sort_by(|a, b| b.cmp(a));
    sizes
}
