//! Reverse geocoding over a memory-mapped index.
//!
//! The index is immutable and shared by all threads without locks. One
//! lookup reads a few grid cells: the street, address and interpolation
//! cells within the search radius, one admin cell and the place cells around.

use std::borrow::Cow;
use std::ops::Range;
use std::path::Path;

use zerocopy::{FromBytes, Immutable, KnownLayout};

use crate::format::{FormatError, IndexFile, Meta};
use crate::geo::{LocalFrame, Point, Projection, SCALE};
use crate::model::*;
use crate::pip::crosses;

/// Largest search radius a caller may ask for, in metres.
pub const MAX_RADIUS_M: f64 = 1_000.0;
/// Search radius when the caller gives none, in metres.
pub const DEFAULT_RADIUS_M: f64 = 100.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// An address point (node or building).
    Address,
    /// A number interpolated along an `addr:interpolation` way.
    Interpolated,
    /// The nearest named street, without a house number.
    Street,
    /// No street within the radius; the nearest settlement.
    Place,
    /// Admin areas only.
    Admin,
    /// Nothing found.
    None,
}

/// Result of one lookup. Strings borrow from the index where possible.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Address<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub house_number: Option<Cow<'a, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub street: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub road_class: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suburb: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locality: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub district: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub postcode: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country_code: Option<String>,
}

/// Where an answer comes from: the OSM object that decided the match, and
/// the OSM data date of the index.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Source<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub osm_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub osm_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_date: Option<&'a str>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Answer<'a> {
    #[serde(rename = "match")]
    pub kind: MatchKind,
    /// Metres from the query to the matched feature (house, street or place).
    /// 0 when an area that contains the point decided the match.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance_m: Option<f64>,
    pub address: Address<'a>,
    pub display: String,
    pub source: Source<'a>,
}

/// Result of a snap: the nearest point on a road.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Snapped<'a> {
    pub lat: f64,
    pub lon: f64,
    /// Metres from the query point to the snapped point.
    pub distance_m: f64,
    pub road: Road<'a>,
    pub source: Source<'a>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Road<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
    pub class: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maxspeed_kmh: Option<u16>,
    pub osm_way_id: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Query {
    pub pt: Point,
    /// Search radius for houses and streets, metres. Clamped to (0, MAX_RADIUS_M].
    pub radius_m: f64,
    /// Index into the index's languages, or `None` for the `name` tag.
    pub lang: Option<u32>,
    /// Admin areas only: skip houses, streets and places.
    pub admin_only: bool,
    pub street_rule: StreetRule,
}

/// Which street a lookup names when a road and a house disagree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreetRule {
    /// The nearest road names the street, unless a house is closer than
    /// every road: then that house's street and number. The legacy geocoder's rule.
    #[default]
    Hybrid,
    /// The nearest road always names the street. A house number comes only
    /// from a house on that road.
    Road,
}

impl StreetRule {
    pub fn parse(v: &str) -> Option<StreetRule> {
        match v {
            "hybrid" => Some(StreetRule::Hybrid),
            "road" => Some(StreetRule::Road),
            _ => None,
        }
    }
}

/// Validated byte range of one section.
#[derive(Clone)]
struct Sec(Range<usize>);

macro_rules! sections {
    ($($field:ident: $name:expr),* $(,)?) => {
        struct Secs { $($field: Sec,)* }
        impl Secs {
            fn load(f: &IndexFile) -> Result<Secs, FormatError> {
                Ok(Secs { $($field: Sec(range(f, $name)?),)* })
            }
        }
    };
}

sections! {
    strings: sec::STRINGS, names: sec::NAMES, name_alts: sec::NAME_ALTS,
    nodes: sec::NODES, ways: sec::WAYS, pieces: sec::PIECES,
    piece_grid: sec::PIECE_GRID, piece_ids: sec::PIECE_IDS,
    addrs: sec::ADDRS, addr_grid: sec::ADDR_GRID, addr_ids: sec::ADDR_IDS,
    interps: sec::INTERPS, interp_grid: sec::INTERP_GRID, interp_ids: sec::INTERP_IDS,
    places: sec::PLACES, place_grid: sec::PLACE_GRID, place_ids: sec::PLACE_IDS,
    areas: sec::AREAS, area_points: sec::AREA_POINTS, area_edges: sec::AREA_EDGES,
    area_cells: sec::AREA_CELLS, area_grid: sec::AREA_GRID,
}

fn range(f: &IndexFile, name: &str) -> Result<Range<usize>, FormatError> {
    let s = f
        .meta()
        .sections
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| FormatError::MissingSection(name.to_string()))?;
    Ok(s.offset as usize..(s.offset + s.len) as usize)
}

pub struct Index {
    file: IndexFile,
    s: Secs,
    street_grid: GridSpec,
    admin_grid: GridSpec,
}

/// Best candidate of one kind: distance and id.
#[derive(Clone, Copy)]
struct Best {
    dist: f64,
    id: u32,
    t: f64,
}

impl Index {
    pub fn open(path: impl AsRef<Path>) -> Result<Index, FormatError> {
        let file = IndexFile::open(path)?;
        let s = Secs::load(&file)?;
        let idx = Index {
            street_grid: GridSpec {
                size: file.meta().settings.street_cell,
            },
            admin_grid: GridSpec {
                size: file.meta().settings.admin_cell,
            },
            file,
            s,
        };
        idx.validate()?;
        Ok(idx)
    }

    /// Checks record sizes and that every cross-reference stays in bounds,
    /// so lookups can index without panicking on a damaged file.
    fn validate(&self) -> Result<(), FormatError> {
        if self.street_grid.size <= 0 || self.admin_grid.size <= 0 {
            return Err(FormatError::Corrupt(
                "grid cell size is not positive".into(),
            ));
        }
        let bad = |what: &str| FormatError::Corrupt(format!("{what} out of bounds"));
        let n_strings = self.bytes(&self.s.strings).len() as u64;
        let str_ok = |r: StrRef| r.off as u64 + r.len as u64 <= n_strings;
        let n_names = self.view::<NameRec>("names", &self.s.names)?.len();
        let n_alts = self.view::<NameAlt>("name_alts", &self.s.name_alts)?.len();
        let name_ok = |n: NameId| n == NONE || (n as usize) < n_names;
        for n in self.view::<NameRec>("names", &self.s.names)? {
            if !str_ok(n.default) || n.alt_off as usize + n.alt_n as usize > n_alts {
                return Err(bad("name"));
            }
        }
        for a in self.view::<NameAlt>("name_alts", &self.s.name_alts)? {
            if !str_ok(a.s) {
                return Err(bad("name translation"));
            }
        }
        let n_nodes = self.view::<Point>("nodes", &self.s.nodes)?.len() as u64;
        let n_ways = self.view::<Way>("ways", &self.s.ways)?.len();
        for w in self.view::<Way>("ways", &self.s.ways)? {
            if !name_ok(w.name) {
                return Err(bad("way name"));
            }
        }
        for p in self.view::<Piece>("pieces", &self.s.pieces)? {
            if p.n < 2 || p.node_off + p.n as u64 > n_nodes || p.way as usize >= n_ways {
                return Err(bad("piece"));
            }
        }
        for a in self.view::<Addr>("addrs", &self.s.addrs)? {
            if ![a.housenumber, a.street, a.place, a.postcode, a.city]
                .into_iter()
                .all(str_ok)
            {
                return Err(bad("address"));
            }
        }
        for i in self.view::<Interp>("interps", &self.s.interps)? {
            if i.n < 2 || i.node_off + i.n as u64 > n_nodes || !str_ok(i.street) {
                return Err(bad("interpolation"));
            }
        }
        for p in self.view::<Place>("places", &self.s.places)? {
            if !name_ok(p.name) {
                return Err(bad("place"));
            }
        }
        let n_area_pts = self
            .view::<Point>("area_points", &self.s.area_points)?
            .len() as u64;
        let n_areas = self.view::<Area>("areas", &self.s.areas)?.len();
        for a in self.view::<Area>("areas", &self.s.areas)? {
            if !name_ok(a.name) || !str_ok(a.postcode) {
                return Err(bad("area"));
            }
        }
        let edges = self.view::<u64>("area_edges", &self.s.area_edges)?;
        if edges.iter().any(|&e| e + 1 >= n_area_pts) {
            return Err(bad("area edge"));
        }
        let cells = self.view::<AreaCell>("area_cells", &self.s.area_cells)?;
        for c in cells {
            if c.area as usize >= n_areas || c.edge_off + c.edge_n as u64 > edges.len() as u64 {
                return Err(bad("area cell"));
            }
        }
        let n_pieces = self.view::<Piece>("pieces", &self.s.pieces)?.len();
        let n_addrs = self.view::<Addr>("addrs", &self.s.addrs)?.len();
        let n_interps = self.view::<Interp>("interps", &self.s.interps)?.len();
        let n_places = self.view::<Place>("places", &self.s.places)?.len();
        // (grid name, grid, id list or None for area cells, records the ids point at)
        let grids = [
            (
                "piece_grid",
                &self.s.piece_grid,
                Some(("piece_ids", &self.s.piece_ids)),
                n_pieces,
            ),
            (
                "addr_grid",
                &self.s.addr_grid,
                Some(("addr_ids", &self.s.addr_ids)),
                n_addrs,
            ),
            (
                "interp_grid",
                &self.s.interp_grid,
                Some(("interp_ids", &self.s.interp_ids)),
                n_interps,
            ),
            (
                "place_grid",
                &self.s.place_grid,
                Some(("place_ids", &self.s.place_ids)),
                n_places,
            ),
            ("area_grid", &self.s.area_grid, None, cells.len()),
        ];
        for (name, sec, ids, n_targets) in grids {
            let g = self.view::<GridCell>(name, sec)?;
            if g.windows(2).any(|w| w[0].key >= w[1].key) {
                return Err(FormatError::Corrupt(format!("{name} is not sorted")));
            }
            let n_ids = match ids {
                Some((ids_name, ids_sec)) => {
                    let ids = self.view::<u32>(ids_name, ids_sec)?;
                    if ids.iter().any(|&id| id as usize >= n_targets) {
                        return Err(bad(ids_name));
                    }
                    ids.len()
                }
                None => n_targets,
            };
            if g.iter().any(|c| c.off + c.n as u64 > n_ids as u64) {
                return Err(bad(name));
            }
        }
        Ok(())
    }

    pub fn meta(&self) -> &Meta {
        self.file.meta()
    }

    /// Touches every page of the index. Call before reporting ready.
    pub fn warm(&self) -> u64 {
        self.file.warm()
    }

    fn bytes(&self, s: &Sec) -> &[u8] {
        &self.file_bytes()[s.0.clone()]
    }

    fn file_bytes(&self) -> &[u8] {
        // The whole map; sections were bounds-checked at open.
        self.file.bytes_all()
    }

    fn view<T: FromBytes + Immutable + KnownLayout>(
        &self,
        name: &str,
        s: &Sec,
    ) -> Result<&[T], FormatError> {
        <[T]>::ref_from_bytes(self.bytes(s)).map_err(|_| FormatError::BadSection {
            name: name.to_string(),
            reason: "record size or alignment".into(),
        })
    }

    /// A section known to be valid since `open`.
    fn v<T: FromBytes + Immutable + KnownLayout>(&self, s: &Sec) -> &[T] {
        <[T]>::ref_from_bytes(self.bytes(s)).unwrap_or(&[])
    }

    fn str(&self, r: StrRef) -> Option<&str> {
        if r.is_empty() {
            return None;
        }
        let b = self.bytes(&self.s.strings);
        std::str::from_utf8(&b[r.off as usize..(r.off + r.len) as usize]).ok()
    }

    fn name(&self, id: NameId, lang: Option<u32>) -> Option<&str> {
        let n = self.v::<NameRec>(&self.s.names).get(id as usize)?;
        if let Some(l) = lang {
            let alts = &self.v::<NameAlt>(&self.s.name_alts)
                [n.alt_off as usize..(n.alt_off + n.alt_n) as usize];
            if let Some(a) = alts.iter().find(|a| a.lang == l) {
                return self.str(a.s);
            }
        }
        self.str(n.default)
    }

    /// Ids in all grid cells within `radius_m` of `pt`.
    fn ids_near<'s>(
        &'s self,
        grid: &GridSpec,
        cells: &'s Sec,
        ids: &'s Sec,
        pt: Point,
        radius_m: f64,
    ) -> impl Iterator<Item = u32> + 's {
        let f = LocalFrame::new(pt);
        let (sw, ne) = (
            f.point_at(-radius_m, -radius_m),
            f.point_at(radius_m, radius_m),
        );
        let cells: &[GridCell] = self.v(cells);
        let ids: &[u32] = self.v(ids);
        // Near the antimeridian the box wraps; split columns in that case.
        let col_ranges: Vec<(u32, u32)> = if sw.lon <= ne.lon {
            vec![(grid.col(sw.lon), grid.col(ne.lon))]
        } else {
            vec![(grid.col(sw.lon), grid.cols()), (0, grid.col(ne.lon))]
        };
        let (r0, r1) = (grid.row(sw.lat), grid.row(ne.lat));
        (r0..=r1)
            .flat_map(move |r| {
                col_ranges
                    .clone()
                    .into_iter()
                    .flat_map(move |(c0, c1)| (c0..=c1).map(move |c| GridSpec::key(r, c)))
            })
            .filter_map(move |k| find_cell(cells, k))
            .flat_map(move |c| {
                ids[c.off as usize..(c.off + c.n as u64) as usize]
                    .iter()
                    .copied()
            })
    }

    /// Nearest road segment within `radius` among ways that `keep` accepts.
    /// Returns the way (in `Best::id`) and the projection onto the segment.
    fn nearest_piece(
        &self,
        f: &LocalFrame,
        radius: f64,
        keep: impl Fn(&Way) -> bool,
    ) -> Option<(Best, Projection)> {
        let pieces: &[Piece] = self.v(&self.s.pieces);
        let nodes: &[Point] = self.v(&self.s.nodes);
        let ways: &[Way] = self.v(&self.s.ways);
        let mut best: Option<(Best, Projection)> = None;
        for id in self.ids_near(
            &self.street_grid,
            &self.s.piece_grid,
            &self.s.piece_ids,
            f.origin(),
            radius,
        ) {
            let p = &pieces[id as usize];
            if !keep(&ways[p.way as usize]) {
                continue;
            }
            let pts = &nodes[p.node_off as usize..(p.node_off + p.n as u64) as usize];
            for w in pts.windows(2) {
                let pr = f.project(w[0], w[1]);
                if pr.dist <= radius && best.is_none_or(|(b, _)| pr.dist < b.dist) {
                    best = Some((
                        Best {
                            dist: pr.dist,
                            id: p.way,
                            t: pr.t,
                        },
                        pr,
                    ));
                }
            }
        }
        best
    }

    /// The nearest point on a road within `radius_m` (clamped to
    /// `MAX_RADIUS_M`), or `None` when there is no road that close. With
    /// `vehicle_only`, pedestrian ways are skipped.
    pub fn snap(
        &self,
        pt: Point,
        radius_m: f64,
        lang: Option<u32>,
        vehicle_only: bool,
    ) -> Option<Snapped<'_>> {
        let radius = if radius_m > 0.0 {
            radius_m.min(MAX_RADIUS_M)
        } else {
            DEFAULT_RADIUS_M
        };
        let f = LocalFrame::new(pt);
        let (b, pr) = self.nearest_piece(&f, radius, |w| {
            !vehicle_only || RoadClass::from_u8(w.class) != RoadClass::Pedestrian
        })?;
        let way = &self.v::<Way>(&self.s.ways)[b.id as usize];
        let at = f.point_at(pr.x, pr.y);
        Some(Snapped {
            lat: at.lat_deg(),
            lon: at.lon_deg(),
            distance_m: (b.dist * 10.0).round() / 10.0,
            road: Road {
                name: if way.name == NONE {
                    None
                } else {
                    self.name(way.name, lang)
                },
                class: RoadClass::from_u8(way.class).as_str(),
                maxspeed_kmh: (way.maxspeed > 0).then_some(way.maxspeed),
                osm_way_id: way.osm_id,
            },
            source: self.source(Some((OsmType::Way, way.osm_id))),
        })
    }

    /// Nearest address point within `radius` whose `addr:street` passes `keep`.
    fn nearest_addr(
        &self,
        f: &LocalFrame,
        radius: f64,
        keep: impl Fn(StrRef) -> bool,
    ) -> Option<Best> {
        let addrs: &[Addr] = self.v(&self.s.addrs);
        let mut best: Option<Best> = None;
        for id in self.ids_near(
            &self.street_grid,
            &self.s.addr_grid,
            &self.s.addr_ids,
            f.origin(),
            radius,
        ) {
            let ad = &addrs[id as usize];
            if !keep(ad.street) {
                continue;
            }
            let d = f.dist(ad.pt);
            if d <= radius && best.is_none_or(|b| d < b.dist) {
                best = Some(Best {
                    dist: d,
                    id,
                    t: 0.0,
                });
            }
        }
        best
    }

    /// Nearest interpolation way whose street passes `keep`, with `t` as the
    /// fraction of its length.
    fn nearest_interp(
        &self,
        f: &LocalFrame,
        radius: f64,
        keep: impl Fn(StrRef) -> bool,
    ) -> Option<Best> {
        let interps: &[Interp] = self.v(&self.s.interps);
        let nodes: &[Point] = self.v(&self.s.nodes);
        let mut best: Option<Best> = None;
        for id in self.ids_near(
            &self.street_grid,
            &self.s.interp_grid,
            &self.s.interp_ids,
            f.origin(),
            radius,
        ) {
            let it = &interps[id as usize];
            if !keep(it.street) {
                continue;
            }
            let pts = &nodes[it.node_off as usize..(it.node_off + it.n as u64) as usize];
            let (mut along, mut total) = (0.0, 0.0);
            let (mut bd, mut bt) = (f64::MAX, 0.0);
            for w in pts.windows(2) {
                let pr = f.project(w[0], w[1]);
                if pr.dist < bd {
                    bd = pr.dist;
                    bt = along + pr.t * pr.seg_len;
                }
                along += pr.seg_len;
                total += pr.seg_len;
            }
            if total > 0.0 && bd <= radius && best.is_none_or(|b| bd < b.dist) {
                best = Some(Best {
                    dist: bd,
                    id,
                    t: bt / total,
                });
            }
        }
        best
    }

    /// Nearest place node whose own radius covers the point.
    fn place_near(&self, f: &LocalFrame, want: fn(PlaceKind) -> bool) -> Option<(f64, &Place)> {
        let places: &[Place] = self.v(&self.s.places);
        let mut best: Option<(f64, f64, &Place)> = None;
        for id in self.ids_near(
            &self.admin_grid,
            &self.s.place_grid,
            &self.s.place_ids,
            f.origin(),
            PLACE_MAX_RADIUS_M,
        ) {
            let p = &places[id as usize];
            let Some(kind) = PlaceKind::from_u8(p.kind) else {
                continue;
            };
            if !want(kind) {
                continue;
            }
            let d = f.dist(p.pt);
            let score = d / kind.radius_m();
            if score <= 1.0 && best.is_none_or(|(s, _, _)| score < s) {
                best = Some((score, d, p));
            }
        }
        best.map(|(_, d, p)| (d, p))
    }

    /// Areas containing the point, by exact test against the cell's edges.
    fn areas_at(&self, pt: Point) -> impl Iterator<Item = &Area> {
        let grid: &[GridCell] = self.v(&self.s.area_grid);
        let cells: &[AreaCell] = self.v(&self.s.area_cells);
        let edges: &[u64] = self.v(&self.s.area_edges);
        let points: &[Point] = self.v(&self.s.area_points);
        let areas: &[Area] = self.v(&self.s.areas);
        let (r, c) = (self.admin_grid.row(pt.lat), self.admin_grid.col(pt.lon));
        let (sw, ne) = self.admin_grid.bounds(r, c);
        let center = Point {
            lat: ((sw.lat as i64 + ne.lat as i64) / 2) as i32,
            lon: ((sw.lon as i64 + ne.lon as i64) / 2) as i32,
        };
        let run = find_cell(grid, GridSpec::key(r, c)).map_or(&[][..], |g| {
            &cells[g.off as usize..(g.off + g.n as u64) as usize]
        });
        run.iter().filter_map(move |ac| {
            let mut inside = ac.center_inside == 1;
            for &e in &edges[ac.edge_off as usize..(ac.edge_off + ac.edge_n as u64) as usize] {
                if crosses(center, pt, points[e as usize], points[e as usize + 1]) {
                    inside = !inside;
                }
            }
            inside.then(|| &areas[ac.area as usize])
        })
    }

    /// The answer source for an OSM object, or for the index alone.
    fn source(&self, osm: Option<(OsmType, u64)>) -> Source<'_> {
        Source {
            osm_type: osm.map(|(t, _)| t.as_str()),
            osm_id: osm.map(|(_, id)| id),
            data_date: self.meta().data_date.as_deref(),
        }
    }

    /// The default name and every translation of a name.
    fn all_names(&self, id: NameId) -> Vec<&str> {
        let Some(n) = self.v::<NameRec>(&self.s.names).get(id as usize) else {
            return Vec::new();
        };
        let alts = &self.v::<NameAlt>(&self.s.name_alts)
            [n.alt_off as usize..(n.alt_off + n.alt_n) as usize];
        std::iter::once(n.default)
            .chain(alts.iter().map(|a| a.s))
            .filter_map(|r| self.str(r))
            .collect()
    }

    pub fn lookup(&self, q: &Query) -> Answer<'_> {
        let radius = if q.radius_m > 0.0 {
            q.radius_m.min(MAX_RADIUS_M)
        } else {
            DEFAULT_RADIUS_M
        };
        let f = LocalFrame::new(q.pt);
        let mut a = Address::default();

        // Admin fields: smallest matching area per field.
        let mut best_km2 = [f64::MAX; 6];
        let (state, district, city, suburb, country, postcode) = (0, 1, 2, 3, 4, 5);
        let mut set = |slot: usize, km2: f64| {
            if km2 < best_km2[slot] {
                best_km2[slot] = km2;
                true
            } else {
                false
            }
        };
        // The smallest area that filled a field, and the area that named the locality.
        let mut finest: Option<(f64, (OsmType, u64))> = None;
        let mut locality_area: Option<(OsmType, u64)> = None;
        for area in self.areas_at(q.pt) {
            let name = || self.name(area.name, q.lang);
            let osm = (
                OsmType::from_u8(area.osm_type).unwrap_or(OsmType::Way),
                area.osm_id,
            );
            let mut used = false;
            if area.kind == AreaKind::Admin as u8 {
                let slot = match area.level {
                    2 => country,
                    3 | 4 => state,
                    5 | 6 => district,
                    7 | 8 => city,
                    _ => suburb,
                };
                if set(slot, area.km2) {
                    used = true;
                    match slot {
                        0 => a.state = name(),
                        1 => a.district = name(),
                        2 => {
                            a.locality = name();
                            locality_area = Some(osm);
                        }
                        3 => a.suburb = name(),
                        _ => {
                            a.country = name();
                            a.country_code = (area.country != [0, 0])
                                .then(|| String::from_utf8_lossy(&area.country).into_owned());
                        }
                    }
                }
            } else if area.kind == AreaKind::Place as u8 {
                match PlaceKind::from_u8(area.level) {
                    Some(k) if k.is_locality() => {
                        if set(city, area.km2) {
                            used = true;
                            a.locality = name();
                            locality_area = Some(osm);
                        }
                    }
                    Some(_) if set(suburb, area.km2) => {
                        used = true;
                        a.suburb = name();
                    }
                    Some(_) => {}
                    None => {}
                }
            }
            if !area.postcode.is_empty() && set(postcode, area.km2) {
                used = true;
                a.postcode = self.str(area.postcode);
            }
            if used && finest.is_none_or(|(km2, _)| area.km2 < km2) {
                finest = Some((area.km2, osm));
            }
        }
        let finest = finest.map(|(_, osm)| osm);

        if q.admin_only {
            let (kind, dist) = match finest {
                Some(_) => (MatchKind::Admin, Some(0.0)),
                None => (MatchKind::None, None),
            };
            return self.finish(kind, dist, a, self.source(finest));
        }

        // The street is the nearest named road: for vehicles, the road they
        // are on. A house number comes only from a house on that road. With
        // no road in range (or, under Hybrid, a house nearer than any road),
        // the nearest house names the address.
        let street = self.nearest_piece(&f, radius, |w| w.name != NONE);
        let addrs: &[Addr] = self.v(&self.s.addrs);
        let interps: &[Interp] = self.v(&self.s.interps);
        let ways: &[Way] = self.v(&self.s.ways);
        // Hybrid: a house closer than every road takes over the address.
        let street = match (q.street_rule, street) {
            (StreetRule::Hybrid, Some((b, pr))) => {
                let closer = self.nearest_addr(&f, b.dist, |_| true);
                if closer.is_some_and(|h| h.dist < b.dist) {
                    None
                } else {
                    Some((b, pr))
                }
            }
            (_, s) => s,
        };
        // A house is on the road when its `addr:street` equals the road name
        // in any stored language, for example `name:ru` where `name` is Kazakh.
        let road_names = street.map_or(Vec::new(), |(b, _)| {
            self.all_names(ways[b.id as usize].name)
        });
        let keep = |s: StrRef| match street {
            Some(_) => self.str(s).is_some_and(|hs| road_names.contains(&hs)),
            None => true,
        };
        let addr = self.nearest_addr(&f, radius, keep);
        let interp = self.nearest_interp(&f, radius, keep);

        // The nearest house: an address point, or an interpolated number.
        let house = match (addr, interp) {
            (Some(ad), Some(it)) if it.dist < ad.dist => Some((MatchKind::Interpolated, it)),
            (Some(ad), _) => Some((MatchKind::Address, ad)),
            (None, Some(it)) => Some((MatchKind::Interpolated, it)),
            (None, None) => None,
        };
        let (mut house_kind, mut house_dist, mut house_osm) = (None, None, None);
        let mut house_street = StrRef::EMPTY;
        if let Some((k, h)) = house {
            house_kind = Some(k);
            house_dist = Some(h.dist);
            match k {
                MatchKind::Address => {
                    let ad = &addrs[h.id as usize];
                    house_osm = Some((
                        OsmType::from_u8(ad.osm_type).unwrap_or(OsmType::Node),
                        ad.osm_id,
                    ));
                    a.house_number = self.str(ad.housenumber).map(Cow::Borrowed);
                    house_street = ad.street;
                    if a.postcode.is_none() {
                        a.postcode = self.str(ad.postcode);
                    }
                    if street.is_none() && ad.street.is_empty() {
                        // addr:place addresses name the settlement instead.
                        if let Some(p) = self.str(ad.place) {
                            a.locality = Some(p);
                        }
                    }
                    if a.locality.is_none() {
                        a.locality = self.str(ad.city);
                    }
                }
                _ => {
                    let it = &interps[h.id as usize];
                    house_osm = Some((OsmType::Way, it.osm_id));
                    a.house_number = Some(Cow::Owned(interpolate(it, h.t).to_string()));
                    house_street = it.street;
                }
            }
        }

        let (mut kind, mut dist, mut osm) = match street {
            Some((b, _)) => {
                let way = &ways[b.id as usize];
                a.street = self.name(way.name, q.lang);
                a.road_class = Some(RoadClass::from_u8(way.class).as_str());
                match house_kind {
                    Some(k) => (k, Some(b.dist), house_osm),
                    None => (
                        MatchKind::Street,
                        Some(b.dist),
                        Some((OsmType::Way, way.osm_id)),
                    ),
                }
            }
            None => match house_kind {
                Some(k) => {
                    a.street = self.str(house_street);
                    (k, house_dist, house_osm)
                }
                None => (MatchKind::Place, Some(0.0), locality_area),
            },
        };

        if a.locality.is_none()
            && let Some((d, p)) = self.place_near(&f, PlaceKind::is_locality)
        {
            a.locality = self.name(p.name, q.lang);
            if kind == MatchKind::Place {
                dist = Some(d);
                osm = Some((OsmType::Node, p.osm_id));
            }
        }
        if a.suburb.is_none()
            && let Some((_, p)) = self.place_near(&f, |k| !k.is_locality())
        {
            a.suburb = self.name(p.name, q.lang);
        }
        if kind == MatchKind::Place && a.locality.is_none() {
            (kind, dist, osm) = match finest {
                Some(_) if a.country.is_some() => (MatchKind::Admin, Some(0.0), finest),
                _ => (MatchKind::None, None, None),
            };
        }
        self.finish(kind, dist, a, self.source(osm))
    }

    fn finish<'a>(
        &'a self,
        kind: MatchKind,
        dist: Option<f64>,
        a: Address<'a>,
        source: Source<'a>,
    ) -> Answer<'a> {
        let mut parts: Vec<String> = Vec::new();
        match (&a.street, &a.house_number) {
            (Some(s), Some(h)) => parts.push(format!("{s} {h}")),
            (Some(s), None) => parts.push(s.to_string()),
            (None, Some(h)) => parts.push(h.to_string()),
            _ => {}
        }
        for p in [
            a.suburb, a.locality, a.district, a.state, a.postcode, a.country,
        ]
        .into_iter()
        .flatten()
        {
            if parts.last().map(String::as_str) != Some(p) {
                parts.push(p.to_string());
            }
        }
        Answer {
            kind,
            distance_m: dist.map(|d| (d * 10.0).round() / 10.0),
            display: parts.join(", "),
            address: a,
            source,
        }
    }

    /// Language index for a code such as "de", or `None` for the default name.
    pub fn lang_index(&self, code: &str) -> Option<u32> {
        self.meta()
            .settings
            .langs
            .iter()
            .position(|l| l == code)
            .map(|i| i as u32)
    }
}

/// House number at fraction `t` of an interpolation way, on the way's
/// parity grid, for ascending and descending ranges.
pub fn interpolate(it: &Interp, t: f64) -> u32 {
    let (s, e, step) = (it.start as i64, it.end as i64, it.step.max(1) as i64);
    let span = e - s;
    let raw = span as f64 * t.clamp(0.0, 1.0);
    let k = (raw / step as f64).round() as i64 * step;
    let n = s + k.clamp(span.min(0), span.max(0));
    n.max(1) as u32
}

/// Degrees to fixed-point, for tests and callers.
pub fn deg(v: f64) -> i32 {
    (v * SCALE).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn it(start: u32, end: u32, step: u32) -> Interp {
        Interp {
            node_off: 0,
            n: 2,
            start,
            end,
            step,
            street: StrRef::EMPTY,
            osm_id: 0,
        }
    }

    #[test]
    fn interpolation_both_directions() {
        assert_eq!(interpolate(&it(2, 20, 2), 0.5), 12);
        assert_eq!(interpolate(&it(20, 2, 2), 0.5), 10);
        assert_eq!(interpolate(&it(20, 2, 2), 0.0), 20);
        assert_eq!(interpolate(&it(20, 2, 2), 1.0), 2);
        assert_eq!(interpolate(&it(1, 9, 2), 0.3), 3);
        assert_eq!(interpolate(&it(9, 1, 2), 0.3), 7);
        assert_eq!(interpolate(&it(1, 10, 1), 0.5), 6);
    }

    use crate::format::{BuildSettings, IndexWriter};

    /// Metres to 1e-7 degree units of latitude.
    const M: i32 = 90;

    /// A small world near 43N 76E: one road along the parallel, houses on
    /// it and on another street, a place node, and a country and a city
    /// area that cover the whole test grid cell.
    fn world(dir: &Path) -> Index {
        let (lat0, lon0) = (deg(43.0), deg(76.0));
        let at = |north_m: i32, east: i32| Point {
            lat: lat0 + north_m * M,
            lon: lon0 + east,
        };
        let mut strings = StringTable::default();
        let mut names = NameTable::default();
        // langs: 0 = en, 1 = ru. The road's `name` is Kazakh; houses use the Russian name.
        let road = names.add(
            strings.add("Абай көшесі"),
            &[(1, strings.add("улица Абая"))],
        );
        let country = names.add(strings.add("Қазақстан"), &[(0, strings.add("Kazakhstan"))]);
        let city = names.add(strings.add("Алматы"), &[(0, strings.add("Almaty"))]);
        let village = names.add(strings.add("Ауыл"), &[]);

        let nodes = vec![at(0, 0), at(0, 20_000)];
        let ways = [Way {
            name: road,
            class: RoadClass::Residential as u8,
            _pad: [0; 3],
            maxspeed: 60,
            _pad2: [0; 6],
            osm_id: 500,
        }];
        let pieces = [Piece {
            node_off: 0,
            way: 0,
            n: 2,
        }];
        let mut addr = |pt, hn: &str, street: &str, id| Addr {
            pt,
            housenumber: strings.add(hn),
            street: strings.add(street),
            place: StrRef::EMPTY,
            postcode: StrRef::EMPTY,
            city: StrRef::EMPTY,
            osm_id: id,
            osm_type: OsmType::Node as u8,
            _pad: [0; 7],
        };
        let addrs = [
            addr(at(15, 10_000), "10", "улица Абая", 1001),
            addr(at(-4, 15_000), "5", "Other", 1002),
        ];
        let places = [Place {
            pt: at(2_000, 0),
            name: village,
            kind: PlaceKind::Village as u8,
            _pad: [0; 3],
            osm_id: 700,
        }];
        let area = |name, level, cc: &[u8; 2], km2, id| Area {
            name,
            kind: AreaKind::Admin as u8,
            level,
            country: *cc,
            postcode: StrRef::EMPTY,
            km2,
            osm_id: id,
            osm_type: OsmType::Relation as u8,
            _pad: [0; 7],
        };
        let areas = [
            area(country, 2, b"KZ", 2.7e6, 1),
            area(city, 8, &[0, 0], 680.0, 2),
        ];

        let settings = BuildSettings {
            street_cell: 50_000,
            // One admin cell covers the whole test area.
            admin_cell: 100_000_000,
            langs: vec!["en".into(), "ru".into()],
        };
        let sg = GridSpec {
            size: settings.street_cell,
        };
        let ag = GridSpec {
            size: settings.admin_cell,
        };
        let mut g = GridBuilder::default();
        for (r, c) in sg.cells_in_box(nodes[0], nodes[1]) {
            g.insert(GridSpec::key(r, c), 0);
        }
        let (piece_grid, piece_ids) = g.finish();
        let mut g = GridBuilder::default();
        for (i, a) in addrs.iter().enumerate() {
            g.insert(sg.key_of(a.pt), i as u32);
        }
        let (addr_grid, addr_ids) = g.finish();
        let mut g = GridBuilder::default();
        g.insert(ag.key_of(places[0].pt), 0);
        let (place_grid, place_ids) = g.finish();
        let cell = |area| AreaCell {
            area,
            center_inside: 1,
            edge_off: 0,
            edge_n: 0,
            _pad: 0,
        };
        let area_cells = [cell(0), cell(1)];
        let area_grid = [GridCell {
            key: ag.key_of(at(0, 0)),
            off: 0,
            n: 2,
            _pad: 0,
        }];

        let path = dir.join("w.wpi");
        let mut w = IndexWriter::create(&path).unwrap();
        w.add(sec::NODES, &nodes).unwrap();
        w.add(sec::WAYS, &ways).unwrap();
        w.add(sec::PIECES, &pieces).unwrap();
        w.add(sec::PIECE_GRID, &piece_grid).unwrap();
        w.add(sec::PIECE_IDS, &piece_ids).unwrap();
        w.add(sec::ADDRS, &addrs).unwrap();
        w.add(sec::ADDR_GRID, &addr_grid).unwrap();
        w.add(sec::ADDR_IDS, &addr_ids).unwrap();
        w.add::<Interp>(sec::INTERPS, &[]).unwrap();
        w.add::<GridCell>(sec::INTERP_GRID, &[]).unwrap();
        w.add::<u32>(sec::INTERP_IDS, &[]).unwrap();
        w.add(sec::PLACES, &places).unwrap();
        w.add(sec::PLACE_GRID, &place_grid).unwrap();
        w.add(sec::PLACE_IDS, &place_ids).unwrap();
        w.add(sec::AREAS, &areas).unwrap();
        w.add::<Point>(sec::AREA_POINTS, &[]).unwrap();
        w.add::<u64>(sec::AREA_EDGES, &[]).unwrap();
        w.add(sec::AREA_CELLS, &area_cells).unwrap();
        w.add(sec::AREA_GRID, &area_grid).unwrap();
        w.add(sec::NAMES, &names.names).unwrap();
        w.add(sec::NAME_ALTS, &names.alts).unwrap();
        w.add_bytes(sec::STRINGS, &strings.into_bytes()).unwrap();
        w.finish(Meta {
            data_date: Some("2026-11-08T21:21:00Z".into()),
            settings,
            ..Default::default()
        })
        .unwrap();
        Index::open(&path).unwrap()
    }

    fn query(idx: &Index, north_m: i32, east: i32) -> Query {
        Query {
            pt: Point {
                lat: deg(43.0) + north_m * M,
                lon: deg(76.0) + east,
            },
            radius_m: DEFAULT_RADIUS_M,
            lang: idx.lang_index("en"),
            admin_only: false,
            street_rule: StreetRule::Hybrid,
        }
    }

    #[test]
    fn house_on_road_matches_by_translated_street_name() {
        let dir = tempfile::tempdir().unwrap();
        let idx = world(dir.path());
        // 5 m from the road, 10 m from house 10: the road names the street,
        // and house 10 is on it under its Russian name.
        let a = idx.lookup(&query(&idx, 5, 10_000));
        assert_eq!(a.kind, MatchKind::Address);
        assert_eq!(a.address.street, Some("Абай көшесі"));
        assert_eq!(a.address.house_number.as_deref(), Some("10"));
        assert_eq!(a.address.locality, Some("Almaty"));
        assert_eq!(a.address.country, Some("Kazakhstan"));
        assert_eq!(a.address.country_code.as_deref(), Some("KZ"));
        assert_eq!(
            a.source,
            Source {
                osm_type: Some("node"),
                osm_id: Some(1001),
                data_date: Some("2026-11-08T21:21:00Z"),
            }
        );
    }

    #[test]
    fn street_rule_decides_between_road_and_closer_house() {
        let dir = tempfile::tempdir().unwrap();
        let idx = world(dir.path());
        // 1 m from house 5 on another street, 3 m from the road.
        let mut q = query(&idx, -3, 15_000);
        let a = idx.lookup(&q);
        assert_eq!(a.kind, MatchKind::Address);
        assert_eq!(a.address.street, Some("Other"));
        assert_eq!(a.address.house_number.as_deref(), Some("5"));

        q.street_rule = StreetRule::Road;
        let a = idx.lookup(&q);
        assert_eq!(a.address.street, Some("Абай көшесі"));
        assert_eq!(a.address.house_number.as_deref(), Some("10"));
    }

    #[test]
    fn far_from_roads_falls_back_to_area_with_source() {
        let dir = tempfile::tempdir().unwrap();
        let idx = world(dir.path());
        let a = idx.lookup(&query(&idx, 1_500, 0));
        assert_eq!(a.kind, MatchKind::Place);
        assert_eq!(a.distance_m, Some(0.0));
        assert_eq!(a.address.street, None);
        assert_eq!(a.address.locality, Some("Almaty"));
        assert_eq!(a.address.suburb, None);
        assert_eq!(a.source.osm_type, Some("relation"));
        assert_eq!(a.source.osm_id, Some(2));
    }

    #[test]
    fn admin_only_skips_streets() {
        let dir = tempfile::tempdir().unwrap();
        let idx = world(dir.path());
        let mut q = query(&idx, 5, 10_000);
        q.admin_only = true;
        q.lang = None;
        let a = idx.lookup(&q);
        assert_eq!(a.kind, MatchKind::Admin);
        assert_eq!(a.address.street, None);
        assert_eq!(a.address.locality, Some("Алматы"));
        assert_eq!(a.source.osm_id, Some(2));
        assert_eq!(a.display, "Алматы, Қазақстан");
    }

    #[test]
    fn snap_reports_road_and_source() {
        let dir = tempfile::tempdir().unwrap();
        let idx = world(dir.path());
        let q = query(&idx, 20, 5_000);
        let s = idx.snap(q.pt, 50.0, q.lang, true).unwrap();
        assert!((s.distance_m - 20.0).abs() < 0.5, "{}", s.distance_m);
        assert_eq!(s.road.maxspeed_kmh, Some(60));
        assert_eq!(s.source.osm_type, Some("way"));
        assert_eq!(s.source.osm_id, Some(500));
        assert!(idx.snap(q.pt, 10.0, None, true).is_none());
    }
}
