//! Lookup latency on one thread.
//!
//! Usage: cargo run --release -p waypost-core --example bench -- <index.wpi> <points.jsonl>
//! Points are JSON lines with `lat` and `lon`, for example from tools/golden_gen.py.

use std::io::BufRead;
use std::time::Instant;

use waypost_core::geo::Point;
use waypost_core::lookup::{DEFAULT_RADIUS_M, Index, Query, StreetRule};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, index, points] = args.as_slice() else {
        return Err("usage: bench <index.wpi> <points.jsonl>".into());
    };
    let idx = Index::open(index)?;
    idx.warm();
    let mut pts = Vec::new();
    for line in std::io::BufReader::new(std::fs::File::open(points)?).lines() {
        let v: serde_json::Value = serde_json::from_str(&line?)?;
        let (Some(lat), Some(lon)) = (v["lat"].as_f64(), v["lon"].as_f64()) else {
            continue;
        };
        pts.extend(Point::from_deg(lat, lon));
    }
    if pts.is_empty() {
        return Err("no points".into());
    }
    let query = |pt| Query {
        pt,
        radius_m: DEFAULT_RADIUS_M,
        lang: None,
        admin_only: false,
        street_rule: StreetRule::Hybrid,
    };
    // One pass to warm caches, one measured pass.
    for &pt in &pts {
        std::hint::black_box(idx.lookup(&query(pt)));
    }
    let mut us: Vec<f64> = Vec::with_capacity(pts.len());
    let total = Instant::now();
    for &pt in &pts {
        let t = Instant::now();
        std::hint::black_box(idx.lookup(&query(pt)));
        us.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let secs = total.elapsed().as_secs_f64();
    us.sort_by(f64::total_cmp);
    let pct = |p: f64| us[((us.len() - 1) as f64 * p).round() as usize];
    println!(
        "{} lookups: p50 {:.1} µs, p99 {:.1} µs, mean {:.1} µs, {:.0} lookups/s on one thread",
        us.len(),
        pct(0.5),
        pct(0.99),
        us.iter().sum::<f64>() / us.len() as f64,
        us.len() as f64 / secs
    );
    Ok(())
}
