use super::*;

fn audio_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../codecs-audio/tests/fixtures").join(name)
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

/// Every picture, as RGB with rotation applied.
fn all_frames(path: &Path, from: Format) -> (VideoInfo, Vec<(f64, RgbImage)>) {
    let mut r = VideoReader::open(path, from, 2).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let info = r.info().clone();
    let mut out = Vec::new();
    while let Some(p) = r.next_picture().unwrap() {
        out.push((p.pts, rotate(p.to_rgb8(), info.rotation)));
    }
    (info, out)
}

/// Mean absolute difference per channel (0–255).
fn diff(a: &RgbImage, b: &RgbImage) -> f64 {
    assert_eq!(a.dimensions(), b.dimensions());
    let sum: u64 = a.as_raw().iter().zip(b.as_raw()).map(|(&x, &y)| x.abs_diff(y) as u64).sum();
    sum as f64 / a.as_raw().len() as f64
}

#[test]
fn every_codec_and_container_decodes_like_ffmpeg() {
    for (file, from, codec) in [
        ("video_opus.mp4", Format::Mp4, Codec::Av1),
        ("video_two_tracks.mkv", Format::Mkv, Codec::Vp9),
        ("video_silent.webm", Format::Webm, Codec::Vp9),
        ("video_pcm.mov", Format::Mov, Codec::Mjpeg),
        ("video_pcm.avi", Format::Avi, Codec::Mjpeg),
    ] {
        let (info, frames) = all_frames(&audio_fixture(file), from);
        assert_eq!(info.codec, codec, "{file}");
        assert_eq!(frames.len(), 5, "{file}: 0.5 s at 10 fps");
        assert!(frames.windows(2).all(|w| (w[1].0 - w[0].0 - 0.1).abs() < 0.002), "{file}: {:?}", frames.iter().map(|f| f.0).collect::<Vec<_>>());
        assert_eq!(frames[0].1.dimensions(), (64, 64), "{file}");
        assert_eq!((info.width, info.height), (64, 64), "{file}: size known before decoding");
        let mut r = VideoReader::open(&audio_fixture(file), from, 1).unwrap();
        r.keyframes().unwrap();
        let info = r.info();
        assert_eq!(info.frames, Some(5), "{file}");
        assert!(info.duration.is_some_and(|d| (d - 0.5).abs() < 0.02), "{file}: {:?}", info.duration);
        assert!(info.fps().is_some_and(|f| (f - 10.0).abs() < 0.5), "{file}: {:?}", info.fps());
        let reference = image::open(fixture(&format!("ref-{file}.png"))).unwrap().to_rgb8();
        let d = diff(&frames[2].1, &reference);
        // Chroma is sampled more simply than ffmpeg's scaler does, so edges differ a little.
        // JPEG decoders differ more among themselves on this tiny, heavily compressed 4:2:2
        // frame with sharp synthetic edges (libjpeg is 5–9.5 off ffmpeg here too).
        let limit = if codec == Codec::Mjpeg { 10.0 } else { 4.0 };
        assert!(d < limit, "{file}: frame 2 differs from ffmpeg's by {d:.2} on average");
    }
}

#[test]
fn seeking_lands_on_the_frame_shown_at_that_time() {
    for (file, from) in [("video_opus.mp4", Format::Mp4), ("video_two_tracks.mkv", Format::Mkv), ("video_pcm.avi", Format::Avi)] {
        let path = audio_fixture(file);
        let (_, frames) = all_frames(&path, from);
        let mut r = VideoReader::open(&path, from, 1).unwrap();
        assert_eq!(r.keyframes().unwrap()[0], 0.0);
        // 0.25 s falls between the frames at 0.2 and 0.3: the one at 0.2 is on screen.
        for (t, want) in [(0.25, 2), (0.0, 0), (0.4, 4), (0.1, 1), (9.0, 4)] {
            let got = r.frame_at(t).unwrap().unwrap();
            assert_eq!(got, frames[want].1, "{file} at {t} s");
        }
        // Reading on after a seek continues in order.
        r.seek(0.1).unwrap();
        let next: Vec<f64> = std::iter::from_fn(|| r.next_picture().unwrap()).map(|p| p.pts).collect();
        assert_eq!(next.len(), 4, "{file}: {next:?}");
    }
}

#[test]
fn rotation_from_the_track_header_is_applied() {
    let (info, frames) = all_frames(&fixture("rotated.mp4"), Format::Mp4);
    assert_eq!((info.width, info.height, info.display_size()), (64, 32, (32, 64)));
    let reference = image::open(fixture("ref-rotated.mp4.png")).unwrap().to_rgb8();
    let d = diff(&frames[2].1, &reference);
    assert!(d < 4.0, "rotated the same way as ffmpeg: {d:.2}");
}

#[test]
fn codecs_we_cant_decode_say_so() {
    let dir = std::env::temp_dir().join(format!("tf-video-unsup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // An MP4 whose video sample entry is H.264: rename the AV1 fixture's `av01` entry.
    let mut bytes = std::fs::read(audio_fixture("video_opus.mp4")).unwrap();
    // (The first "av01" is a brand in the file-type box; the sample entry follows "stsd".)
    let stsd = bytes.windows(4).position(|w| w == b"stsd").unwrap();
    let at = stsd + bytes[stsd..].windows(4).position(|w| w == b"av01").unwrap();
    bytes[at..at + 4].copy_from_slice(b"avc1");
    let h264 = dir.join("h264.mp4");
    std::fs::write(&h264, &bytes).unwrap();
    let err = VideoReader::open(&h264, Format::Mp4, 1).err().unwrap().to_string();
    assert!(err.contains("H.264") && err.contains("patent"), "{err}");
    std::fs::remove_dir_all(&dir).ok();
}
