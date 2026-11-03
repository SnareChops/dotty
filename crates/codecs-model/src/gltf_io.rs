//! glTF 2.0 and GLB: read through the `gltf` crate (scene graph, any accessor type, embedded
//! or neighbouring buffers); written as one mesh, as a GLB or a single self-contained `.gltf`.
//! Materials and textures aren't kept (a plain colour per vertex is).

use crate::base64;
use crate::mesh::*;
use dotty_core::{Error, Result};
use serde_json::json;
use std::path::Path;

pub struct Read {
    pub mesh: Mesh,
    pub notes: Vec<String>,
}

fn bad(why: impl std::fmt::Display) -> Error {
    Error::Codec(format!("not a valid glTF file: {why}"))
}

type M4 = [[f32; 4]; 4];

/// Column-major product `a * b`.
fn mul(a: &M4, b: &M4) -> M4 {
    let mut r = [[0.0; 4]; 4];
    for (c, col) in r.iter_mut().enumerate() {
        for (row, v) in col.iter_mut().enumerate() {
            *v = (0..4).map(|k| a[k][row] * b[c][k]).sum();
        }
    }
    r
}

fn point(m: &M4, p: V3) -> V3 {
    [0, 1, 2].map(|r| m[0][r] * p[0] + m[1][r] * p[1] + m[2][r] * p[2] + m[3][r])
}

fn direction(m: &M4, n: V3) -> V3 {
    normalize([0, 1, 2].map(|r| m[0][r] * n[0] + m[1][r] * n[1] + m[2][r] * n[2]))
}

fn buffer_data(g: &gltf::Gltf, dir: Option<&Path>) -> Result<Vec<Vec<u8>>> {
    g.buffers()
        .map(|b| match b.source() {
            gltf::buffer::Source::Bin => g.blob.clone().ok_or_else(|| bad("the binary part is missing")),
            gltf::buffer::Source::Uri(uri) => {
                if let Some(rest) = uri.strip_prefix("data:") {
                    let data = rest.split_once(',').map(|(_, d)| d).ok_or_else(|| bad("a data URI without data"))?;
                    base64::decode(data).ok_or_else(|| bad("a buffer isn't valid base64"))
                } else {
                    let dir = dir.ok_or_else(|| bad("it refers to other files, which can't be found from here"))?;
                    let name = uri.replace("%20", " ");
                    std::fs::read(dir.join(&name)).map_err(|_| Error::Codec(format!("the glTF's data file \"{name}\" is missing: keep it next to the .gltf")))
                }
            }
        })
        .collect()
}

pub fn read(bytes: &[u8], dir: Option<&Path>) -> Result<Read> {
    let g = gltf::Gltf::from_slice(bytes).map_err(bad)?;
    let buffers = buffer_data(&g, dir)?;
    let scene = g.default_scene().or_else(|| g.scenes().next()).ok_or_else(|| bad("it has no scene"))?;
    let (mut parts, mut dropped_modes, mut textured, mut points): (Vec<Mesh>, usize, bool, Mesh) = (Vec::new(), 0, false, Mesh::new(Up::Y));
    let identity: M4 = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
    let mut stack: Vec<(gltf::Node, M4)> = scene.nodes().map(|n| (n, identity)).collect();
    while let Some((node, parent)) = stack.pop() {
        let m = mul(&parent, &node.transform().matrix());
        for child in node.children() {
            stack.push((child, m));
        }
        let Some(mesh) = node.mesh() else { continue };
        for prim in mesh.primitives() {
            let reader = prim.reader(|b| buffers.get(b.index()).map(|v| v.as_slice()));
            let Some(pos) = reader.read_positions() else { continue };
            let mut part = Mesh::new(Up::Y);
            part.positions = pos.map(|p| point(&m, p)).collect();
            if let Some(n) = reader.read_normals() {
                part.normals = n.map(|n| direction(&m, n)).collect();
            }
            let material = prim.material().pbr_metallic_roughness();
            textured |= material.base_color_texture().is_some();
            let factor = material.base_color_factor();
            let vertex_colors = reader.read_colors(0).map(|c| c.into_rgba_u8().collect::<Vec<_>>());
            if vertex_colors.is_some() || factor.iter().take(3).any(|f| (f - 1.0).abs() > 0.01) {
                let tint = |c: u8, f: f32| (c as f32 * f).round().clamp(0.0, 255.0) as u8;
                part.colors = match vertex_colors {
                    Some(v) => v.into_iter().map(|c| [tint(c[0], factor[0]), tint(c[1], factor[1]), tint(c[2], factor[2]), tint(c[3], factor[3])]).collect(),
                    None => vec![[tint(255, factor[0]), tint(255, factor[1]), tint(255, factor[2]), tint(255, factor[3])]; part.positions.len()],
                };
            }
            let n = part.positions.len() as u32;
            let index: Vec<u32> = reader.read_indices().map(|i| i.into_u32().collect()).unwrap_or_else(|| (0..n).collect());
            match prim.mode() {
                gltf::mesh::Mode::Triangles => part.triangles = index.as_chunks::<3>().0.to_vec(),
                gltf::mesh::Mode::TriangleStrip => part.triangles = (2..index.len()).map(|i| if i % 2 == 0 { [index[i - 2], index[i - 1], index[i]] } else { [index[i - 1], index[i - 2], index[i]] }).collect(),
                gltf::mesh::Mode::TriangleFan => part.triangles = (2..index.len()).map(|i| [index[0], index[i - 1], index[i]]).collect(),
                gltf::mesh::Mode::Points => {
                    points.append(&part);
                    continue;
                }
                _ => {
                    dropped_modes += 1;
                    continue;
                }
            }
            parts.push(part);
        }
    }
    let mut out = Mesh::new(Up::Y);
    // A colour on some parts means every part gets one, so none is lost when they're joined.
    if parts.iter().any(Mesh::has_colors) {
        for p in parts.iter_mut().filter(|p| !p.has_colors()) {
            p.colors = vec![[255; 4]; p.positions.len()];
        }
    }
    for p in &parts {
        out.append(p);
    }
    if out.triangles.is_empty() && !points.positions.is_empty() {
        out = points;
    }
    let dropped = out.clean();
    if out.positions.is_empty() {
        return Err(Error::Codec("this glTF holds no triangle meshes (Draco or other compressed geometry isn't supported)".into()));
    }
    let mut notes = Vec::new();
    if textured || g.materials().len() > 0 {
        notes.push("Materials and textures in the glTF weren't kept (plain colours were)".to_string());
    }
    if dropped_modes > 0 {
        notes.push(format!("{dropped_modes} line or point part(s) were left out"));
    }
    if dropped > 0 {
        notes.push(format!("{dropped} empty or broken triangle(s) were left out"));
    }
    if g.nodes().any(|n| n.skin().is_some()) || g.animations().len() > 0 {
        notes.push("Animation and skeleton data weren't kept (the model is in its rest pose)".to_string());
    }
    Ok(Read { mesh: out, notes })
}

/// The mesh as GLB bytes (`glb`) or a self-contained glTF JSON file.
pub fn write(m: &Mesh, glb: bool) -> Vec<u8> {
    let mut bin: Vec<u8> = Vec::new();
    let mut views = Vec::new();
    let mut accessors = Vec::new();
    let mut attributes = serde_json::Map::new();
    let pad = |bin: &mut Vec<u8>| bin.resize(bin.len().div_ceil(4) * 4, 0);
    let (lo, hi) = m.bounds().unwrap_or(([0.0; 3], [0.0; 3]));
    let mut add = |bin: &mut Vec<u8>, bytes: &[u8], target: u32| -> usize {
        pad(bin);
        let view = views.len();
        views.push(json!({"buffer": 0, "byteOffset": bin.len(), "byteLength": bytes.len(), "target": target}));
        bin.extend_from_slice(bytes);
        view
    };
    let flat = |v: &[V3]| v.iter().flat_map(|p| p.iter().flat_map(|x| x.to_le_bytes())).collect::<Vec<u8>>();
    let v = add(&mut bin, &flat(&m.positions), 34962);
    attributes.insert("POSITION".into(), json!(accessors.len()));
    accessors.push(json!({"bufferView": v, "componentType": 5126, "count": m.positions.len(), "type": "VEC3", "min": lo, "max": hi}));
    if m.has_normals() {
        let v = add(&mut bin, &flat(&m.normals), 34962);
        attributes.insert("NORMAL".into(), json!(accessors.len()));
        accessors.push(json!({"bufferView": v, "componentType": 5126, "count": m.normals.len(), "type": "VEC3"}));
    }
    if m.has_colors() {
        let v = add(&mut bin, &m.colors.iter().flatten().copied().collect::<Vec<u8>>(), 34962);
        attributes.insert("COLOR_0".into(), json!(accessors.len()));
        accessors.push(json!({"bufferView": v, "componentType": 5121, "normalized": true, "count": m.colors.len(), "type": "VEC4"}));
    }
    let mut primitive = json!({"attributes": attributes, "material": 0});
    if m.triangles.is_empty() {
        primitive["mode"] = json!(0);
    } else {
        let wide = m.positions.len() > 65_535;
        let bytes: Vec<u8> = if wide { m.triangles.iter().flatten().flat_map(|i| i.to_le_bytes()).collect() } else { m.triangles.iter().flatten().flat_map(|&i| (i as u16).to_le_bytes()).collect() };
        let v = add(&mut bin, &bytes, 34963);
        primitive["indices"] = json!(accessors.len());
        accessors.push(json!({"bufferView": v, "componentType": if wide { 5125 } else { 5123 }, "count": m.triangles.len() * 3, "type": "SCALAR"}));
    }
    pad(&mut bin);
    let mut buffer = json!({"byteLength": bin.len()});
    if !glb {
        buffer["uri"] = json!(format!("data:application/octet-stream;base64,{}", base64::encode(&bin)));
    }
    let doc = json!({
        "asset": {"version": "2.0", "generator": "Dotty"},
        "scene": 0,
        "scenes": [{"nodes": [0]}],
        "nodes": [{"mesh": 0}],
        "meshes": [{"primitives": [primitive]}],
        "materials": [{"pbrMetallicRoughness": {"baseColorFactor": if m.has_colors() { [1.0, 1.0, 1.0, 1.0] } else { [0.8, 0.8, 0.8, 1.0] }, "metallicFactor": 0.0, "roughnessFactor": 0.85}, "doubleSided": true}],
        "accessors": accessors,
        "bufferViews": views,
        "buffers": [buffer],
    });
    let mut text = serde_json::to_vec(&doc).unwrap_or_default();
    if !glb {
        return text;
    }
    while !text.len().is_multiple_of(4) {
        text.push(b' ');
    }
    let total = 12 + 8 + text.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x4E4F534Au32.to_le_bytes());
    out.extend_from_slice(&text);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x004E4942u32.to_le_bytes());
    out.extend_from_slice(&bin);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rich_cube() -> Mesh {
        let mut m = cube();
        m.up = Up::Y;
        m.normals = m.smooth_normals();
        m.colors = (0..8).map(|i| [i * 30, 90, 200, 255]).collect();
        m
    }

    #[test]
    fn glb_and_gltf_round_trip() {
        let m = rich_cube();
        for glb in [true, false] {
            let bytes = write(&m, glb);
            assert_eq!(bytes.starts_with(b"glTF"), glb);
            let back = read(&bytes, None).unwrap();
            assert_eq!((back.mesh.positions.clone(), back.mesh.triangles.clone(), back.mesh.colors.clone(), back.mesh.up), (m.positions.clone(), m.triangles.clone(), m.colors.clone(), Up::Y), "glb: {glb}");
            assert!(back.mesh.has_normals());
            assert!(back.notes.iter().any(|n| n.contains("Materials")), "our own material is noted too: {:?}", back.notes);
        }
        // GLB is a well-formed container: lengths add up.
        let glb = write(&m, true);
        assert_eq!(u32::from_le_bytes(glb[8..12].try_into().unwrap()) as usize, glb.len());
        assert_eq!(glb.len() % 4, 0);
    }

    #[test]
    fn big_meshes_use_wide_indices_and_point_clouds_survive() {
        let mut m = Mesh::new(Up::Y);
        m.positions = (0..70_000).map(|i| [i as f32, 0.0, 0.0]).collect();
        m.triangles = vec![[0, 1, 69_999], [2, 3, 4]];
        let back = read(&write(&m, true), None).unwrap().mesh;
        assert_eq!((back.positions.len(), back.triangles.clone()), (70_000, m.triangles.clone()));
        let mut cloud = Mesh::new(Up::Y);
        cloud.positions = vec![[0.0; 3], [1.0, 2.0, 3.0]];
        let back = read(&write(&cloud, false), None).unwrap().mesh;
        assert_eq!((back.positions, back.triangles.len()), (cloud.positions, 0));
    }

    #[test]
    fn node_transforms_apply_and_material_colours_tint() {
        // One triangle, instanced by a node translated by (10, 0, 0), red material.
        let mut bin = Vec::new();
        for p in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
            for x in p {
                bin.extend_from_slice(&x.to_le_bytes());
            }
        }
        let doc = json!({
            "asset": {"version": "2.0"}, "scene": 0, "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0, "translation": [10.0, 0.0, 0.0]}],
            "meshes": [{"primitives": [{"attributes": {"POSITION": 0}}]}],
            "materials": [], "accessors": [{"bufferView": 0, "componentType": 5126, "count": 3, "type": "VEC3", "min": [0, 0, 0], "max": [1, 1, 0]}],
            "bufferViews": [{"buffer": 0, "byteLength": 36}],
            "buffers": [{"byteLength": 36, "uri": format!("data:application/octet-stream;base64,{}", base64::encode(&bin))}]
        });
        let mut doc = doc;
        doc["materials"] = json!([{"pbrMetallicRoughness": {"baseColorFactor": [1.0, 0.0, 0.0, 1.0]}}]);
        doc["meshes"][0]["primitives"][0]["material"] = json!(0);
        let r = read(doc.to_string().as_bytes(), None).unwrap();
        assert_eq!(r.mesh.positions[1], [11.0, 0.0, 0.0], "the node's translation is applied");
        assert_eq!(r.mesh.triangles, vec![[0, 1, 2]], "no indices: the vertices in order");
        assert_eq!(r.mesh.colors[0], [255, 0, 0, 255], "the material colour becomes the vertex colour");
        assert!(read(b"{\"nope\": 1}", None).is_err());
        assert!(read(b"garbage", None).is_err());
    }
}
