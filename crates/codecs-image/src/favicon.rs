//! The "favicon pack" target: a picture becomes everything a website needs for its icon,
//! in a folder or zip: `favicon.ico` (16, 32, 48), PNGs for browsers, phones and tablets,
//! a web manifest, and the HTML to paste into `<head>`.

use crate::{ImageData, edit};
use dotty_core::{Bundle, ConvertCtx, Error, Result};
use image::{DynamicImage, ExtendedColorType, RgbaImage, codecs::ico, imageops};
use std::io::Cursor;
use std::path::Path;

/// `(file name, side)` of each PNG in the pack.
pub const PNGS: [(&str, u32); 5] = [("favicon-16x16.png", 16), ("favicon-32x32.png", 32), ("apple-touch-icon.png", 180), ("android-chrome-192x192.png", 192), ("android-chrome-512x512.png", 512)];
const ICO_SIZES: [u32; 3] = [16, 32, 48];

/// The picture centred on a transparent square, so nothing is cropped or stretched.
fn square(img: &DynamicImage) -> RgbaImage {
    let rgba = img.to_rgba8();
    let side = rgba.width().max(rgba.height()).max(1);
    let mut canvas = RgbaImage::new(side, side);
    imageops::overlay(&mut canvas, &rgba, ((side - rgba.width()) / 2) as i64, ((side - rgba.height()) / 2) as i64);
    canvas
}

fn png(img: &RgbaImage) -> Result<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).map_err(|e| Error::Codec(format!("PNG: {e}")))?;
    Ok(out.into_inner())
}

const MANIFEST: &str = r##"{
  "name": "",
  "short_name": "",
  "icons": [
    { "src": "/android-chrome-192x192.png", "sizes": "192x192", "type": "image/png" },
    { "src": "/android-chrome-512x512.png", "sizes": "512x512", "type": "image/png" }
  ],
  "theme_color": "#ffffff",
  "background_color": "#ffffff",
  "display": "standalone"
}
"##;

const SNIPPET: &str = r#"<!-- Put the files at the root of your site, and these lines in <head>. -->
<link rel="icon" href="/favicon.ico" sizes="48x48">
<link rel="icon" type="image/png" sizes="32x32" href="/favicon-32x32.png">
<link rel="icon" type="image/png" sizes="16x16" href="/favicon-16x16.png">
<link rel="apple-touch-icon" sizes="180x180" href="/apple-touch-icon.png">
<link rel="manifest" href="/site.webmanifest">
"#;

/// Write the pack for `img` beside `dst`.
pub fn write(mut img: ImageData, dst: &Path, ctx: &ConvertCtx) -> Result<()> {
    if let Some(edits) = ctx.edits {
        edit::apply(&mut img, edits);
    }
    let base = square(img.first());
    if base.width() < 512 {
        ctx.note(format!("The picture is {0}×{0} pixels; the larger icons (up to 512) were scaled up from it, so they will look soft", base.width()));
    }
    let mut bundle = Bundle::create(dst, "favicons", ctx.options.bundle.unwrap_or_default())?;
    let result = (|| {
        let scaled = |side: u32| imageops::resize(&base, side, side, imageops::FilterType::Lanczos3);
        let frames = ICO_SIZES
            .iter()
            .map(|&s| {
                let i = scaled(s);
                ico::IcoFrame::as_png(i.as_raw(), s, s, ExtendedColorType::Rgba8).map_err(|e| Error::Codec(format!("ICO: {e}")))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut ico_bytes = Cursor::new(Vec::new());
        ico::IcoEncoder::new(&mut ico_bytes).encode_images(&frames).map_err(|e| Error::Codec(format!("ICO: {e}")))?;
        bundle.add("favicon.ico", ico_bytes.get_ref())?;
        for (name, side) in PNGS {
            ctx.check_cancelled()?;
            bundle.add(name, &png(&scaled(side))?)?;
        }
        bundle.add("site.webmanifest", MANIFEST.as_bytes())?;
        bundle.add("favicon-snippet.html", SNIPPET.as_bytes())?;
        Ok::<(), Error>(())
    })();
    match result {
        Ok(()) => {
            let (name, n) = (bundle.name(), bundle.count());
            let zip = ctx.options.bundle == Some(dotty_core::BundleMode::Zip);
            ctx.note(format!("Saved a favicon pack of {n} files in the {} \"{name}\"", if zip { "zip" } else { "folder" }));
            ctx.set_output(bundle.finish()?);
            Ok(())
        }
        Err(e) => {
            bundle.abort();
            Err(e)
        }
    }
}
