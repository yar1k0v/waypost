//! Exact point-in-polygon primitives on fixed-point coordinates.
//!
//! Polygons are planar in lon/lat, as OSM draws them. All predicates use
//! integer arithmetic, so the builder and the server always agree.
//!
//! The admin index stores, per grid cell, whether the cell centre is inside
//! an area and which of its edges touch the cell. A query counts crossings of
//! the segment from the centre to the query point with those edges: an odd
//! count flips the centre's answer. The segment stays inside the cell (cells
//! are convex), so every edge it can cross is in the cell's list.

use crate::geo::Point;

/// Sign of the cross product (b - a) x (c - a): >0 left turn, <0 right, 0 collinear.
#[inline]
pub fn orient(a: Point, b: Point, c: Point) -> i32 {
    let (ax, ay) = (a.lon as i128, a.lat as i128);
    let v =
        (b.lon as i128 - ax) * (c.lat as i128 - ay) - (b.lat as i128 - ay) * (c.lon as i128 - ax);
    v.signum() as i32
}

/// Whether segment `p`-`q` crosses edge `a`-`b`, counted so that a path of
/// edges is crossed a consistent number of times at shared vertices:
/// - the edge's endpoints are split by the line through p-q with a half-open
///   rule (strictly left versus not left), like the classic ray test;
/// - `p` and `q` lie strictly on opposite sides of the edge's line, or `q`
///   lies on it (a point on the boundary counts as crossed).
#[inline]
pub fn crosses(p: Point, q: Point, a: Point, b: Point) -> bool {
    let sa = orient(p, q, a) > 0;
    let sb = orient(p, q, b) > 0;
    if sa == sb {
        return false;
    }
    let op = orient(a, b, p);
    let oq = orient(a, b, q);
    (op > 0 && oq <= 0) || (op < 0 && oq >= 0) || (op == 0 && oq != 0)
}

/// Even-odd point-in-polygon over all rings (outer rings and holes), with a
/// horizontal ray. Used by the builder on whole rings.
pub fn rings_contain(rings: &[Vec<Point>], p: Point) -> bool {
    let mut inside = false;
    for ring in rings {
        for w in ring.windows(2) {
            let (a, b) = (w[0], w[1]);
            if (a.lat > p.lat) != (b.lat > p.lat) {
                // x of the edge at p.lat, compared exactly:
                // p.lon < a.lon + (p.lat - a.lat) * (b.lon - a.lon) / (b.lat - a.lat)
                let lhs = (p.lon as i128 - a.lon as i128) * (b.lat as i128 - a.lat as i128);
                let rhs = (p.lat as i128 - a.lat as i128) * (b.lon as i128 - a.lon as i128);
                let left = if b.lat > a.lat { lhs < rhs } else { lhs > rhs };
                if left {
                    inside = !inside;
                }
            }
        }
    }
    inside
}

/// Whether segment `a`-`b` touches the closed box `sw`-`ne`.
pub fn segment_touches_box(a: Point, b: Point, sw: Point, ne: Point) -> bool {
    let (x0, y0) = (a.lon as f64, a.lat as f64);
    let (dx, dy) = (b.lon as f64 - x0, b.lat as f64 - y0);
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    let checks = [
        (-dx, x0 - sw.lon as f64),
        (dx, ne.lon as f64 - x0),
        (-dy, y0 - sw.lat as f64),
        (dy, ne.lat as f64 - y0),
    ];
    for (p, q) in checks {
        if p == 0.0 {
            if q < 0.0 {
                return false;
            }
        } else {
            let r = q / p;
            if p < 0.0 {
                t0 = t0.max(r);
            } else {
                t1 = t1.min(r);
            }
            if t0 > t1 {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(lon: i32, lat: i32) -> Point {
        Point { lat, lon }
    }

    fn square(x0: i32, y0: i32, s: i32) -> Vec<Point> {
        vec![
            pt(x0, y0),
            pt(x0 + s, y0),
            pt(x0 + s, y0 + s),
            pt(x0, y0 + s),
            pt(x0, y0),
        ]
    }

    #[test]
    fn hole_is_outside() {
        let rings = vec![square(0, 0, 100), square(40, 40, 20)];
        assert!(rings_contain(&rings, pt(10, 10)));
        assert!(!rings_contain(&rings, pt(50, 50)));
        assert!(!rings_contain(&rings, pt(150, 50)));
    }

    /// Crossing parity from a reference point must agree with the full test,
    /// including queries through vertices and along collinear lines.
    #[test]
    fn crossing_parity_matches_full_test() {
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = |m: i32| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % m as u64) as i32
        };
        for _ in 0..200 {
            // Random star-ish polygon on a small integer grid, so many
            // vertices and edges are collinear with queries.
            let n = 3 + rnd(10) as usize;
            let mut ring: Vec<Point> = (0..n)
                .map(|i| {
                    let ang = i as f64 / n as f64 * std::f64::consts::TAU;
                    let r = 5.0 + rnd(10) as f64;
                    pt(
                        20 + (r * ang.cos()).round() as i32,
                        20 + (r * ang.sin()).round() as i32,
                    )
                })
                .collect();
            ring.push(ring[0]);
            let rings = vec![ring.clone()];
            let c = pt(rnd(40), rnd(40));
            let c_in = rings_contain(&rings, c);
            for _ in 0..200 {
                let q = pt(rnd(40), rnd(40));
                let on_boundary = ring.windows(2).any(|w| {
                    orient(w[0], w[1], q) == 0
                        && q.lon >= w[0].lon.min(w[1].lon)
                        && q.lon <= w[0].lon.max(w[1].lon)
                        && q.lat >= w[0].lat.min(w[1].lat)
                        && q.lat <= w[0].lat.max(w[1].lat)
                });
                let c_on_boundary = ring.windows(2).any(|w| {
                    orient(w[0], w[1], c) == 0
                        && c.lon >= w[0].lon.min(w[1].lon)
                        && c.lon <= w[0].lon.max(w[1].lon)
                        && c.lat >= w[0].lat.min(w[1].lat)
                        && c.lat <= w[0].lat.max(w[1].lat)
                });
                if on_boundary || c_on_boundary || q == c {
                    continue;
                }
                let n = ring
                    .windows(2)
                    .filter(|w| crosses(c, q, w[0], w[1]))
                    .count();
                let got = c_in ^ (n % 2 == 1);
                assert_eq!(
                    got,
                    rings_contain(&rings, q),
                    "c {c:?} q {q:?} ring {ring:?}"
                );
            }
        }
    }

    #[test]
    fn segment_box() {
        let (sw, ne) = (pt(0, 0), pt(10, 10));
        assert!(segment_touches_box(pt(-5, 5), pt(15, 5), sw, ne));
        assert!(!segment_touches_box(pt(-5, 20), pt(20, 11), sw, ne));
        assert!(segment_touches_box(pt(-1, 11), pt(11, -1), sw, ne));
        assert!(segment_touches_box(pt(3, 3), pt(4, 4), sw, ne));
    }
}
