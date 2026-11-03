//! STL: binary or ASCII triangle lists (every triangle carries its own corners and normal).

use crate::mesh::*;
use dotty_core::{Error, Result};

fn bad(why: impl std::fmt::Display) -> Error {
    Error::Codec(format!("not a valid STL file: {why}"))
}

fn f32_at(b: &[u8], at: usize) -> f32 {
    f32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// A normal from the file when it's usable, else worked out from the corners.
fn facet(m: &mut Mesh, stored: V3, v: [V3; 3]) {
    let computed = normalize(cross(sub(v[1], v[0]), sub(v[2], v[0])));
    let n = if stored.iter().all(|c| c.is_finite()) && dot(stored, stored) > 0.25 { normalize(stored) } else { computed };
    let base = m.positions.len() as u32;
    m.positions.extend_from_slice(&v);
    m.normals.extend_from_slice(&[n; 3]);
    m.triangles.push([base, base + 1, base + 2]);
}

pub fn read(b: &[u8]) -> Result<Mesh> {
    let mut m = Mesh::new(Up::Z);
    let binary = b.len() >= 84 && {
        let n = u32::from_le_bytes([b[80], b[81], b[82], b[83]]) as usize;
        // The size must match; a text file that happens to start with "solid" never does.
        n.checked_mul(50).and_then(|x| x.checked_add(84)).is_some_and(|total| total == b.len() || (total < b.len() && !b.starts_with(b"solid")))
    };
    if binary {
        let n = u32::from_le_bytes([b[80], b[81], b[82], b[83]]) as usize;
        m.positions.reserve(n * 3);
        for i in 0..n {
            let at = 84 + i * 50;
            let f = |k: usize| f32_at(b, at + 4 * k);
            facet(&mut m, [f(0), f(1), f(2)], [[f(3), f(4), f(5)], [f(6), f(7), f(8)], [f(9), f(10), f(11)]]);
        }
        return finish(m);
    }
    let text = String::from_utf8_lossy(b);
    let mut words = text.split_ascii_whitespace();
    let (mut normal, mut corners): (V3, Vec<V3>) = ([0.0; 3], Vec::new());
    let num = |w: Option<&str>| -> Result<f32> { w.and_then(|w| w.parse().ok()).ok_or_else(|| bad("a number is missing or unreadable")) };
    while let Some(w) = words.next() {
        match w.to_ascii_lowercase().as_str() {
            "facet" => {
                corners.clear();
                // "facet normal nx ny nz"
                if words.clone().next().is_some_and(|n| n.eq_ignore_ascii_case("normal")) {
                    words.next();
                    normal = [num(words.next())?, num(words.next())?, num(words.next())?];
                }
            }
            "vertex" => corners.push([num(words.next())?, num(words.next())?, num(words.next())?]),
            "endfacet" => {
                if corners.len() == 3 {
                    facet(&mut m, normal, [corners[0], corners[1], corners[2]]);
                }
                corners.clear();
            }
            _ => {}
        }
    }
    finish(m)
}

fn finish(m: Mesh) -> Result<Mesh> {
    if m.triangles.is_empty() { Err(bad("it holds no triangles")) } else { Ok(m) }
}

pub fn write(m: &Mesh, ascii: bool) -> Vec<u8> {
    let corners = |t: [u32; 3]| [m.positions[t[0] as usize], m.positions[t[1] as usize], m.positions[t[2] as usize]];
    let normal = |c: [V3; 3]| normalize(cross(sub(c[1], c[0]), sub(c[2], c[0])));
    if ascii {
        let mut out = String::from("solid dotty\n");
        for &t in &m.triangles {
            let c = corners(t);
            let n = normal(c);
            out.push_str(&format!("  facet normal {} {} {}\n    outer loop\n", n[0], n[1], n[2]));
            for v in c {
                out.push_str(&format!("      vertex {} {} {}\n", v[0], v[1], v[2]));
            }
            out.push_str("    endloop\n  endfacet\n");
        }
        out.push_str("endsolid dotty\n");
        return out.into_bytes();
    }
    let mut out = Vec::with_capacity(84 + m.triangles.len() * 50);
    let mut header = b"Binary STL written by Dotty".to_vec();
    header.resize(80, 0);
    out.extend_from_slice(&header);
    out.extend_from_slice(&(m.triangles.len() as u32).to_le_bytes());
    for &t in &m.triangles {
        let c = corners(t);
        for v in std::iter::once(normal(c)).chain(c) {
            for x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        out.extend_from_slice(&[0, 0]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_and_ascii_round_trip_a_cube() {
        let cube = cube();
        for ascii in [false, true] {
            let bytes = write(&cube, ascii);
            assert_eq!(bytes.starts_with(b"solid"), ascii);
            let back = read(&bytes).unwrap();
            assert_eq!((back.triangles.len(), back.positions.len(), back.up), (12, 36, Up::Z), "triangles are not shared between faces");
            assert!(back.has_normals() && back.normals.iter().all(|n| (dot(*n, *n) - 1.0).abs() < 1e-4));
            assert_eq!(back.bounds(), Some(([0.0; 3], [1.0; 3])));
        }
        assert_eq!(write(&cube, false).len(), 84 + 12 * 50);
    }

    #[test]
    fn a_binary_file_whose_header_starts_with_solid_is_still_binary() {
        let mut bytes = write(&cube(), false);
        bytes[..5].copy_from_slice(b"solid");
        assert_eq!(read(&bytes).unwrap().triangles.len(), 12);
    }

    #[test]
    fn bad_normals_are_worked_out_and_junk_is_refused() {
        let text = "solid x\nfacet normal 0 0 0\nouter loop\nvertex 0 0 0\nvertex 1 0 0\nvertex 0 1 0\nendloop\nendfacet\nendsolid";
        let m = read(text.as_bytes()).unwrap();
        assert_eq!(m.normals[0], [0.0, 0.0, 1.0]);
        assert!(read(b"solid empty\nendsolid").is_err());
        assert!(read(b"not a model at all").is_err());
        assert!(read(b"solid x\nfacet normal 0 0 1\nvertex 1 two 3").is_err());
    }
}
