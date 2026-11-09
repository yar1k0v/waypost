//! Streets, address points, interpolation ways and place nodes, read from a
//! `.streets.pbf` whose ways carry node locations (osmium add-locations-to-ways).

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use osmpbf::{Element, ElementReader};
use waypost_core::format::{BuildSettings, IndexWriter};
use waypost_core::geo::{LocalFrame, Point};
use waypost_core::model::{
    Addr, GridBuilder, GridSpec, Interp, NONE, OsmType, PIECE_MAX_SEGMENTS, Piece, Place,
    PlaceKind, RoadClass, StrRef, Way, sec,
};

use crate::Tables;
use crate::tags::{Tags, leading_number, maxspeed_kmh, name_of};

pub struct Streets {
    grid: GridSpec,
    place_grid: GridSpec,
    nodes: Vec<Point>,
    ways: Vec<Way>,
    pieces: Vec<Piece>,
    addrs: Vec<Addr>,
    interps: Vec<Interp>,
    places: Vec<Place>,
    skipped_ways: u64,
    skipped_interps: u64,
}

/// House number and street of an address node, kept for interpolation ends.
struct AddrNode {
    number: Option<u32>,
    street: StrRef,
}

impl Streets {
    pub fn new(s: &BuildSettings) -> Streets {
        Streets {
            grid: GridSpec {
                size: s.street_cell,
            },
            place_grid: GridSpec { size: s.admin_cell },
            nodes: Vec::new(),
            ways: Vec::new(),
            pieces: Vec::new(),
            addrs: Vec::new(),
            interps: Vec::new(),
            places: Vec::new(),
            skipped_ways: 0,
            skipped_interps: 0,
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "{} ways, {} pieces, {} nodes, {} addresses, {} interpolations, {} places; skipped {} ways and {} interpolations",
            self.ways.len(),
            self.pieces.len(),
            self.nodes.len(),
            self.addrs.len(),
            self.interps.len(),
            self.places.len(),
            self.skipped_ways,
            self.skipped_interps
        )
    }

    pub fn read(&mut self, path: &Path, t: &mut Tables) -> Result<()> {
        let mut addr_nodes: HashMap<i64, AddrNode> = HashMap::new();
        let reader = ElementReader::from_path(path)?;
        reader.for_each(|el| match el {
            Element::Node(n) => {
                let tags = Tags(n.tags().collect());
                self.node(
                    n.id(),
                    n.decimicro_lat(),
                    n.decimicro_lon(),
                    &tags,
                    t,
                    &mut addr_nodes,
                );
            }
            Element::DenseNode(n) => {
                let tags = Tags(n.tags().collect());
                self.node(
                    n.id(),
                    n.decimicro_lat(),
                    n.decimicro_lon(),
                    &tags,
                    t,
                    &mut addr_nodes,
                );
            }
            Element::Way(w) => {
                let tags = Tags(w.tags().collect());
                let pts: Option<Vec<Point>> = w
                    .node_locations()
                    .map(|l| valid(l.decimicro_lat(), l.decimicro_lon()))
                    .collect();
                let Some(pts) = pts.filter(|p| p.len() >= 2) else {
                    self.skipped_ways += 1;
                    return;
                };
                let refs: Vec<i64> = w.refs().collect();
                self.way(w.id(), &tags, pts, &refs, t, &addr_nodes);
            }
            Element::Relation(_) => {}
        })?;
        Ok(())
    }

    fn node(
        &mut self,
        id: i64,
        lat: i32,
        lon: i32,
        tags: &Tags,
        t: &mut Tables,
        addr_nodes: &mut HashMap<i64, AddrNode>,
    ) {
        let Some(pt) = valid(lat, lon) else { return };
        if let Some(hn) = tags.get("addr:housenumber") {
            let street = t.strings.add_opt(tags.get("addr:street"));
            addr_nodes.insert(
                id,
                AddrNode {
                    number: leading_number(hn),
                    street,
                },
            );
            self.add_addr(pt, (OsmType::Node, id), hn, tags, t);
        }
        if let Some(kind) = tags.get("place").and_then(PlaceKind::from_tag) {
            let name = name_of(t, |k| tags.get(k).map(str::to_string), None);
            if name != NONE {
                self.places.push(Place {
                    pt,
                    name,
                    kind: kind as u8,
                    _pad: [0; 3],
                    osm_id: id as u64,
                });
            }
        }
    }

    fn add_addr(
        &mut self,
        pt: Point,
        (osm_type, osm_id): (OsmType, i64),
        hn: &str,
        tags: &Tags,
        t: &mut Tables,
    ) {
        let street = tags.get("addr:street");
        let place = tags.get("addr:place");
        // An address needs something to attach the number to.
        if street.is_none() && place.is_none() {
            return;
        }
        self.addrs.push(Addr {
            pt,
            housenumber: t.strings.add(hn.trim()),
            street: t.strings.add_opt(street),
            place: t.strings.add_opt(place),
            postcode: t.strings.add_opt(tags.get("addr:postcode")),
            city: t.strings.add_opt(tags.get("addr:city")),
            osm_id: osm_id as u64,
            osm_type: osm_type as u8,
            _pad: [0; 7],
        });
    }

    fn way(
        &mut self,
        id: i64,
        tags: &Tags,
        pts: Vec<Point>,
        refs: &[i64],
        t: &mut Tables,
        addr_nodes: &HashMap<i64, AddrNode>,
    ) {
        if let Some(hn) = tags.get("addr:housenumber") {
            self.add_addr(centroid(&pts), (OsmType::Way, id), hn, tags, t);
        }
        if let Some(kind) = tags.get("addr:interpolation") {
            self.interp(id, kind, tags, &pts, refs, t, addr_nodes);
        }
        let Some(class) = tags.get("highway").and_then(RoadClass::from_highway) else {
            return;
        };
        // Unnamed roads stay: snapping needs them. Reverse lookups skip them.
        let name = name_of(t, |k| tags.get(k).map(str::to_string), Some("ref"));
        let way = self.ways.len() as u32;
        self.ways.push(Way {
            name,
            class: class as u8,
            _pad: [0; 3],
            maxspeed: maxspeed_kmh(tags.get("maxspeed")),
            _pad2: [0; 6],
            osm_id: id as u64,
        });
        let base = self.nodes.len() as u64;
        self.nodes.extend_from_slice(&pts);
        // Pieces share their boundary node: piece k covers nodes
        // [k*S, k*S + S] of the way.
        let segs = pts.len() - 1;
        let mut start = 0;
        while start < segs {
            let n_seg = (segs - start).min(PIECE_MAX_SEGMENTS);
            self.pieces.push(Piece {
                node_off: base + start as u64,
                way,
                n: (n_seg + 1) as u32,
            });
            start += n_seg;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn interp(
        &mut self,
        id: i64,
        kind: &str,
        tags: &Tags,
        pts: &[Point],
        refs: &[i64],
        t: &mut Tables,
        addr_nodes: &HashMap<i64, AddrNode>,
    ) {
        let step = match kind {
            "odd" | "even" => 2,
            "all" | "1" => 1,
            "2" => 2,
            _ => {
                self.skipped_interps += 1;
                return;
            }
        };
        let (Some(a), Some(b)) = (
            refs.first().and_then(|r| addr_nodes.get(r)),
            refs.last().and_then(|r| addr_nodes.get(r)),
        ) else {
            self.skipped_interps += 1;
            return;
        };
        let (Some(start), Some(end)) = (a.number, b.number) else {
            self.skipped_interps += 1;
            return;
        };
        // The way's own addr:street wins; else the endpoints' street.
        let street = match tags.get("addr:street") {
            Some(s) => t.strings.add(s),
            None if !a.street.is_empty() => a.street,
            None => b.street,
        };
        if street.is_empty() || start == end {
            self.skipped_interps += 1;
            return;
        }
        let node_off = self.nodes.len() as u64;
        self.nodes.extend_from_slice(pts);
        self.interps.push(Interp {
            node_off,
            n: pts.len() as u32,
            start,
            end,
            step,
            street,
            osm_id: id as u64,
        });
    }

    pub fn write(&self, w: &mut IndexWriter) -> Result<()> {
        let bbox_cells = |grid: &GridSpec, pts: &[Point]| {
            let (mut sw, mut ne) = (pts[0], pts[0]);
            for p in pts {
                sw.lat = sw.lat.min(p.lat);
                sw.lon = sw.lon.min(p.lon);
                ne.lat = ne.lat.max(p.lat);
                ne.lon = ne.lon.max(p.lon);
            }
            grid.cells_in_box(sw, ne).collect::<Vec<_>>()
        };

        let mut g = GridBuilder::default();
        for (i, p) in self.pieces.iter().enumerate() {
            let pts = &self.nodes[p.node_off as usize..(p.node_off + p.n as u64) as usize];
            for (r, c) in bbox_cells(&self.grid, pts) {
                g.insert(GridSpec::key(r, c), i as u32);
            }
        }
        let (cells, ids) = g.finish();
        w.add(sec::PIECE_GRID, &cells)?;
        w.add(sec::PIECE_IDS, &ids)?;

        let mut g = GridBuilder::default();
        for (i, a) in self.addrs.iter().enumerate() {
            g.insert(self.grid.key_of(a.pt), i as u32);
        }
        let (cells, ids) = g.finish();
        w.add(sec::ADDR_GRID, &cells)?;
        w.add(sec::ADDR_IDS, &ids)?;

        let mut g = GridBuilder::default();
        for (i, it) in self.interps.iter().enumerate() {
            let pts = &self.nodes[it.node_off as usize..(it.node_off + it.n as u64) as usize];
            for (r, c) in bbox_cells(&self.grid, pts) {
                g.insert(GridSpec::key(r, c), i as u32);
            }
        }
        let (cells, ids) = g.finish();
        w.add(sec::INTERP_GRID, &cells)?;
        w.add(sec::INTERP_IDS, &ids)?;

        let mut g = GridBuilder::default();
        for (i, p) in self.places.iter().enumerate() {
            g.insert(self.place_grid.key_of(p.pt), i as u32);
        }
        let (cells, ids) = g.finish();
        w.add(sec::PLACE_GRID, &cells)?;
        w.add(sec::PLACE_IDS, &ids)?;

        w.add(sec::NODES, &self.nodes)?;
        w.add(sec::WAYS, &self.ways)?;
        w.add(sec::PIECES, &self.pieces)?;
        w.add(sec::ADDRS, &self.addrs)?;
        w.add(sec::INTERPS, &self.interps)?;
        w.add(sec::PLACES, &self.places)?;
        Ok(())
    }
}

/// A location from osmium, or `None` when osmium marked it undefined.
fn valid(lat: i32, lon: i32) -> Option<Point> {
    ((-900_000_000..=900_000_000).contains(&lat) && (-1_800_000_000..=1_800_000_000).contains(&lon))
        .then_some(Point { lat, lon })
}

/// Area centroid of a closed ring, or the vertex average for open ways and
/// degenerate rings.
fn centroid(pts: &[Point]) -> Point {
    let f = LocalFrame::new(pts[0]);
    let xy: Vec<(f64, f64)> = pts.iter().map(|&p| f.xy(p)).collect();
    if pts.len() >= 4 && pts.first() == pts.last() {
        let (mut a2, mut cx, mut cy) = (0.0, 0.0, 0.0);
        for w in xy.windows(2) {
            let ((x0, y0), (x1, y1)) = (w[0], w[1]);
            let cr = x0 * y1 - x1 * y0;
            a2 += cr;
            cx += (x0 + x1) * cr;
            cy += (y0 + y1) * cr;
        }
        if a2.abs() > 1e-6 {
            return f.point_at(cx / (3.0 * a2), cy / (3.0 * a2));
        }
    }
    let n = xy.len() as f64;
    let (sx, sy) = xy.iter().fold((0.0, 0.0), |(a, b), (x, y)| (a + x, b + y));
    f.point_at(sx / n, sy / n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centroid_of_square() {
        let p = |lat: f64, lon: f64| Point::from_deg(lat, lon).unwrap();
        let ring = [
            p(43.0, 76.0),
            p(43.0, 76.001),
            p(43.001, 76.001),
            p(43.001, 76.0),
            p(43.0, 76.0),
        ];
        let c = centroid(&ring);
        assert!(
            (c.lat - 430_005_000).abs() <= 2 && (c.lon - 760_005_000).abs() <= 2,
            "{c:?}"
        );
    }
}
