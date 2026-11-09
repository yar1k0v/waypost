//! Admin, place and postcode areas, read from a `.areas.geojsonseq` that
//! osmium export wrote. Polygons keep every vertex and every hole.
//!
//! For each area the builder lists, per admin grid cell, the edges that touch
//! the cell and whether the cell centre is inside. Cells with no edges that
//! lie inside the area get an entry with no edges. See `waypost_core::pip`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use waypost_core::format::{BuildSettings, IndexWriter};
use waypost_core::geo::{LocalFrame, Point};
use waypost_core::model::{
    Area, AreaCell, AreaKind, GridCell, GridSpec, NONE, PlaceKind, StrRef, sec,
};
use waypost_core::pip::{rings_contain, segment_touches_box};

use crate::Tables;
use crate::tags::name_of;

pub struct Areas {
    grid: GridSpec,
    areas: Vec<Area>,
    points: Vec<Point>,
    /// (cell key, cell entry, edges of the entry).
    entries: Vec<(u64, AreaCell, Vec<u64>)>,
    skipped: u64,
}

impl Areas {
    pub fn new(s: &BuildSettings) -> Areas {
        Areas {
            grid: GridSpec { size: s.admin_cell },
            areas: Vec::new(),
            points: Vec::new(),
            entries: Vec::new(),
            skipped: 0,
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{} areas, {} vertices, {} cell entries; skipped {}",
            self.areas.len(),
            self.points.len(),
            self.entries.len(),
            self.skipped
        )
    }

    pub fn read(&mut self, path: &Path, t: &mut Tables) -> Result<()> {
        let file = std::fs::File::open(path)?;
        for (lineno, line) in BufReader::with_capacity(1 << 20, file).lines().enumerate() {
            let line = line?;
            let line = line.trim_start_matches('\u{1e}').trim();
            if line.is_empty() {
                continue;
            }
            let f: Value = serde_json::from_str(line)
                .with_context(|| format!("{}:{}", path.display(), lineno + 1))?;
            if !self.feature(&f, t) {
                self.skipped += 1;
            }
        }
        Ok(())
    }

    /// Adds one GeoJSON feature. Returns false when it is not an area we keep.
    fn feature(&mut self, f: &Value, t: &mut Tables) -> bool {
        let props = &f["properties"];
        let get = |k: &str| -> Option<String> {
            props
                .get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let (kind, level) = if get("boundary").as_deref() == Some("administrative") {
            match get("admin_level").and_then(|l| l.parse::<u8>().ok()) {
                Some(l @ 2..=11) => (AreaKind::Admin, l),
                _ => return false,
            }
        } else if get("boundary").as_deref() == Some("postal_code") {
            (AreaKind::Postcode, 0)
        } else if let Some(pk) = get("place").as_deref().and_then(PlaceKind::from_tag) {
            (AreaKind::Place, pk as u8)
        } else {
            return false;
        };

        let postcode = get("postal_code")
            .or_else(|| (kind == AreaKind::Postcode).then(|| get("name")).flatten());
        let name = match kind {
            AreaKind::Postcode => NONE,
            _ => name_of(t, get, None),
        };
        if kind != AreaKind::Postcode && name == NONE {
            return false;
        }
        if kind == AreaKind::Postcode && postcode.is_none() {
            return false;
        }
        let postcode = postcode.map_or(StrRef::EMPTY, |p| t.strings.add(&p));

        let country = if kind == AreaKind::Admin && level == 2 {
            get("ISO3166-1:alpha2")
                .or_else(|| get("ISO3166-1"))
                .or_else(|| get("country_code"))
                .map(|c| c.to_ascii_uppercase())
                .filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic()))
                .map_or([0; 2], |c| [c.as_bytes()[0], c.as_bytes()[1]])
        } else {
            [0; 2]
        };

        let Some((rings, km2)) = parse_geometry(&f["geometry"]) else {
            return false;
        };
        let osm_id = props.get("@id").and_then(Value::as_u64).unwrap_or(0);
        let area_id = self.areas.len() as u32;
        self.areas.push(Area {
            name,
            kind: kind as u8,
            level,
            country,
            postcode,
            km2,
            osm_id,
        });
        self.index_area(area_id, rings);
        true
    }

    fn index_area(&mut self, area: u32, rings: Vec<Vec<Point>>) {
        let (mut sw, mut ne) = (rings[0][0], rings[0][0]);
        let mut edge_cells: HashMap<(u32, u32), Vec<u64>> = HashMap::new();
        for ring in &rings {
            let base = self.points.len() as u64;
            self.points.extend_from_slice(ring);
            for (i, w) in ring.windows(2).enumerate() {
                let (a, b) = (w[0], w[1]);
                for p in [a, b] {
                    sw.lat = sw.lat.min(p.lat);
                    sw.lon = sw.lon.min(p.lon);
                    ne.lat = ne.lat.max(p.lat);
                    ne.lon = ne.lon.max(p.lon);
                }
                let esw = Point {
                    lat: a.lat.min(b.lat),
                    lon: a.lon.min(b.lon),
                };
                let ene = Point {
                    lat: a.lat.max(b.lat),
                    lon: a.lon.max(b.lon),
                };
                let cells: Vec<(u32, u32)> = self.grid.cells_in_box(esw, ene).collect();
                let single = cells.len() == 1;
                for (r, c) in cells {
                    let (csw, cne) = self.grid.bounds(r, c);
                    if single || segment_touches_box(a, b, csw, cne) {
                        edge_cells.entry((r, c)).or_default().push(base + i as u64);
                    }
                }
            }
        }

        let center = |r: u32, c: u32| {
            let (csw, cne) = self.grid.bounds(r, c);
            Point {
                lat: ((csw.lat as i64 + cne.lat as i64) / 2) as i32,
                lon: ((csw.lon as i64 + cne.lon as i64) / 2) as i32,
            }
        };
        let (r0, r1) = (self.grid.row(sw.lat), self.grid.row(ne.lat));
        let (c0, c1) = (self.grid.col(sw.lon), self.grid.col(ne.lon));
        for r in r0..=r1 {
            let mut c = c0;
            while c <= c1 {
                if let Some(edges) = edge_cells.remove(&(r, c)) {
                    let inside = rings_contain(&rings, center(r, c));
                    self.entries.push((
                        GridSpec::key(r, c),
                        AreaCell {
                            area,
                            center_inside: inside as u32,
                            edge_off: 0,
                            edge_n: edges.len() as u32,
                            _pad: 0,
                        },
                        edges,
                    ));
                    c += 1;
                    continue;
                }
                // A run of cells with no edges is wholly inside or outside.
                let start = c;
                while c <= c1 && !edge_cells.contains_key(&(r, c)) {
                    c += 1;
                }
                if rings_contain(&rings, center(r, start)) {
                    for cc in start..c {
                        self.entries.push((
                            GridSpec::key(r, cc),
                            AreaCell {
                                area,
                                center_inside: 1,
                                edge_off: 0,
                                edge_n: 0,
                                _pad: 0,
                            },
                            Vec::new(),
                        ));
                    }
                }
            }
        }
    }

    pub fn write(&mut self, w: &mut IndexWriter) -> Result<()> {
        let mut entries = std::mem::take(&mut self.entries);
        entries.sort_by_key(|(k, c, _)| (*k, c.area));
        let mut cells = Vec::with_capacity(entries.len());
        let mut edges: Vec<u64> = Vec::new();
        let mut grid: Vec<GridCell> = Vec::new();
        for (key, mut cell, e) in entries {
            cell.edge_off = edges.len() as u64;
            edges.extend_from_slice(&e);
            match grid.last_mut() {
                Some(g) if g.key == key => g.n += 1,
                _ => grid.push(GridCell {
                    key,
                    off: cells.len() as u64,
                    n: 1,
                    _pad: 0,
                }),
            }
            cells.push(cell);
        }
        w.add(sec::AREAS, &self.areas)?;
        w.add(sec::AREA_POINTS, &self.points)?;
        w.add(sec::AREA_EDGES, &edges)?;
        w.add(sec::AREA_CELLS, &cells)?;
        w.add(sec::AREA_GRID, &grid)?;
        Ok(())
    }
}

/// Rings (closed, outer rings and holes) and area in km² of a Polygon or
/// MultiPolygon. Returns `None` for other types or broken coordinates.
fn parse_geometry(g: &Value) -> Option<(Vec<Vec<Point>>, f64)> {
    let polys: Vec<&Value> = match g["type"].as_str()? {
        "Polygon" => vec![&g["coordinates"]],
        "MultiPolygon" => g["coordinates"].as_array()?.iter().collect(),
        _ => return None,
    };
    let mut rings = Vec::new();
    let mut km2 = 0.0;
    for poly in polys {
        for (i, ring) in poly.as_array()?.iter().enumerate() {
            let mut pts: Vec<Point> = ring
                .as_array()?
                .iter()
                .map(|c| Point::from_deg(c[1].as_f64()?, c[0].as_f64()?))
                .collect::<Option<_>>()?;
            pts.dedup();
            if pts.len() < 4 {
                continue;
            }
            if pts.first() != pts.last() {
                pts.push(pts[0]);
            }
            let a = ring_km2(&pts);
            km2 += if i == 0 { a } else { -a };
            rings.push(pts);
        }
    }
    (!rings.is_empty()).then_some((rings, km2.max(0.0)))
}

/// Unsigned ring area in km², in a local frame at the ring's first vertex.
fn ring_km2(pts: &[Point]) -> f64 {
    let f = LocalFrame::new(pts[0]);
    let mut a2 = 0.0;
    let mut prev = f.xy(pts[0]);
    for &p in &pts[1..] {
        let cur = f.xy(p);
        a2 += prev.0 * cur.1 - cur.0 * prev.1;
        prev = cur;
    }
    (a2 / 2.0).abs() / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tables;
    use waypost_core::model::{NameTable, StringTable};

    fn tables() -> Tables {
        Tables {
            strings: StringTable::default(),
            names: NameTable::default(),
            langs: vec!["en".into()],
        }
    }

    #[test]
    fn parses_polygon_with_hole() {
        let g: Value = serde_json::from_str(
            r#"{"type":"Polygon","coordinates":[
                [[0,0],[1,0],[1,1],[0,1],[0,0]],
                [[0.4,0.4],[0.6,0.4],[0.6,0.6],[0.4,0.6],[0.4,0.4]]]}"#,
        )
        .unwrap();
        let (rings, km2) = parse_geometry(&g).unwrap();
        assert_eq!(rings.len(), 2);
        // About 12364 km² for the 1x1 degree square at the equator, minus 4%.
        assert!((km2 - 12364.0 * 0.96).abs() < 100.0, "{km2}");
    }

    #[test]
    fn cells_cover_inside_and_skip_hole() {
        let s = BuildSettings {
            street_cell: 50_000,
            admin_cell: 1_000_000,
            langs: vec![],
        };
        let mut a = Areas::new(&s);
        let mut t = tables();
        let f: Value = serde_json::from_str(
            r#"{"type":"Feature","geometry":{"type":"Polygon","coordinates":[
                [[0,0],[1,0],[1,1],[0,1],[0,0]],
                [[0.3,0.3],[0.7,0.3],[0.7,0.7],[0.3,0.7],[0.3,0.3]]]},
               "properties":{"boundary":"administrative","admin_level":"4","name":"Test","@id":7}}"#,
        )
        .unwrap();
        assert!(a.feature(&f, &mut t));
        // 0.1 degree cells: 10x10 grid. Hole interior cells (0.4..0.6) have no entry.
        let g = a.grid;
        let has = |lat: f64, lon: f64| {
            let p = Point::from_deg(lat, lon).unwrap();
            let k = g.key_of(p);
            a.entries.iter().any(|(kk, _, _)| *kk == k)
        };
        assert!(has(0.15, 0.15));
        assert!(!has(0.45, 0.45));
        assert!(!has(1.5, 0.5));
    }
}
