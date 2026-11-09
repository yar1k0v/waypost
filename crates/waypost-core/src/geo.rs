//! Coordinates and distances. One coordinate model and one distance function
//! for the whole codebase.
//!
//! Coordinates are fixed-point `i32` in units of 1e-7 degrees. Distances are
//! metres, computed in a local equirectangular frame centred on the query
//! point. The frame error stays well under 0.5% within 5 km.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Fixed-point units per degree.
pub const SCALE: f64 = 1e7;

/// Mean Earth radius in metres (IUGG).
pub const EARTH_RADIUS_M: f64 = 6_371_008.8;

const UNITS_PER_TURN: i64 = 360 * 10_000_000;
const UNITS_PER_HALF_TURN: i64 = 180 * 10_000_000;

/// A WGS84 point in 1e-7 degree units.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct Point {
    pub lat: i32,
    pub lon: i32,
}

impl Point {
    /// Builds a point from degrees. Returns `None` for NaN, infinity or
    /// values outside lat [-90, 90] and lon [-180, 180].
    pub fn from_deg(lat: f64, lon: f64) -> Option<Point> {
        if !(lat.is_finite() && lon.is_finite()) {
            return None;
        }
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            return None;
        }
        Some(Point {
            lat: (lat * SCALE).round() as i32,
            lon: (lon * SCALE).round() as i32,
        })
    }

    pub fn lat_deg(self) -> f64 {
        self.lat as f64 / SCALE
    }

    pub fn lon_deg(self) -> f64 {
        self.lon as f64 / SCALE
    }
}

/// Signed longitude difference `b - a` in units, wrapped to [-180, 180) degrees,
/// so segments across the antimeridian stay short.
fn dlon_units(a: i32, b: i32) -> i64 {
    let mut d = b as i64 - a as i64;
    if d >= UNITS_PER_HALF_TURN {
        d -= UNITS_PER_TURN;
    } else if d < -UNITS_PER_HALF_TURN {
        d += UNITS_PER_TURN;
    }
    d
}

/// A local metric frame centred on one origin point. `xy` maps any point to
/// metres east (x) and north (y) of the origin.
#[derive(Clone, Copy, Debug)]
pub struct LocalFrame {
    origin: Point,
    /// Metres per unit of longitude at the origin latitude.
    kx: f64,
    /// Metres per unit of latitude.
    ky: f64,
}

impl LocalFrame {
    pub fn new(origin: Point) -> LocalFrame {
        let m_per_unit = EARTH_RADIUS_M * std::f64::consts::PI / 180.0 / SCALE;
        LocalFrame {
            origin,
            kx: m_per_unit * origin.lat_deg().to_radians().cos(),
            ky: m_per_unit,
        }
    }

    pub fn origin(&self) -> Point {
        self.origin
    }

    /// Metres east and north of the origin.
    pub fn xy(&self, p: Point) -> (f64, f64) {
        let dx = dlon_units(self.origin.lon, p.lon) as f64 * self.kx;
        let dy = (p.lat as i64 - self.origin.lat as i64) as f64 * self.ky;
        (dx, dy)
    }

    /// Distance in metres from the origin to `p`.
    pub fn dist(&self, p: Point) -> f64 {
        let (x, y) = self.xy(p);
        x.hypot(y)
    }

    /// Nearest point on segment `a`-`b` to the origin. Returns the distance in
    /// metres and the position `t` in [0, 1] along the segment.
    pub fn project(&self, a: Point, b: Point) -> Projection {
        let (ax, ay) = self.xy(a);
        let (bx, by) = self.xy(b);
        let (dx, dy) = (bx - ax, by - ay);
        let len_sq = dx * dx + dy * dy;
        let t = if len_sq == 0.0 {
            0.0
        } else {
            ((-ax * dx - ay * dy) / len_sq).clamp(0.0, 1.0)
        };
        let (px, py) = (ax + t * dx, ay + t * dy);
        Projection {
            dist: px.hypot(py),
            t,
            seg_len: len_sq.sqrt(),
            x: px,
            y: py,
        }
    }

    /// Point at metres (x, y) from the origin. Inverse of `xy`.
    pub fn point_at(&self, x: f64, y: f64) -> Point {
        let lat = self.origin.lat as f64 + y / self.ky;
        let mut lon = self.origin.lon as f64 + if self.kx > 0.0 { x / self.kx } else { 0.0 };
        if lon >= UNITS_PER_HALF_TURN as f64 {
            lon -= UNITS_PER_TURN as f64;
        } else if lon < -(UNITS_PER_HALF_TURN as f64) {
            lon += UNITS_PER_TURN as f64;
        }
        Point {
            lat: lat.round().clamp(-900_000_000.0, 900_000_000.0) as i32,
            lon: lon.round() as i32,
        }
    }
}

/// Result of projecting the frame origin onto a segment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Projection {
    /// Metres from the origin to the nearest point on the segment.
    pub dist: f64,
    /// Position of the nearest point along the segment, 0 = start, 1 = end.
    pub t: f64,
    /// Segment length in metres.
    pub seg_len: f64,
    /// The nearest point, in metres east and north of the origin.
    pub x: f64,
    pub y: f64,
}

/// Distance in metres between two points.
pub fn dist(a: Point, b: Point) -> f64 {
    LocalFrame::new(a).dist(b)
}

/// Length in metres of a polyline.
pub fn polyline_len(points: &[Point]) -> f64 {
    points.windows(2).map(|w| dist(w[0], w[1])).sum()
}

/// Great-circle distance in metres. Reference for tests and long distances.
pub fn haversine(a: Point, b: Point) -> f64 {
    let (la1, la2) = (a.lat_deg().to_radians(), b.lat_deg().to_radians());
    let dla = la2 - la1;
    let dlo = (dlon_units(a.lon, b.lon) as f64 / SCALE).to_radians();
    let h = (dla / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlo / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * h.sqrt().asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(lat: f64, lon: f64) -> Point {
        Point::from_deg(lat, lon).unwrap()
    }

    #[test]
    fn rejects_out_of_range_and_non_finite() {
        assert!(Point::from_deg(95.0, 0.0).is_none());
        assert!(Point::from_deg(0.0, 400.0).is_none());
        assert!(Point::from_deg(f64::NAN, 0.0).is_none());
        assert!(Point::from_deg(0.0, f64::INFINITY).is_none());
        assert!(Point::from_deg(90.0, -180.0).is_some());
    }

    #[test]
    fn distance_within_half_percent_of_haversine_up_to_5km() {
        for &lat in &[0.0, 43.25, 51.13, 60.0, 70.0] {
            let a = p(lat, 76.9);
            for &(dlat, dlon) in &[(0.0, 0.01), (0.01, 0.0), (0.03, 0.03), (-0.02, 0.05)] {
                let b = p(lat + dlat, 76.9 + dlon);
                let h = haversine(a, b);
                if h > 5000.0 {
                    continue;
                }
                let d = dist(a, b);
                assert!((d - h).abs() / h < 0.005, "lat {lat}: {d} vs {h}");
            }
        }
    }

    #[test]
    fn projection_is_metric_at_60_degrees() {
        // Diagonal segment at 60N. In raw degrees the nearest point differs
        // from the metric one, because a degree of longitude is half as long.
        let q = p(60.0, 30.0);
        let a = p(59.999, 29.998);
        let b = p(60.001, 30.002);
        let f = LocalFrame::new(q);
        let pr = f.project(a, b);
        // Brute force the true nearest point along the segment.
        let best = (0..=10_000)
            .map(|i| {
                let t = i as f64 / 10_000.0;
                let pt = Point {
                    lat: (a.lat as f64 + t * (b.lat - a.lat) as f64).round() as i32,
                    lon: (a.lon as f64 + t * (b.lon - a.lon) as f64).round() as i32,
                };
                haversine(q, pt)
            })
            .fold(f64::MAX, f64::min);
        assert!((pr.dist - best).abs() < 0.05, "{} vs {}", pr.dist, best);
    }

    #[test]
    fn projected_point_lies_on_segment() {
        let q = p(43.2385, 76.9452);
        let (a, b) = (p(43.2380, 76.9440), p(43.2390, 76.9470));
        let f = LocalFrame::new(q);
        let pr = f.project(a, b);
        let s = f.point_at(pr.x, pr.y);
        // The snapped point is on the segment (within rounding to 1e-7 deg)
        // and at the reported distance from the query.
        let on = LocalFrame::new(s).project(a, b);
        assert!(on.dist < 0.02, "{} m off the segment", on.dist);
        assert!((dist(q, s) - pr.dist).abs() < 0.02);
    }

    #[test]
    fn antimeridian_segment_is_short() {
        let a = p(65.0, 179.9999);
        let b = p(65.0, -179.9999);
        assert!(dist(a, b) < 20.0);
    }

    #[test]
    fn point_at_inverts_xy() {
        let f = LocalFrame::new(p(43.25, 76.95));
        let q = f.point_at(120.0, -80.0);
        let (x, y) = f.xy(q);
        assert!((x - 120.0).abs() < 0.02 && (y + 80.0).abs() < 0.02);
    }
}
