//! A triangle mesh: what every 3D format is read into and written from.

/// Which axis points up. STL and PLY (3D printing, scanning) use Z; OBJ and glTF use Y.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Up {
    #[default]
    Y,
    Z,
}

#[derive(Clone, Debug, Default)]
pub struct Mesh {
    pub positions: Vec<[f32; 3]>,
    /// Per vertex, or empty.
    pub normals: Vec<[f32; 3]>,
    /// Per vertex RGBA, or empty.
    pub colors: Vec<[u8; 4]>,
    pub triangles: Vec<[u32; 3]>,
    pub up: Up,
}

pub type V3 = [f32; 3];

pub fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

pub fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub fn normalize(a: V3) -> V3 {
    let l = dot(a, a).sqrt();
    if l > 1e-20 && l.is_finite() { [a[0] / l, a[1] / l, a[2] / l] } else { [0.0, 0.0, 0.0] }
}

impl Mesh {
    pub fn new(up: Up) -> Self {
        Self { up, ..Default::default() }
    }

    pub fn has_normals(&self) -> bool {
        !self.normals.is_empty() && self.normals.len() == self.positions.len()
    }

    pub fn has_colors(&self) -> bool {
        !self.colors.is_empty() && self.colors.len() == self.positions.len()
    }

    /// Smallest and largest coordinates, if there are any vertices.
    pub fn bounds(&self) -> Option<(V3, V3)> {
        let first = *self.positions.first()?;
        Some(self.positions.iter().fold((first, first), |(lo, hi), p| ([lo[0].min(p[0]), lo[1].min(p[1]), lo[2].min(p[2])], [hi[0].max(p[0]), hi[1].max(p[1]), hi[2].max(p[2])])))
    }

    /// Unnormalised normal of a triangle (its length is twice the area).
    pub fn face_vector(&self, t: [u32; 3]) -> V3 {
        let p = |i: u32| self.positions[i as usize];
        cross(sub(p(t[1]), p(t[0])), sub(p(t[2]), p(t[0])))
    }

    /// Normals averaged over the faces around each vertex, weighted by face size.
    pub fn smooth_normals(&self) -> Vec<V3> {
        let mut n = vec![[0.0f32; 3]; self.positions.len()];
        for &t in &self.triangles {
            let f = self.face_vector(t);
            for &i in &t {
                let v = &mut n[i as usize];
                v[0] += f[0];
                v[1] += f[1];
                v[2] += f[2];
            }
        }
        n.into_iter().map(normalize).collect()
    }

    /// Drop triangles that point outside the vertex list, repeat a vertex, or have non-finite
    /// corners; returns how many went.
    pub fn clean(&mut self) -> usize {
        let before = self.triangles.len();
        let n = self.positions.len() as u32;
        let finite: Vec<bool> = self.positions.iter().map(|p| p.iter().all(|c| c.is_finite())).collect();
        self.triangles.retain(|t| t.iter().all(|&i| i < n && finite[i as usize]) && t[0] != t[1] && t[1] != t[2] && t[0] != t[2]);
        before - self.triangles.len()
    }

    /// Add another mesh's geometry to this one.
    pub fn append(&mut self, other: &Mesh) {
        let base = self.positions.len() as u32;
        let (had_normals, had_colors, was_empty) = (self.has_normals(), self.has_colors(), self.positions.is_empty());
        self.positions.extend_from_slice(&other.positions);
        self.triangles.extend(other.triangles.iter().map(|t| [t[0] + base, t[1] + base, t[2] + base]));
        // Normals and colours only survive when both have them (or this one was empty).
        if was_empty || had_normals {
            if other.has_normals() {
                self.normals.extend_from_slice(&other.normals);
            } else if !was_empty {
                self.normals.clear();
            }
        }
        if was_empty || had_colors {
            if other.has_colors() {
                self.colors.extend_from_slice(&other.colors);
            } else if !was_empty {
                self.colors.clear();
            }
        }
        if !was_empty && !had_normals {
            self.normals.clear();
        }
        if !was_empty && !had_colors {
            self.colors.clear();
        }
    }

    /// Turn the model so `up` points up (a quarter turn about X), normals with it.
    pub fn set_up(&mut self, up: Up) {
        if self.up == up {
            return;
        }
        let turn = |p: V3| if up == Up::Y { [p[0], p[2], -p[1]] } else { [p[0], -p[2], p[1]] };
        self.positions.iter_mut().for_each(|p| *p = turn(*p));
        self.normals.iter_mut().for_each(|n| *n = turn(*n));
        self.up = up;
    }

    pub fn size(&self) -> V3 {
        self.bounds().map_or([0.0; 3], |(lo, hi)| sub(hi, lo))
    }
}

/// A unit cube, 12 triangles, with Z up, for tests.
#[cfg(test)]
pub fn cube() -> Mesh {
    let mut m = Mesh::new(Up::Z);
    for z in [0.0, 1.0] {
        for y in [0.0, 1.0] {
            for x in [0.0, 1.0] {
                m.positions.push([x, y, z]);
            }
        }
    }
    // Vertex index = x + 2y + 4z. Outward-facing triangles.
    m.triangles = vec![[0, 2, 1], [1, 2, 3], [4, 5, 6], [5, 7, 6], [0, 1, 4], [1, 5, 4], [2, 6, 3], [3, 6, 7], [0, 4, 2], [2, 4, 6], [1, 3, 5], [3, 7, 5]];
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_normals_and_cleaning() {
        let mut m = cube();
        assert_eq!(m.bounds(), Some(([0.0; 3], [1.0; 3])));
        let n = m.smooth_normals();
        assert!(n.iter().all(|v| (dot(*v, *v) - 1.0).abs() < 1e-5));
        // Every face's normal points away from the middle of the cube.
        for &t in &m.triangles {
            let f = m.face_vector(t);
            let centre = [(m.positions[t[0] as usize][0] + m.positions[t[1] as usize][0] + m.positions[t[2] as usize][0]) / 3.0 - 0.5, (m.positions[t[0] as usize][1] + m.positions[t[1] as usize][1] + m.positions[t[2] as usize][1]) / 3.0 - 0.5, (m.positions[t[0] as usize][2] + m.positions[t[1] as usize][2] + m.positions[t[2] as usize][2]) / 3.0 - 0.5];
            assert!(dot(f, centre) > 0.0, "{t:?} faces inwards");
        }
        m.triangles.push([0, 0, 1]);
        m.triangles.push([0, 1, 99]);
        m.positions[7][0] = f32::NAN;
        let dropped = m.clean();
        assert_eq!(dropped, 2 + 3, "a repeated vertex, a missing one, and the 3 triangles using the NaN corner");
    }

    #[test]
    fn up_axis_turns_the_model_and_back() {
        let mut m = Mesh::new(Up::Z);
        m.positions = vec![[1.0, 2.0, 3.0]];
        m.normals = vec![[0.0, 0.0, 1.0]];
        m.set_up(Up::Y);
        assert_eq!((m.positions[0], m.normals[0]), ([1.0, 3.0, -2.0], [0.0, 1.0, 0.0]), "Z-up's top is Y-up's top");
        m.set_up(Up::Z);
        assert_eq!(m.positions[0], [1.0, 2.0, 3.0]);
    }

    #[test]
    fn meshes_are_joined_keeping_only_what_both_have() {
        let mut a = cube();
        a.colors = vec![[1, 2, 3, 255]; 8];
        let b = cube();
        a.append(&b);
        assert_eq!((a.positions.len(), a.triangles.len(), a.triangles[12][0]), (16, 24, 8));
        assert!(a.colors.is_empty(), "colours dropped when one part has none");
    }
}
