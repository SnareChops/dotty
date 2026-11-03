//! PLY (Stanford): ASCII or binary (either byte order), vertices with optional normals and
//! colours, faces of any size (cut into triangles). Other elements and properties are skipped.

use crate::mesh::*;
use dotty_core::{Error, Result};

fn bad(why: impl std::fmt::Display) -> Error {
    Error::Codec(format!("not a valid PLY file: {why}"))
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Ty {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

impl Ty {
    fn parse(s: &str) -> Option<Ty> {
        Some(match s {
            "char" | "int8" => Ty::I8,
            "uchar" | "uint8" => Ty::U8,
            "short" | "int16" => Ty::I16,
            "ushort" | "uint16" => Ty::U16,
            "int" | "int32" => Ty::I32,
            "uint" | "uint32" => Ty::U32,
            "float" | "float32" => Ty::F32,
            "double" | "float64" => Ty::F64,
            _ => return None,
        })
    }

    fn size(self) -> usize {
        match self {
            Ty::I8 | Ty::U8 => 1,
            Ty::I16 | Ty::U16 => 2,
            Ty::I32 | Ty::U32 | Ty::F32 => 4,
            Ty::F64 => 8,
        }
    }
}

#[derive(Debug)]
struct Prop {
    name: String,
    ty: Ty,
    /// Type of the length, for a list.
    list: Option<Ty>,
}

#[derive(Debug)]
struct Element {
    name: String,
    count: usize,
    props: Vec<Prop>,
}

#[derive(Clone, Copy, PartialEq)]
enum Format {
    Ascii,
    Little,
    Big,
}

struct Reader<'a> {
    format: Format,
    bytes: &'a [u8],
    at: usize,
    words: std::str::SplitAsciiWhitespace<'a>,
}

impl Reader<'_> {
    fn next(&mut self, ty: Ty) -> Result<f64> {
        if self.format == Format::Ascii {
            return self.words.next().and_then(|w| w.parse().ok()).ok_or_else(|| bad("a value is missing or unreadable"));
        }
        let n = ty.size();
        let b = self.bytes.get(self.at..self.at + n).ok_or_else(|| bad("the file ends too soon"))?;
        self.at += n;
        let le = self.format == Format::Little;
        macro_rules! num {
            ($t:ty) => {{
                let a: [u8; std::mem::size_of::<$t>()] = b.try_into().unwrap();
                (if le { <$t>::from_le_bytes(a) } else { <$t>::from_be_bytes(a) }) as f64
            }};
        }
        Ok(match ty {
            Ty::I8 => b[0] as i8 as f64,
            Ty::U8 => b[0] as f64,
            Ty::I16 => num!(i16),
            Ty::U16 => num!(u16),
            Ty::I32 => num!(i32),
            Ty::U32 => num!(u32),
            Ty::F32 => num!(f32),
            Ty::F64 => num!(f64),
        })
    }
}

pub fn read(bytes: &[u8]) -> Result<Mesh> {
    let end = bytes.windows(10).position(|w| w == b"end_header").ok_or_else(|| bad("no header"))?;
    let after = bytes[end..].iter().position(|&b| b == b'\n').ok_or_else(|| bad("the header is cut short"))? + end + 1;
    let header = String::from_utf8_lossy(&bytes[..end]);
    let (mut format, mut elements): (Option<Format>, Vec<Element>) = (None, Vec::new());
    for line in header.lines().skip(1) {
        let w: Vec<&str> = line.split_ascii_whitespace().collect();
        match w.as_slice() {
            ["format", f, ..] => {
                format = Some(match *f {
                    "ascii" => Format::Ascii,
                    "binary_little_endian" => Format::Little,
                    "binary_big_endian" => Format::Big,
                    other => return Err(bad(format!("unknown format {other}"))),
                })
            }
            ["element", name, count] => elements.push(Element { name: name.to_string(), count: count.parse().map_err(|_| bad("an element count isn't a number"))?, props: Vec::new() }),
            ["property", "list", count, item, name] => {
                let (Some(c), Some(t)) = (Ty::parse(count), Ty::parse(item)) else { return Err(bad("an unknown property type")) };
                elements.last_mut().ok_or_else(|| bad("a property before any element"))?.props.push(Prop { name: name.to_string(), ty: t, list: Some(c) });
            }
            ["property", ty, name] => {
                let t = Ty::parse(ty).ok_or_else(|| bad(format!("unknown property type {ty}")))?;
                elements.last_mut().ok_or_else(|| bad("a property before any element"))?.props.push(Prop { name: name.to_string(), ty: t, list: None });
            }
            _ => {}
        }
    }
    let format = format.ok_or_else(|| bad("no format line"))?;
    let body = &bytes[after..];
    let text = if format == Format::Ascii { String::from_utf8_lossy(body).into_owned() } else { String::new() };
    let mut r = Reader { format, bytes: body, at: 0, words: text.split_ascii_whitespace() };

    let mut m = Mesh::new(Up::Z);
    for el in &elements {
        match el.name.as_str() {
            "vertex" => {
                let idx = |n: &str| el.props.iter().position(|p| p.name == n);
                let (ix, iy, iz) = (idx("x").ok_or_else(|| bad("vertices without x"))?, idx("y").ok_or_else(|| bad("vertices without y"))?, idx("z").ok_or_else(|| bad("vertices without z"))?);
                let nrm = [idx("nx"), idx("ny"), idx("nz")];
                let rgb = [idx("red").or(idx("r")), idx("green").or(idx("g")), idx("blue").or(idx("b"))];
                let alpha = idx("alpha").or(idx("a"));
                let (has_n, has_c) = (nrm.iter().all(Option::is_some), rgb.iter().all(Option::is_some));
                let mut vals = vec![0.0f64; el.props.len()];
                m.positions.reserve(el.count.min(1 << 24));
                for _ in 0..el.count {
                    for (k, p) in el.props.iter().enumerate() {
                        match p.list {
                            None => vals[k] = r.next(p.ty)?,
                            Some(ct) => {
                                for _ in 0..r.next(ct)? as usize {
                                    r.next(p.ty)?;
                                }
                            }
                        }
                    }
                    m.positions.push([vals[ix] as f32, vals[iy] as f32, vals[iz] as f32]);
                    if has_n {
                        m.normals.push([vals[nrm[0].unwrap()] as f32, vals[nrm[1].unwrap()] as f32, vals[nrm[2].unwrap()] as f32]);
                    }
                    if has_c {
                        // Bytes normally; floats 0–1 in some files.
                        let k = if el.props[rgb[0].unwrap()].ty == Ty::F32 || el.props[rgb[0].unwrap()].ty == Ty::F64 { 255.0 } else { 1.0 };
                        let c = |i: usize| (vals[i] * k).round().clamp(0.0, 255.0) as u8;
                        m.colors.push([c(rgb[0].unwrap()), c(rgb[1].unwrap()), c(rgb[2].unwrap()), alpha.map_or(255, c)]);
                    }
                }
            }
            "face" => {
                for _ in 0..el.count {
                    for p in &el.props {
                        match p.list {
                            Some(ct) => {
                                let n = r.next(ct)? as usize;
                                let mut corners = Vec::with_capacity(n);
                                for _ in 0..n {
                                    corners.push(r.next(p.ty)? as u32);
                                }
                                if p.name == "vertex_indices" || p.name == "vertex_index" {
                                    for k in 1..corners.len().saturating_sub(1) {
                                        m.triangles.push([corners[0], corners[k], corners[k + 1]]);
                                    }
                                }
                            }
                            None => {
                                r.next(p.ty)?;
                            }
                        }
                    }
                }
            }
            _ => {
                // Something we don't use: read past it.
                for _ in 0..el.count {
                    for p in &el.props {
                        match p.list {
                            Some(ct) => {
                                for _ in 0..r.next(ct)? as usize {
                                    r.next(p.ty)?;
                                }
                            }
                            None => {
                                r.next(p.ty)?;
                            }
                        }
                    }
                }
            }
        }
    }
    if m.positions.is_empty() {
        return Err(bad("it holds no vertices"));
    }
    Ok(m)
}

pub fn write(m: &Mesh, ascii: bool) -> Vec<u8> {
    let mut head = String::from("ply\n");
    head.push_str(if ascii { "format ascii 1.0\n" } else { "format binary_little_endian 1.0\n" });
    head.push_str("comment Written by Dotty\n");
    head.push_str(&format!("element vertex {}\nproperty float x\nproperty float y\nproperty float z\n", m.positions.len()));
    if m.has_normals() {
        head.push_str("property float nx\nproperty float ny\nproperty float nz\n");
    }
    if m.has_colors() {
        head.push_str("property uchar red\nproperty uchar green\nproperty uchar blue\nproperty uchar alpha\n");
    }
    head.push_str(&format!("element face {}\nproperty list uchar uint vertex_indices\nend_header\n", m.triangles.len()));
    let mut out = head.into_bytes();
    for (i, p) in m.positions.iter().enumerate() {
        if ascii {
            let mut line = format!("{} {} {}", p[0], p[1], p[2]);
            if m.has_normals() {
                line.push_str(&format!(" {} {} {}", m.normals[i][0], m.normals[i][1], m.normals[i][2]));
            }
            if m.has_colors() {
                let c = m.colors[i];
                line.push_str(&format!(" {} {} {} {}", c[0], c[1], c[2], c[3]));
            }
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        } else {
            for x in p {
                out.extend_from_slice(&x.to_le_bytes());
            }
            if m.has_normals() {
                for x in m.normals[i] {
                    out.extend_from_slice(&x.to_le_bytes());
                }
            }
            if m.has_colors() {
                out.extend_from_slice(&m.colors[i]);
            }
        }
    }
    for t in &m.triangles {
        if ascii {
            out.extend_from_slice(format!("3 {} {} {}\n", t[0], t[1], t[2]).as_bytes());
        } else {
            out.push(3);
            for i in t {
                out.extend_from_slice(&i.to_le_bytes());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rich_cube() -> Mesh {
        let mut m = cube();
        m.normals = m.smooth_normals();
        m.colors = (0..8).map(|i| [i * 30, 90, 200, 255 - i]).collect();
        m
    }

    #[test]
    fn ascii_and_binary_round_trip_exactly() {
        let m = rich_cube();
        for ascii in [false, true] {
            let bytes = write(&m, ascii);
            assert_eq!(bytes.starts_with(b"ply\nformat ascii"), ascii);
            let back = read(&bytes).unwrap();
            assert_eq!((back.positions.clone(), back.triangles.clone(), back.colors.clone(), back.up), (m.positions.clone(), m.triangles.clone(), m.colors.clone(), Up::Z), "ascii: {ascii}");
            assert!(back.normals.iter().zip(&m.normals).all(|(a, b)| (0..3).all(|k| (a[k] - b[k]).abs() < 1e-5)));
        }
    }

    #[test]
    fn big_endian_extra_elements_quads_and_float_colours() {
        // Hand-built big-endian file: an element we don't use, float colours, one quad.
        let mut f = b"ply\nformat binary_big_endian 1.0\nelement vertex 4\nproperty double x\nproperty double y\nproperty double z\nproperty float red\nproperty float green\nproperty float blue\nelement edge 1\nproperty int a\nproperty int b\nelement face 1\nproperty list uchar int vertex_indices\nend_header\n".to_vec();
        for (x, y) in [(0.0f64, 0.0f64), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
            for v in [x, y, 0.0] {
                f.extend_from_slice(&v.to_be_bytes());
            }
            for c in [1.0f32, 0.5, 0.0] {
                f.extend_from_slice(&c.to_be_bytes());
            }
        }
        f.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 2]);
        f.push(4);
        for i in [0i32, 1, 2, 3] {
            f.extend_from_slice(&i.to_be_bytes());
        }
        let m = read(&f).unwrap();
        assert_eq!((m.positions.len(), m.triangles.len(), m.colors[0]), (4, 2, [255, 128, 0, 255]));
        assert_eq!(m.positions[2], [1.0, 1.0, 0.0]);
    }

    #[test]
    fn point_clouds_and_broken_files() {
        let cloud = read(b"ply\nformat ascii 1.0\nelement vertex 2\nproperty float x\nproperty float y\nproperty float z\nend_header\n1 2 3\n4 5 6\n").unwrap();
        assert_eq!((cloud.positions.len(), cloud.triangles.len()), (2, 0));
        assert!(read(b"ply\nformat ascii 1.0\nelement vertex 2\nproperty float x\nproperty float y\nproperty float z\nend_header\n1 2 3\n").is_err(), "cut short");
        assert!(read(b"ply\nformat ascii 1.0\nelement vertex 1\nproperty float x\nend_header\n1\n").is_err(), "no y or z");
        assert!(read(b"not a ply").is_err());
    }
}
