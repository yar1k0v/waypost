//! Records stored in the index, and the helpers that build them.
//!
//! Every record is `#[repr(C)]` with explicit padding, so the builder writes
//! and the server reads the same bytes on every platform (little-endian).

use std::collections::HashMap;

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::geo::Point;

/// Marks an absent `NameId` or `StrRef`.
pub const NONE: u32 = u32::MAX;

/// Section names.
pub mod sec {
    pub const STRINGS: &str = "strings";
    pub const NAMES: &str = "names";
    pub const NAME_ALTS: &str = "name_alts";
    pub const NODES: &str = "nodes";
    pub const WAYS: &str = "ways";
    pub const PIECES: &str = "pieces";
    pub const PIECE_GRID: &str = "piece_grid";
    pub const PIECE_IDS: &str = "piece_ids";
    pub const ADDRS: &str = "addrs";
    pub const ADDR_GRID: &str = "addr_grid";
    pub const ADDR_IDS: &str = "addr_ids";
    pub const INTERPS: &str = "interps";
    pub const INTERP_GRID: &str = "interp_grid";
    pub const INTERP_IDS: &str = "interp_ids";
    pub const PLACES: &str = "places";
    pub const PLACE_GRID: &str = "place_grid";
    pub const PLACE_IDS: &str = "place_ids";
    pub const AREAS: &str = "areas";
    pub const AREA_POINTS: &str = "area_points";
    pub const AREA_EDGES: &str = "area_edges";
    pub const AREA_CELLS: &str = "area_cells";
    pub const AREA_GRID: &str = "area_grid";
}

/// Kind of an OSM object, stored as `u8` in records.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsmType {
    Node = 1,
    Way = 2,
    Relation = 3,
}

impl OsmType {
    pub fn from_u8(v: u8) -> Option<OsmType> {
        match v {
            1 => Some(OsmType::Node),
            2 => Some(OsmType::Way),
            3 => Some(OsmType::Relation),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            OsmType::Node => "node",
            OsmType::Way => "way",
            OsmType::Relation => "relation",
        }
    }
}

/// A UTF-8 string in the `strings` section.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct StrRef {
    pub off: u32,
    pub len: u32,
}

impl StrRef {
    pub const EMPTY: StrRef = StrRef { off: 0, len: 0 };
    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// A name with translations. Index into the `names` section.
pub type NameId = u32;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct NameRec {
    /// The `name` tag.
    pub default: StrRef,
    /// First translation in `name_alts`.
    pub alt_off: u32,
    pub alt_n: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct NameAlt {
    /// Index into `BuildSettings::langs`.
    pub lang: u32,
    pub s: StrRef,
}

/// Road class, from the OSM `highway` tag.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoadClass {
    Motorway = 1,
    Trunk = 2,
    Primary = 3,
    Secondary = 4,
    Tertiary = 5,
    Unclassified = 6,
    Residential = 7,
    LivingStreet = 8,
    Service = 9,
    Pedestrian = 10,
    Track = 11,
    Other = 12,
}

impl RoadClass {
    /// Classes kept as streets. Footways, paths, proposed, abandoned,
    /// construction and similar values return `None`.
    pub fn from_highway(v: &str) -> Option<RoadClass> {
        use RoadClass::*;
        Some(match v {
            "motorway" | "motorway_link" => Motorway,
            "trunk" | "trunk_link" => Trunk,
            "primary" | "primary_link" => Primary,
            "secondary" | "secondary_link" => Secondary,
            "tertiary" | "tertiary_link" => Tertiary,
            "unclassified" => Unclassified,
            "residential" => Residential,
            "living_street" => LivingStreet,
            "service" => Service,
            "pedestrian" => Pedestrian,
            "track" => Track,
            "road" | "busway" => Other,
            _ => return None,
        })
    }

    pub fn from_u8(v: u8) -> RoadClass {
        use RoadClass::*;
        match v {
            1 => Motorway,
            2 => Trunk,
            3 => Primary,
            4 => Secondary,
            5 => Tertiary,
            6 => Unclassified,
            7 => Residential,
            8 => LivingStreet,
            9 => Service,
            10 => Pedestrian,
            11 => Track,
            _ => Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        use RoadClass::*;
        match self {
            Motorway => "motorway",
            Trunk => "trunk",
            Primary => "primary",
            Secondary => "secondary",
            Tertiary => "tertiary",
            Unclassified => "unclassified",
            Residential => "residential",
            LivingStreet => "living_street",
            Service => "service",
            Pedestrian => "pedestrian",
            Track => "track",
            Other => "road",
        }
    }
}

/// A named street. Its geometry lives in pieces.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct Way {
    pub name: NameId,
    pub class: u8,
    pub _pad: [u8; 3],
    /// `maxspeed` in km/h, 0 when unknown.
    pub maxspeed: u16,
    pub _pad2: [u8; 6],
    pub osm_id: u64,
}

/// At most `PIECE_MAX_SEGMENTS` consecutive segments of one way. Pieces keep
/// nearest-street search local: a long way is never scanned whole.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct Piece {
    /// First node in `nodes`.
    pub node_off: u64,
    pub way: u32,
    /// Number of nodes, at least 2.
    pub n: u32,
}

pub const PIECE_MAX_SEGMENTS: usize = 16;

/// An address point: a node or a building centroid.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct Addr {
    pub pt: Point,
    pub housenumber: StrRef,
    pub street: StrRef,
    /// `addr:place`, used where addresses belong to a settlement, not a street.
    pub place: StrRef,
    pub postcode: StrRef,
    pub city: StrRef,
    pub osm_id: u64,
    /// `OsmType`: a node, or a building way.
    pub osm_type: u8,
    pub _pad: [u8; 7],
}

/// An `addr:interpolation` way.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct Interp {
    pub node_off: u64,
    pub n: u32,
    pub start: u32,
    pub end: u32,
    /// 1 for all numbers, 2 for odd or even.
    pub step: u32,
    pub street: StrRef,
    /// The `addr:interpolation` way.
    pub osm_id: u64,
}

/// Settlement kind of a `place=*` node or area.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PlaceKind {
    City = 1,
    Town = 2,
    Village = 3,
    Hamlet = 4,
    Suburb = 5,
    Neighbourhood = 6,
}

impl PlaceKind {
    pub fn from_tag(v: &str) -> Option<PlaceKind> {
        use PlaceKind::*;
        Some(match v {
            "city" => City,
            "town" => Town,
            "village" => Village,
            "hamlet" | "isolated_dwelling" => Hamlet,
            "suburb" | "quarter" | "borough" => Suburb,
            "neighbourhood" => Neighbourhood,
            _ => return None,
        })
    }

    pub fn from_u8(v: u8) -> Option<PlaceKind> {
        use PlaceKind::*;
        Some(match v {
            1 => City,
            2 => Town,
            3 => Village,
            4 => Hamlet,
            5 => Suburb,
            6 => Neighbourhood,
            _ => return None,
        })
    }

    /// Radius in metres within which a place node names its surroundings.
    pub fn radius_m(self) -> f64 {
        use PlaceKind::*;
        match self {
            City => 15_000.0,
            Town => 5_000.0,
            Village => 2_000.0,
            Hamlet => 800.0,
            Suburb => 1_500.0,
            Neighbourhood => 600.0,
        }
    }

    pub fn is_locality(self) -> bool {
        self <= PlaceKind::Hamlet
    }
}

/// The largest place-node radius. The place grid search uses it.
pub const PLACE_MAX_RADIUS_M: f64 = 15_000.0;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct Place {
    pub pt: Point,
    pub name: NameId,
    pub kind: u8,
    pub _pad: [u8; 3],
    /// The `place=*` node.
    pub osm_id: u64,
}

/// Kind of an area polygon.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AreaKind {
    /// `boundary=administrative` with `admin_level`.
    Admin = 1,
    /// `place=*` area; `level` holds the `PlaceKind`.
    Place = 2,
    /// `boundary=postal_code`; the name is the postcode.
    Postcode = 3,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct Area {
    pub name: NameId,
    pub kind: u8,
    /// `admin_level` for Admin, `PlaceKind` for Place, 0 for Postcode.
    pub level: u8,
    /// ISO 3166-1 alpha-2 code for countries, else zero bytes.
    pub country: [u8; 2],
    /// `postal_code` tag of an admin or place area, or the postcode itself.
    pub postcode: StrRef,
    /// Area in square kilometres. The smallest matching area of a field wins.
    pub km2: f64,
    pub osm_id: u64,
    /// `OsmType`: a closed way or a multipolygon relation.
    pub osm_type: u8,
    pub _pad: [u8; 7],
}

/// One area in one admin grid cell.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct AreaCell {
    pub area: u32,
    /// 1 if the cell centre lies inside the area.
    pub center_inside: u32,
    /// First edge in `area_edges`. Edges of this area that touch the cell.
    pub edge_off: u64,
    pub edge_n: u32,
    pub _pad: u32,
}

/// One cell of a uniform lat/lon grid. Points at a run of ids or records.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct GridCell {
    pub key: u64,
    pub off: u64,
    pub n: u32,
    pub _pad: u32,
}

/// Uniform grid geometry: cells of `size` units, rows from -90, columns from -180.
#[derive(Clone, Copy, Debug)]
pub struct GridSpec {
    pub size: i32,
}

impl GridSpec {
    pub fn row(&self, lat: i32) -> u32 {
        ((lat as i64 + 900_000_000) / self.size as i64) as u32
    }

    pub fn col(&self, lon: i32) -> u32 {
        ((lon as i64 + 1_800_000_000) / self.size as i64) as u32
    }

    pub fn key(row: u32, col: u32) -> u64 {
        ((row as u64) << 32) | col as u64
    }

    pub fn key_of(&self, p: Point) -> u64 {
        Self::key(self.row(p.lat), self.col(p.lon))
    }

    /// Number of columns around the globe.
    pub fn cols(&self) -> u32 {
        (3_600_000_000i64 / self.size as i64) as u32 + 1
    }

    /// South-west corner and north-east corner of a cell.
    pub fn bounds(&self, row: u32, col: u32) -> (Point, Point) {
        let s = self.size as i64;
        let lat0 = row as i64 * s - 900_000_000;
        let lon0 = col as i64 * s - 1_800_000_000;
        let clamp = |v: i64, m: i64| v.clamp(-m, m) as i32;
        (
            Point {
                lat: clamp(lat0, 900_000_000),
                lon: clamp(lon0, 1_800_000_000),
            },
            Point {
                lat: clamp(lat0 + s, 900_000_000),
                lon: clamp(lon0 + s, 1_800_000_000),
            },
        )
    }

    /// Rows and columns of every cell that a box touches.
    pub fn cells_in_box(&self, sw: Point, ne: Point) -> impl Iterator<Item = (u32, u32)> + use<> {
        let (r0, r1) = (self.row(sw.lat), self.row(ne.lat));
        let (c0, c1) = (self.col(sw.lon), self.col(ne.lon));
        (r0..=r1).flat_map(move |r| (c0..=c1).map(move |c| (r, c)))
    }
}

/// Deduplicating string table for the `strings` section.
#[derive(Default)]
pub struct StringTable {
    bytes: Vec<u8>,
    seen: HashMap<Box<str>, StrRef>,
}

impl StringTable {
    pub fn add(&mut self, s: &str) -> StrRef {
        if s.is_empty() {
            return StrRef::EMPTY;
        }
        if let Some(&r) = self.seen.get(s) {
            return r;
        }
        let off = u32::try_from(self.bytes.len()).expect("strings section exceeds 4 GiB");
        let len = u32::try_from(s.len()).expect("string exceeds 4 GiB");
        self.bytes.extend_from_slice(s.as_bytes());
        let r = StrRef { off, len };
        self.seen.insert(s.into(), r);
        r
    }

    pub fn add_opt(&mut self, s: Option<&str>) -> StrRef {
        s.map_or(StrRef::EMPTY, |s| self.add(s))
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// Default string (off, len) and translations (lang, off, len).
type NameKey = (u32, u32, Vec<(u32, u32, u32)>);

/// Builds `names` and `name_alts`, deduplicating identical name sets.
#[derive(Default)]
pub struct NameTable {
    pub names: Vec<NameRec>,
    pub alts: Vec<NameAlt>,
    seen: HashMap<NameKey, NameId>,
}

impl NameTable {
    /// `alts` holds (language index, string) pairs.
    pub fn add(&mut self, default: StrRef, alts: &[(u32, StrRef)]) -> NameId {
        let key = (
            default.off,
            default.len,
            alts.iter()
                .map(|(l, s)| (*l, s.off, s.len))
                .collect::<Vec<_>>(),
        );
        if let Some(&id) = self.seen.get(&key) {
            return id;
        }
        let id = u32::try_from(self.names.len()).expect("too many names");
        let alt_off = u32::try_from(self.alts.len()).expect("too many name translations");
        self.alts
            .extend(alts.iter().map(|&(lang, s)| NameAlt { lang, s }));
        self.names.push(NameRec {
            default,
            alt_off,
            alt_n: alts.len() as u32,
        });
        self.seen.insert(key, id);
        id
    }
}

/// Collects (cell key, id) pairs and writes a grid as cell table + id list.
#[derive(Default)]
pub struct GridBuilder {
    pairs: Vec<(u64, u32)>,
}

impl GridBuilder {
    pub fn insert(&mut self, key: u64, id: u32) {
        self.pairs.push((key, id));
    }

    pub fn finish(mut self) -> (Vec<GridCell>, Vec<u32>) {
        self.pairs.sort_unstable();
        self.pairs.dedup();
        let mut cells: Vec<GridCell> = Vec::new();
        let mut ids = Vec::with_capacity(self.pairs.len());
        for (key, id) in self.pairs {
            match cells.last_mut() {
                Some(c) if c.key == key => c.n += 1,
                _ => cells.push(GridCell {
                    key,
                    off: ids.len() as u64,
                    n: 1,
                    _pad: 0,
                }),
            }
            ids.push(id);
        }
        (cells, ids)
    }
}

/// Finds a cell in a sorted cell table.
pub fn find_cell(cells: &[GridCell], key: u64) -> Option<&GridCell> {
    cells
        .binary_search_by_key(&key, |c| c.key)
        .ok()
        .map(|i| &cells[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_sizes_are_stable() {
        assert_eq!(size_of::<Point>(), 8);
        assert_eq!(size_of::<StrRef>(), 8);
        assert_eq!(size_of::<NameRec>(), 16);
        assert_eq!(size_of::<NameAlt>(), 12);
        assert_eq!(size_of::<Way>(), 24);
        assert_eq!(size_of::<Piece>(), 16);
        assert_eq!(size_of::<Addr>(), 64);
        assert_eq!(size_of::<Interp>(), 40);
        assert_eq!(size_of::<Place>(), 24);
        assert_eq!(size_of::<Area>(), 40);
        assert_eq!(size_of::<AreaCell>(), 24);
        assert_eq!(size_of::<GridCell>(), 24);
    }

    #[test]
    fn grid_groups_ids_by_cell() {
        let mut g = GridBuilder::default();
        g.insert(5, 1);
        g.insert(2, 7);
        g.insert(5, 0);
        g.insert(5, 1);
        let (cells, ids) = g.finish();
        assert_eq!(cells.len(), 2);
        let c = find_cell(&cells, 5).unwrap();
        assert_eq!(&ids[c.off as usize..c.off as usize + c.n as usize], &[0, 1]);
        assert!(find_cell(&cells, 3).is_none());
    }

    #[test]
    fn grid_spec_bounds_contain_point() {
        let g = GridSpec { size: 50_000 };
        let p = Point::from_deg(43.2389, 76.8897).unwrap();
        let (sw, ne) = g.bounds(g.row(p.lat), g.col(p.lon));
        assert!(sw.lat <= p.lat && p.lat < ne.lat && sw.lon <= p.lon && p.lon < ne.lon);
    }

    #[test]
    fn excluded_highways() {
        for v in [
            "footway",
            "path",
            "proposed",
            "abandoned",
            "construction",
            "platform",
            "raceway",
        ] {
            assert!(RoadClass::from_highway(v).is_none(), "{v}");
        }
        assert_eq!(
            RoadClass::from_highway("residential"),
            Some(RoadClass::Residential)
        );
    }
}
