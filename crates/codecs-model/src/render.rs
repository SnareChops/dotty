//! A small software renderer for previews and "3D model → picture": orthographic view of
//! the whole model, z-buffer, smooth shading from a headlight plus a key light, optional
//! wireframe. Triangles are never sorted, so it stays fast on big meshes.

use crate::mesh::*;
use image::{Rgba, RgbaImage};

/// Orbit angles (radians) and zoom (1 = the whole model fits).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub yaw: f32,
    pub pitch: f32,
    pub zoom: f32,
}

impl Camera {
    /// From the front-right and a little above: how a product shot is usually taken.
    pub const ISO: Camera = Camera { yaw: 0.6, pitch: 0.5, zoom: 1.0 };

    pub fn named(view: &str) -> Camera {
        use std::f32::consts::{FRAC_PI_2, PI};
        let (yaw, pitch) = match view {
            "front" => (0.0, 0.0),
            "back" => (PI, 0.0),
            "left" => (-FRAC_PI_2, 0.0),
            "right" => (FRAC_PI_2, 0.0),
            "top" => (0.0, FRAC_PI_2),
            _ => (Camera::ISO.yaw, Camera::ISO.pitch),
        };
        Camera { yaw, pitch, zoom: 1.0 }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub wireframe: bool,
    /// Draw at most this many triangles (an even sample of the rest) so huge models stay quick.
    pub max_triangles: usize,
    /// Draw this many times larger and shrink, for smooth edges.
    pub supersample: u32,
    /// Top and bottom of the background.
    pub background: [[u8; 3]; 2],
}

impl Default for Options {
    fn default() -> Self {
        Self { wireframe: false, max_triangles: usize::MAX, supersample: 1, background: [[246, 247, 250], [214, 218, 226]] }
    }
}

pub struct Rendered {
    pub image: RgbaImage,
    /// Triangles drawn, of `total`.
    pub drawn: usize,
    pub total: usize,
}

const BASE: [f32; 3] = [188.0, 197.0, 214.0];

/// Models up to this many triangles get per-corner crease shading.
const CREASE_LIMIT: usize = 400_000;

/// Neighbouring faces within this angle (cosine of 40°) are smoothed together.
const CREASE_COS: f32 = 0.766;

/// Light for every corner of every triangle, smoothing only across faces at gentle angles.
fn creased_shade(mesh: &Mesh, lit: &dyn Fn(V3) -> f32) -> Vec<[f32; 3]> {
    let face: Vec<V3> = mesh.triangles.iter().map(|&t| mesh.face_vector(t)).collect();
    let unit: Vec<V3> = face.iter().map(|&f| normalize(f)).collect();
    // Faces around each vertex, as one flat list per vertex (counting sort).
    let mut start = vec![0u32; mesh.positions.len() + 1];
    for t in &mesh.triangles {
        for &v in t {
            start[v as usize + 1] += 1;
        }
    }
    for i in 0..mesh.positions.len() {
        start[i + 1] += start[i];
    }
    let mut fill = start.clone();
    let mut around = vec![0u32; mesh.triangles.len() * 3];
    for (f, t) in mesh.triangles.iter().enumerate() {
        for &v in t {
            around[fill[v as usize] as usize] = f as u32;
            fill[v as usize] += 1;
        }
    }
    mesh.triangles
        .iter()
        .enumerate()
        .map(|(f, t)| {
            t.map(|v| {
                let mut sum = [0.0f32; 3];
                for &g in &around[start[v as usize] as usize..start[v as usize + 1] as usize] {
                    if dot(unit[g as usize], unit[f]) >= CREASE_COS {
                        let w = face[g as usize];
                        sum = [sum[0] + w[0], sum[1] + w[1], sum[2] + w[2]];
                    }
                }
                lit(if dot(sum, sum) > 0.0 { sum } else { unit[f] })
            })
        })
        .collect()
}

fn rotate(c: &Camera, v: V3) -> V3 {
    let (sy, cy) = c.yaw.sin_cos();
    let (sp, cp) = c.pitch.sin_cos();
    // Yaw about Y, then pitch about X.
    let (x, z) = (v[0] * cy + v[2] * sy, -v[0] * sy + v[2] * cy);
    [x, v[1] * cp - z * sp, v[1] * sp + z * cp]
}

/// Intensity of the light on a surface turned `n` (unit, in view space; faces the camera or not).
fn light(n: V3) -> f32 {
    let n = if n[2] < 0.0 { [-n[0], -n[1], -n[2]] } else { n };
    const KEY: V3 = [-0.38, 0.62, 0.69];
    (0.30 + 0.52 * n[2].max(0.0) + 0.30 * dot(n, KEY).max(0.0)).min(1.15)
}

pub fn render(mesh: &Mesh, cam: &Camera, w: u32, h: u32, opts: &Options) -> Rendered {
    let ss = opts.supersample.clamp(1, 4);
    let (rw, rh) = ((w * ss).max(1), (h * ss).max(1));
    let mut img = RgbaImage::new(rw, rh);
    for y in 0..rh {
        let t = y as f32 / (rh - 1).max(1) as f32;
        let c = |k: usize| (opts.background[0][k] as f32 * (1.0 - t) + opts.background[1][k] as f32 * t) as u8;
        for x in 0..rw {
            img.put_pixel(x, y, Rgba([c(0), c(1), c(2), 255]));
        }
    }
    let total = mesh.triangles.len();
    let Some((lo, hi)) = mesh.bounds() else { return Rendered { image: finish(img, w, h), drawn: 0, total } };

    // Y-up, centred, scaled so the model fits a unit sphere.
    let y_up = |p: V3| if mesh.up == Up::Z { [p[0], p[2], -p[1]] } else { p };
    let (lo, hi) = (y_up(lo), y_up(hi));
    let (a, b) = ([lo[0].min(hi[0]), lo[1].min(hi[1]), lo[2].min(hi[2])], [lo[0].max(hi[0]), lo[1].max(hi[1]), lo[2].max(hi[2])]);
    let centre = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0, (a[2] + b[2]) / 2.0];
    let radius = (dot(sub(b, a), sub(b, a)).sqrt() / 2.0).max(1e-12);
    let scale = rw.min(rh) as f32 / 2.0 * 0.9 * cam.zoom;
    let to_screen = |p: V3| -> V3 {
        let v = rotate(cam, sub(y_up(p), centre).map(|c| c / radius));
        [rw as f32 / 2.0 + v[0] * scale, rh as f32 / 2.0 - v[1] * scale, v[2]]
    };
    let screen: Vec<V3> = mesh.positions.iter().map(|&p| to_screen(p)).collect();
    let lit = |n: V3| light(rotate(cam, normalize(y_up(n))));
    // Models that carry normals use them. Otherwise each corner of a triangle is smoothed only
    // with neighbours at gentle angles, so sharp edges stay sharp (up to a size, then a plain
    // smoothing per vertex).
    let corners = (!mesh.has_normals() && total <= CREASE_LIMIT).then(|| creased_shade(mesh, &lit));
    let shade: Vec<f32> = if corners.is_some() { Vec::new() } else if mesh.has_normals() { mesh.normals.iter().map(|&n| lit(n)).collect() } else { mesh.smooth_normals().iter().map(|&n| lit(n)).collect() };
    let color = |i: usize| -> V3 { if mesh.has_colors() { [mesh.colors[i][0] as f32, mesh.colors[i][1] as f32, mesh.colors[i][2] as f32] } else { BASE } };

    let mut depth = vec![f32::NEG_INFINITY; (rw * rh) as usize];
    let step = if total > opts.max_triangles && opts.max_triangles > 0 { total.div_ceil(opts.max_triangles) } else { 1 };
    let mut drawn = 0;
    for (ti, t) in mesh.triangles.iter().enumerate().step_by(step) {
        drawn += 1;
        let (p0, p1, p2) = (screen[t[0] as usize], screen[t[1] as usize], screen[t[2] as usize]);
        let area = (p1[0] - p0[0]) * (p2[1] - p0[1]) - (p2[0] - p0[0]) * (p1[1] - p0[1]);
        if area.abs() < 1e-9 {
            continue;
        }
        let (min_x, max_x) = (p0[0].min(p1[0]).min(p2[0]).floor().max(0.0) as i64, (p0[0].max(p1[0]).max(p2[0]).ceil() as i64).min(rw as i64 - 1));
        let (min_y, max_y) = (p0[1].min(p1[1]).min(p2[1]).floor().max(0.0) as i64, (p0[1].max(p1[1]).max(p2[1]).ceil() as i64).min(rh as i64 - 1));
        let (c0, c1, c2) = (color(t[0] as usize), color(t[1] as usize), color(t[2] as usize));
        let (s0, s1, s2) = match &corners {
            Some(c) => (c[ti][0], c[ti][1], c[ti][2]),
            None => (shade[t[0] as usize], shade[t[1] as usize], shade[t[2] as usize]),
        };
        let inv = 1.0 / area;
        for y in min_y..=max_y {
            for x in min_x..=max_x {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let w0 = ((p1[0] - px) * (p2[1] - py) - (p2[0] - px) * (p1[1] - py)) * inv;
                let w1 = ((p2[0] - px) * (p0[1] - py) - (p0[0] - px) * (p2[1] - py)) * inv;
                let w2 = 1.0 - w0 - w1;
                if w0 < -1e-4 || w1 < -1e-4 || w2 < -1e-4 {
                    continue;
                }
                let z = w0 * p0[2] + w1 * p1[2] + w2 * p2[2];
                let idx = (y as u32 * rw + x as u32) as usize;
                if z <= depth[idx] {
                    continue;
                }
                depth[idx] = z;
                let lit = w0 * s0 + w1 * s1 + w2 * s2;
                let px_color = |k: usize| ((w0 * c0[k] + w1 * c1[k] + w2 * c2[k]) * lit).clamp(0.0, 255.0) as u8;
                img.put_pixel(x as u32, y as u32, Rgba([px_color(0), px_color(1), px_color(2), 255]));
            }
        }
    }
    if total == 0 {
        // Points only: a dot for each.
        for p in &screen {
            for (dx, dy) in [(0i64, 0i64), (1, 0), (0, 1), (1, 1)] {
                let (x, y) = (p[0] as i64 + dx * ss as i64 / 2, p[1] as i64 + dy * ss as i64 / 2);
                if x >= 0 && y >= 0 && x < rw as i64 && y < rh as i64 {
                    let idx = (y as u32 * rw + x as u32) as usize;
                    if p[2] > depth[idx] {
                        depth[idx] = p[2];
                        img.put_pixel(x as u32, y as u32, Rgba([40, 60, 110, 255]));
                    }
                }
            }
        }
    }
    if opts.wireframe {
        let mut line = |a: V3, b: V3| {
            let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
            let n = dx.abs().max(dy.abs()).ceil().max(1.0) as i64;
            for k in 0..=n {
                let t = k as f32 / n as f32;
                let (x, y, z) = ((a[0] + dx * t) as i64, (a[1] + dy * t) as i64, a[2] + (b[2] - a[2]) * t);
                if x < 0 || y < 0 || x >= rw as i64 || y >= rh as i64 {
                    continue;
                }
                let idx = (y as u32 * rw + x as u32) as usize;
                if z + 0.01 >= depth[idx] {
                    let Rgba(p) = *img.get_pixel(x as u32, y as u32);
                    img.put_pixel(x as u32, y as u32, Rgba([(p[0] as f32 * 0.35) as u8, (p[1] as f32 * 0.4) as u8, (p[2] as f32 * 0.5 + 20.0) as u8, 255]));
                }
            }
        };
        for t in mesh.triangles.iter().step_by(step) {
            let (p0, p1, p2) = (screen[t[0] as usize], screen[t[1] as usize], screen[t[2] as usize]);
            line(p0, p1);
            line(p1, p2);
            line(p2, p0);
        }
    }
    Rendered { image: finish(img, w, h), drawn, total }
}

fn finish(img: RgbaImage, w: u32, h: u32) -> RgbaImage {
    if img.dimensions() == (w, h) { img } else { image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_background(p: &Rgba<u8>, o: &Options) -> bool {
        p.0[..3].iter().zip(o.background[0].iter()).all(|(a, b)| (*a as i32 - *b as i32).abs() < 40)
    }

    #[test]
    fn a_cube_is_drawn_in_the_middle_with_shaded_faces() {
        let o = Options::default();
        let r = render(&cube(), &Camera::ISO, 200, 150, &o);
        assert_eq!((r.image.dimensions(), r.drawn, r.total), ((200, 150), 12, 12));
        assert!(is_background(r.image.get_pixel(3, 3), &o), "corner is background");
        assert!(!is_background(r.image.get_pixel(100, 75), &o), "the cube is in the middle");
        // Three visible faces with three different brightnesses.
        let mut shades: Vec<u8> = (0..150).flat_map(|y| (0..200).map(move |x| (x, y))).map(|(x, y)| r.image.get_pixel(x, y).0[0]).filter(|v| *v < 215).collect();
        shades.sort();
        shades.dedup();
        assert!(shades.len() >= 3, "{} distinct shades", shades.len());
    }

    #[test]
    fn views_differ_and_the_front_of_a_z_up_model_faces_the_camera() {
        let o = Options::default();
        // A flat tall wall in the XZ plane (Z-up: a wall seen from the front = the -Y side).
        let mut wall = Mesh::new(Up::Z);
        wall.positions = vec![[0.0, 0.0, 0.0], [4.0, 0.0, 0.0], [4.0, 0.0, 3.0], [0.0, 0.0, 3.0]];
        wall.triangles = vec![[0, 1, 2], [0, 2, 3]];
        let covered = |cam: &Camera| render(&wall, cam, 120, 120, &o).image.pixels().filter(|p| !is_background(p, &o)).count();
        let (front, side, top) = (covered(&Camera::named("front")), covered(&Camera::named("right")), covered(&Camera::named("top")));
        assert!(front > 4000, "seen face on: {front}");
        assert!(side < 200 && top < 200, "edge on from the side and from above: {side} {top}");
    }

    #[test]
    fn vertex_colours_wireframes_points_and_sampling() {
        let o = Options::default();
        let mut m = cube();
        m.colors = vec![[255, 0, 0, 255]; 8];
        let red = render(&m, &Camera::ISO, 100, 100, &o).image.get_pixel(50, 50).0;
        assert!(red[0] > 100 && red[1] < 60, "{red:?}");
        let plain = render(&cube(), &Camera::ISO, 100, 100, &o).image;
        let wire = render(&cube(), &Camera::ISO, 100, 100, &Options { wireframe: true, ..o }).image;
        assert!(plain.pixels().zip(wire.pixels()).filter(|(a, b)| a != b).count() > 150, "edges are drawn");
        let mut cloud = Mesh::new(Up::Y);
        cloud.positions = (0..50).map(|i| [i as f32, (i * 7 % 13) as f32, 0.0]).collect();
        assert!(render(&cloud, &Camera::ISO, 100, 100, &o).image.pixels().any(|p| p.0[2] as i32 > p.0[0] as i32 + 50), "dots drawn for a point cloud");
        let r = render(&cube(), &Camera::ISO, 100, 100, &Options { max_triangles: 5, ..o });
        assert_eq!((r.drawn, r.total), (4, 12), "an even sample when there are too many: every third");
        let smooth = render(&cube(), &Camera::ISO, 100, 100, &Options { supersample: 2, ..o });
        assert_eq!(smooth.image.dimensions(), (100, 100));
        assert_eq!(render(&Mesh::default(), &Camera::ISO, 10, 10, &o).drawn, 0, "an empty model is just the background");
    }
}
