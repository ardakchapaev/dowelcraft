//! CFRN — the open exchange format of BAZIS-Cloud: a ZIP with `file.json` (objects table + tree with
//! 4×4 placement matrices). Unlike `.b3d` it is documented by Bazis-Center (help.bazissoft.ru, "CFRN file
//! format"), and the BAZIS-Cloud API converts it to `.b3d` (`model-convert`), so a server can produce
//! Bazis models without forging the `.b3d` signature tail.
//!
//! Layout follows the official sample (`TEST_cfrn.cfrn`): panels are `objType` 2 with a contour list
//! (`elemType` 4 of lines), edge banding per contour element, holes in panel axes with a `table.holes`
//! reference, grooves with a rectangular section and a straight trajectory; each top-level object is a block
//! (`objType` 7). Matrices are row-vector style: rows are the panel axes X, Y, Z, then the origin.
//! shortcut: arcs and circles are written as polylines (the official description gives no arc fields and the
//! sample has none); no preview.jpg and no hardware meshes.

use serde_json::{Value, json};

use crate::contour::Seg;
use crate::design::{self, Project};
use crate::geom::Frame;
use crate::panel::Texture;

/// Line pieces per full turn when an arc or a circle is flattened.
const ARC_STEPS: f64 = 32.0;

fn pt(p: [f64; 2]) -> Value {
    json!({ "x": round(p[0]), "y": round(p[1]) })
}

fn pt3(p: [f64; 3]) -> Value {
    json!({ "x": round(p[0]), "y": round(p[1]), "z": round(p[2]) })
}

/// 0.001 mm is far below manufacturing tolerance and keeps the JSON short.
fn round(v: f64) -> f64 {
    let r = (v * 1000.0).round() / 1000.0;
    if r == 0.0 { 0.0 } else { r }
}

fn matrix(fr: &Frame) -> Value {
    let (x, y, z, o) = (fr.x, fr.y, fr.z, fr.origin);
    json!([x[0], x[1], x[2], 0, y[0], y[1], y[2], 0, z[0], z[1], z[2], 0, round(o[0]), round(o[1]), round(o[2]), 1])
}

/// Contour segments as CFRN line elements; every element remembers the segment it came from.
fn lines(segs: &[Seg]) -> Vec<([f64; 2], [f64; 2], usize)> {
    let mut out = Vec::new();
    for (i, sg) in segs.iter().enumerate() {
        match *sg {
            Seg::Line { a, b } => out.push((a, b, i)),
            Seg::Arc { a, b, c, ccw } => {
                let r = ((a[0] - c[0]).powi(2) + (a[1] - c[1]).powi(2)).sqrt();
                let (s, e) = ((a[1] - c[1]).atan2(a[0] - c[0]), (b[1] - c[1]).atan2(b[0] - c[0]));
                let tau = std::f64::consts::TAU;
                let mut sweep = if ccw { e - s } else { s - e };
                while sweep <= 0.0 {
                    sweep += tau;
                }
                let n = ((sweep / tau) * ARC_STEPS).ceil().max(1.0) as usize;
                let dir = if ccw { 1.0 } else { -1.0 };
                let mut prev = a;
                for k in 1..=n {
                    let p = if k == n { b } else {
                        let t = s + dir * sweep * k as f64 / n as f64;
                        [c[0] + r * t.cos(), c[1] + r * t.sin()]
                    };
                    out.push((prev, p, i));
                    prev = p;
                }
            }
            Seg::Circle { c, r } => {
                let n = ARC_STEPS as usize;
                let at = |k: usize| {
                    let t = std::f64::consts::TAU * k as f64 / n as f64;
                    [c[0] + r * t.cos(), c[1] + r * t.sin()]
                };
                for k in 0..n {
                    out.push((at(k), at(k + 1), i));
                }
            }
        }
    }
    out
}

/// Index of a material (`Name\rArticle`) in the materials table, added on first use.
fn material_index(table: &mut Vec<Value>, full: &str) -> usize {
    let (name, art) = full.split_once('\r').unwrap_or((full, ""));
    if let Some(i) = table.iter().position(|m| m["name"] == name && m["art"] == art) {
        return i;
    }
    table.push(json!({ "name": name, "art": art }));
    table.len() - 1
}

fn hole_index(table: &mut Vec<Value>, depth: f64, diameter: f64) -> usize {
    let v = json!({ "depth": round(depth), "diameter": round(diameter) });
    if let Some(i) = table.iter().position(|h| *h == v) {
        return i;
    }
    table.push(v);
    table.len() - 1
}

/// `file.json` of a project.
pub fn file_json(pr: &Project) -> Result<Value, String> {
    let parts = design::build(pr)?;
    let (mut materials, mut holes, mut objects) = (Vec::new(), Vec::new(), Vec::new());
    let mut owners: Vec<u32> = Vec::new();
    for p in &parts {
        if !owners.contains(&p.owner) {
            owners.push(p.owner);
        }
    }
    let mut blocks = Vec::new();
    for &owner in &owners {
        objects.push(json!({ "objType": 7, "name": crate::export::owner_name(pr, owner), "isAssemblyUnit": false }));
        let block_index = objects.len() - 1;
        let mut children = Vec::new();
        for p in parts.iter().filter(|p| p.owner == owner) {
            let pl = &p.panel;
            let elements = lines(&pl.segs);
            let mut butts = Vec::new();
            for e in &pl.edges {
                let m = material_index(&mut materials, &e.material);
                for (k, _) in elements.iter().enumerate().filter(|(_, l)| l.2 == e.segment) {
                    butts.push(json!({
                        "elemIndex": k, "thickness": round(e.thickness), "width": if e.width > 0.0 { round(e.width) } else { -1.0 },
                        "buttType": 0, "overhang": 0, "allowance": round(e.allowance), "cutIndex": -1, "clip": e.clip, "materialIndex": m,
                    }));
                }
            }
            let panel_holes: Vec<Value> = pl.holes.iter().map(|h| json!({ "pos": pt3(h.pos), "dir": pt3(h.dir), "infoIndex": hole_index(&mut holes, h.depth, h.diameter) })).collect();
            // Section: across the path × through the thickness, measured from the back face.
            let grooves: Vec<Value> = pl
                .grooves
                .iter()
                .map(|g| json!({
                    "name": g.name, "sign": format!("{}x{}", round(g.across[1] - g.across[0]), round(g.z[1] - g.z[0])), "frontSide": false,
                    "contour": { "size": pt([g.across[1] - g.across[0], g.z[1] - g.z[0]]), "pos": pt([g.across[0], g.z[0]]) },
                    "trajectory": { "elemType": 4, "list": [{ "elemType": 1, "pos1": pt(g.path[0]), "pos2": pt(g.path[1]) }] },
                }))
                .collect();
            objects.push(json!({
                "objType": 2,
                "name": pl.name,
                "position": pl.pos,
                "designation": pl.designation,
                "materialIndex": material_index(&mut materials, &pl.material),
                "materialWidth": 0,
                "thickness": round(pl.thickness),
                "textureOrientation": match pl.texture { Texture::Horizontal => 1, Texture::Vertical => 2, Texture::Default => 0 },
                "frontFace": 2,
                "contour": { "elemType": 4, "list": elements.iter().map(|(a, b, _)| json!({ "elemType": 1, "pos1": pt(*a), "pos2": pt(*b) })).collect::<Vec<_>>() },
                "butts": butts,
                "holes": panel_holes,
                "grooves": grooves,
            }));
            children.push(json!({ "tableIndex": objects.len() - 1, "matrix": matrix(&pl.frame.unwrap_or(Frame::IDENTITY)) }));
        }
        blocks.push(json!({ "tableIndex": block_index, "matrix": matrix(&Frame::IDENTITY), "objs": children }));
    }
    Ok(json!({
        "model": { "tableIndex": -1, "objs": blocks },
        "table": { "materials": materials, "objects": objects, "holes": holes, "triangles": [] },
        "modelParams": { "name": pr.name, "art": "", "orderName": "" },
    }))
}

/// `.cfrn` file (ZIP) of a project.
pub fn cfrn(pr: &Project) -> Result<Vec<u8>, String> {
    let json = serde_json::to_vec_pretty(&file_json(pr)?).map_err(|e| e.to_string())?;
    Ok(zip(&[("file.json", &json)]))
}

/// Minimal ZIP writer (deflate, UTF-8 names): enough for a few files of a CFRN.
fn zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write;
    let crc = |data: &[u8]| {
        let mut c = flate2::Crc::new();
        c.update(data);
        c.sum()
    };
    let (mut out, mut central) = (Vec::new(), Vec::new());
    for (name, data) in files {
        let mut z = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(data).expect("writing to memory");
        let packed = z.finish().expect("writing to memory");
        let offset = out.len() as u32;
        // version 2.0, flag bit 11 (UTF-8 names), deflate, DOS time 0 (1980-01-01).
        let fields = |v: &mut Vec<u8>| {
            v.extend_from_slice(&[20, 0, 0, 8, 8, 0, 0, 0, 33, 0]);
            v.extend_from_slice(&crc(data).to_le_bytes());
            v.extend_from_slice(&(packed.len() as u32).to_le_bytes());
            v.extend_from_slice(&(data.len() as u32).to_le_bytes());
            v.extend_from_slice(&(name.len() as u16).to_le_bytes());
            v.extend_from_slice(&0u16.to_le_bytes());
        };
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        fields(&mut out);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&packed);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&[20, 0]);
        fields(&mut central);
        central.extend_from_slice(&[0; 6]); // comment length, disk number, internal attributes
        central.extend_from_slice(&0u32.to_le_bytes()); // external attributes
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let start = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::design::Session;

    fn cabinet() -> Project {
        let mut s = Session::new(Project::new("test"));
        s.execute("material.add", &json!({"name": "White 16", "thickness": 16, "article": "W1"})).unwrap();
        s.execute("edge.add", &json!({"name": "Edge 2", "thickness": 2})).unwrap();
        s.execute("cabinet.add", &json!({"width": 800, "height": 720, "depth": 560, "material": "White 16", "edge": "Edge 2"})).unwrap();
        s.project
    }

    #[test]
    fn every_panel_is_in_the_tree_once() {
        let pr = cabinet();
        let j = file_json(&pr).unwrap();
        let panels = design::build(&pr).unwrap().len();
        let in_tree: usize = j["model"]["objs"].as_array().unwrap().iter().map(|b| b["objs"].as_array().unwrap().len()).sum();
        assert_eq!(in_tree, panels);
        let objects = j["table"]["objects"].as_array().unwrap();
        assert_eq!(objects.iter().filter(|o| o["objType"] == 2).count(), panels);
        assert!(j["table"]["materials"].as_array().unwrap().iter().any(|m| m["name"] == "White 16" && m["art"] == "W1"));
    }

    #[test]
    fn butts_point_at_existing_elements() {
        let j = file_json(&cabinet()).unwrap();
        for o in j["table"]["objects"].as_array().unwrap().iter().filter(|o| o["objType"] == 2) {
            let n = o["contour"]["list"].as_array().unwrap().len() as u64;
            for b in o["butts"].as_array().unwrap() {
                assert!(b["elemIndex"].as_u64().unwrap() < n);
            }
        }
    }

    #[test]
    fn arc_becomes_a_closed_polyline() {
        let l = lines(&[Seg::Arc { a: [10.0, 0.0], b: [0.0, 10.0], c: [0.0, 0.0], ccw: true }]);
        assert_eq!(l.len(), 8);
        assert_eq!(l[0].0, [10.0, 0.0]);
        assert_eq!(l.last().unwrap().1, [0.0, 10.0]);
        assert!(l.windows(2).all(|w| w[0].1 == w[1].0));
    }

    #[test]
    fn zip_holds_file_json() {
        let data = cfrn(&cabinet()).unwrap();
        assert_eq!(&data[..4], b"PK\x03\x04");
        let name_len = u16::from_le_bytes([data[26], data[27]]) as usize;
        assert_eq!(&data[30..30 + name_len], b"file.json");
        let packed = u32::from_le_bytes(data[18..22].try_into().unwrap()) as usize;
        let mut json = String::new();
        use std::io::Read;
        flate2::read::DeflateDecoder::new(&data[30 + name_len..30 + name_len + packed]).read_to_string(&mut json).unwrap();
        assert!(serde_json::from_str::<Value>(&json).unwrap()["table"]["objects"].as_array().unwrap().len() > 1);
    }

    #[test]
    fn central_directory_is_consistent() {
        let data = zip(&[("a.json", b"{}"), ("b.txt", b"hello")]);
        let eocd = data.len() - 22;
        assert_eq!(&data[eocd..eocd + 4], b"PK\x05\x06");
        let u16_at = |p: usize| u16::from_le_bytes([data[p], data[p + 1]]) as usize;
        let u32_at = |p: usize| u32::from_le_bytes(data[p..p + 4].try_into().unwrap()) as usize;
        assert_eq!(u16_at(eocd + 10), 2);
        let (size, mut p) = (u32_at(eocd + 12), u32_at(eocd + 16));
        assert_eq!(p + size, eocd);
        for want in ["a.json", "b.txt"] {
            assert_eq!(&data[p..p + 4], b"PK\x01\x02");
            let (n, extra, comment) = (u16_at(p + 28), u16_at(p + 30), u16_at(p + 32));
            let local = u32_at(p + 42);
            assert_eq!(&data[p + 46..p + 46 + n], want.as_bytes());
            assert_eq!(&data[local..local + 4], b"PK\x03\x04");
            p += 46 + n + extra + comment;
        }
    }
}
