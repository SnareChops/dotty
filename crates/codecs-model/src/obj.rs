//! Wavefront OBJ: vertices, normals and faces (polygons are cut into triangles). Texture
//! coordinates, materials and groups aren't kept.

use crate::mesh::*;
use dotty_core::{Error, Result};
use std::collections::HashMap;

pub struct Read {
    pub mesh: Mesh,
    pub notes: Vec<String>,
}

fn bad(line: usize, why: &str) -> Error {
    Error::Codec(format!("not a valid OBJ file (line {line}): {why}"))
}

pub fn read(text: &str) -> Result<Read> {
    let (mut pos, mut col, mut nor): (Vec<V3>, Vec<[f32; 3]>, Vec<V3>) = (Vec::new(), Vec::new(), Vec::new());
    let mut mesh = Mesh::new(Up::Y);
    let mut keys: HashMap<(u32, u32), u32> = HashMap::new();
    let (mut materials, mut textured, mut out_of_range) = (0, false, 0);
    let float = |w: Option<&str>, n: usize| -> Result<f32> { w.and_then(|w| w.parse().ok()).ok_or_else(|| bad(n, "a number is missing or unreadable")) };
    for (n, line) in text.lines().enumerate() {
        let n = n + 1;
        let line = line.split('#').next().unwrap_or("");
        let mut w = line.split_ascii_whitespace();
        match w.next() {
            Some("v") => {
                pos.push([float(w.next(), n)?, float(w.next(), n)?, float(w.next(), n)?]);
                // Some exporters append a colour.
                let rgb: Vec<f32> = w.take(3).filter_map(|x| x.parse().ok()).collect();
                if rgb.len() == 3 {
                    col.resize(pos.len() - 1, [1.0; 3]);
                    col.push([rgb[0], rgb[1], rgb[2]]);
                }
            }
            Some("vn") => nor.push([float(w.next(), n)?, float(w.next(), n)?, float(w.next(), n)?]),
            Some("vt") => textured = true,
            Some("usemtl") => materials += 1,
            Some("f") => {
                let mut corners: Vec<u32> = Vec::new();
                for token in w {
                    let mut parts = token.split('/');
                    let resolve = |s: Option<&str>, len: usize| -> Option<Option<u32>> {
                        match s {
                            None | Some("") => Some(None),
                            Some(s) => {
                                let i: i64 = s.parse().ok()?;
                                let r = if i < 0 { len as i64 + i } else { i - 1 };
                                (0..len as i64).contains(&r).then_some(Some(r as u32))
                            }
                        }
                    };
                    let (v, _texture, vn) = (parts.next(), parts.next(), parts.next());
                    let (Some(Some(vi)), Some(ni)) = (resolve(v, pos.len()), resolve(vn, nor.len())) else {
                        out_of_range += 1;
                        continue;
                    };
                    let key = (vi, ni.unwrap_or(u32::MAX));
                    let index = *keys.entry(key).or_insert_with(|| {
                        mesh.positions.push(pos[vi as usize]);
                        if let Some(ni) = ni {
                            mesh.normals.resize(mesh.positions.len() - 1, [0.0; 3]);
                            mesh.normals.push(nor[ni as usize]);
                        }
                        if let Some(c) = col.get(vi as usize) {
                            mesh.colors.resize(mesh.positions.len() - 1, [255; 4]);
                            let k = if c.iter().any(|x| *x > 1.0) { 1.0 } else { 255.0 };
                            mesh.colors.push([(c[0] * k).round() as u8, (c[1] * k).round() as u8, (c[2] * k).round() as u8, 255]);
                        }
                        mesh.positions.len() as u32 - 1
                    });
                    corners.push(index);
                }
                for k in 1..corners.len().saturating_sub(1) {
                    mesh.triangles.push([corners[0], corners[k], corners[k + 1]]);
                }
            }
            _ => {}
        }
    }
    // Vertices made before a normal or colour turned up are padded, so lengths match or are zero.
    if mesh.normals.len() != mesh.positions.len() {
        mesh.normals.clear();
    }
    if mesh.colors.len() != mesh.positions.len() {
        mesh.colors.clear();
    }
    if pos.is_empty() {
        return Err(bad(1, "no vertices found"));
    }
    if mesh.triangles.is_empty() {
        // Points only: keep them all.
        mesh.positions = pos;
        mesh.colors = col.iter().map(|c| [(c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8, 255]).collect();
        if mesh.colors.len() != mesh.positions.len() {
            mesh.colors.clear();
        }
    }
    let mut notes = Vec::new();
    if materials > 0 || textured {
        notes.push("Materials and texture coordinates in the OBJ weren't kept".to_string());
    }
    if out_of_range > 0 {
        notes.push(format!("{out_of_range} face corner(s) pointing at missing vertices were skipped"));
    }
    Ok(Read { mesh, notes })
}

pub fn write(m: &Mesh) -> String {
    let mut out = String::from("# Written by Dotty\n");
    for (i, p) in m.positions.iter().enumerate() {
        if m.has_colors() {
            let c = m.colors[i];
            out.push_str(&format!("v {} {} {} {} {} {}\n", p[0], p[1], p[2], c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0));
        } else {
            out.push_str(&format!("v {} {} {}\n", p[0], p[1], p[2]));
        }
    }
    if m.has_normals() {
        for n in &m.normals {
            out.push_str(&format!("vn {} {} {}\n", n[0], n[1], n[2]));
        }
    }
    for t in &m.triangles {
        let i = |k: usize| t[k] + 1;
        if m.has_normals() {
            out.push_str(&format!("f {0}//{0} {1}//{1} {2}//{2}\n", i(0), i(1), i(2)));
        } else {
            out.push_str(&format!("f {} {} {}\n", i(0), i(1), i(2)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cube_round_trips_with_normals_and_colours() {
        let mut m = cube();
        m.up = Up::Y;
        m.normals = m.smooth_normals();
        m.colors = (0..8).map(|i| [i * 30, 100, 255 - i * 30, 255]).collect();
        let back = read(&write(&m)).unwrap().mesh;
        assert_eq!((back.positions.len(), back.triangles.len(), back.up), (8, 12, Up::Y));
        assert!(back.has_normals() && back.has_colors());
        assert_eq!(back.colors[3], m.colors[3]);
        // Vertices come back in the order the faces use them: same set, same geometry.
        let (mut a, mut b) = (back.positions.clone(), m.positions.clone());
        a.sort_by(|x, y| x.partial_cmp(y).unwrap());
        b.sort_by(|x, y| x.partial_cmp(y).unwrap());
        assert_eq!(a, b);
        assert_eq!(back.bounds(), m.bounds());
    }

    #[test]
    fn polygons_negative_indices_texture_references_and_leftovers() {
        let text = "# comment\nmtllib a.mtl\nv 0 0 0\nv 1 0 0\nv 1 1 0\nv 0 1 0\nvt 0 0\nvn 0 0 1\nusemtl red\nf 1/1/1 2/1/1 3/1/1 4/1/1\nf -4 -3 -2\nf 1 2 9\ng group\n";
        let r = read(text).unwrap();
        assert_eq!(r.mesh.triangles.len(), 3, "a quad is two triangles, plus one with negative indices");
        assert!(r.notes.iter().any(|n| n.contains("Materials")) && r.notes.iter().any(|n| n.contains("missing vertices")), "{:?}", r.notes);
        assert!(read("f 1 2 3").is_err());
        assert!(read("v 1 two 3").is_err());
        // Points only.
        let p = read("v 1 2 3\nv 4 5 6\n").unwrap().mesh;
        assert_eq!((p.positions.len(), p.triangles.len()), (2, 0));
    }
}
