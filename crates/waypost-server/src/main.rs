//! waypost-server: HTTP API over a memory-mapped waypost index.

mod compat;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use axum::extract::{Query, State, rejection::JsonRejection};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use waypost_core::geo::Point;
use waypost_core::lookup::{self, Answer, Index, MAX_RADIUS_M};

/// Most points one batch request may carry.
pub const MAX_BATCH: usize = 10_000;
/// Batches up to this size run on the request thread; larger ones in parallel.
const PARALLEL_FROM: usize = 256;

#[derive(Parser)]
#[command(version, about = "Serves reverse geocoding over a waypost index")]
struct Args {
    /// Index file built by waypost-builder.
    #[arg(long, env = "WAYPOST_INDEX")]
    index: std::path::PathBuf,
    /// Listen address.
    #[arg(long, env = "WAYPOST_LISTEN", default_value = "0.0.0.0:8080")]
    listen: String,
}

#[derive(Default)]
pub struct Metrics {
    reverse: AtomicU64,
    batch: AtomicU64,
    snap: AtomicU64,
    snap_batch: AtomicU64,
    batch_points: AtomicU64,
    bad_requests: AtomicU64,
    /// Lookup latency histogram, bucket upper bounds in microseconds.
    buckets: [AtomicU64; BUCKETS_US.len()],
    lookup_us_sum: AtomicU64,
    lookups: AtomicU64,
}

const BUCKETS_US: [u64; 8] = [5, 10, 25, 50, 100, 250, 1000, u64::MAX];

impl Metrics {
    fn observe(&self, us: u64) {
        let i = BUCKETS_US
            .iter()
            .position(|&b| us <= b)
            .unwrap_or(BUCKETS_US.len() - 1);
        self.buckets[i].fetch_add(1, Ordering::Relaxed);
        self.lookup_us_sum.fetch_add(us, Ordering::Relaxed);
        self.lookups.fetch_add(1, Ordering::Relaxed);
    }
}

pub struct AppState {
    pub index: OnceLock<Index>,
    pub metrics: Metrics,
}

type St = State<Arc<AppState>>;

#[derive(Deserialize)]
pub struct ReverseParams {
    lat: Option<f64>,
    lon: Option<f64>,
    radius: Option<f64>,
    lang: Option<String>,
    #[serde(default)]
    admin_only: bool,
    street: Option<String>,
}

#[derive(Deserialize)]
pub struct BatchPoint {
    lat: f64,
    lon: f64,
    #[serde(default)]
    id: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct BatchRequest {
    points: Vec<BatchPoint>,
    radius: Option<f64>,
    lang: Option<String>,
    #[serde(default)]
    admin_only: bool,
    street: Option<String>,
}

#[derive(Serialize)]
struct BatchItem<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a serde_json::Value>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    answer: Option<Answer<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
}

#[derive(Serialize)]
struct BatchResponse<'a> {
    results: Vec<BatchItem<'a>>,
}

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

fn not_ready() -> Response {
    error(StatusCode::SERVICE_UNAVAILABLE, "index is loading")
}

fn radius(r: Option<f64>) -> Result<f64, &'static str> {
    match r {
        None => Ok(lookup::DEFAULT_RADIUS_M),
        Some(r) if r.is_finite() && r > 0.0 && r <= MAX_RADIUS_M => Ok(r),
        Some(_) => Err("radius must be in (0, 1000] metres"),
    }
}

fn lang(idx: &Index, code: Option<&str>) -> Result<Option<u32>, String> {
    match code {
        None | Some("") => Ok(None),
        Some(c) => idx.lang_index(c).map(Some).ok_or_else(|| {
            format!(
                "lang {c} is not in this index; available: {:?}",
                idx.meta().settings.langs
            )
        }),
    }
}

fn street_rule(v: Option<&str>) -> Result<lookup::StreetRule, String> {
    match v {
        None | Some("") => Ok(lookup::StreetRule::default()),
        Some(v) => lookup::StreetRule::parse(v)
            .ok_or_else(|| format!("street must be hybrid or road, not {v}")),
    }
}

fn timed<'a>(m: &Metrics, idx: &'a Index, q: &lookup::Query) -> Answer<'a> {
    let t = Instant::now();
    let a = idx.lookup(q);
    m.observe(t.elapsed().as_micros() as u64);
    a
}

async fn reverse(State(st): St, Query(p): Query<ReverseParams>) -> Response {
    st.metrics.reverse.fetch_add(1, Ordering::Relaxed);
    let Some(idx) = st.index.get() else {
        return not_ready();
    };
    let bad = |m: String| {
        st.metrics.bad_requests.fetch_add(1, Ordering::Relaxed);
        error(StatusCode::BAD_REQUEST, m)
    };
    let (Some(lat), Some(lon)) = (p.lat, p.lon) else {
        return bad("lat and lon are required".into());
    };
    let Some(pt) = Point::from_deg(lat, lon) else {
        return bad("lat must be in [-90, 90] and lon in [-180, 180]".into());
    };
    let radius_m = match radius(p.radius) {
        Ok(r) => r,
        Err(e) => return bad(e.into()),
    };
    let lang = match lang(idx, p.lang.as_deref()) {
        Ok(l) => l,
        Err(e) => return bad(e),
    };
    let rule = match street_rule(p.street.as_deref()) {
        Ok(r) => r,
        Err(e) => return bad(e),
    };
    let q = lookup::Query {
        pt,
        radius_m,
        lang,
        admin_only: p.admin_only,
        street_rule: rule,
    };
    Json(timed(&st.metrics, idx, &q)).into_response()
}

async fn batch(State(st): St, body: Result<Json<BatchRequest>, JsonRejection>) -> Response {
    st.metrics.batch.fetch_add(1, Ordering::Relaxed);
    let bad = |m: String| {
        st.metrics.bad_requests.fetch_add(1, Ordering::Relaxed);
        error(StatusCode::BAD_REQUEST, m)
    };
    let req = match body {
        Ok(Json(r)) => r,
        Err(e) => return bad(e.body_text()),
    };
    let Some(idx) = st.index.get() else {
        return not_ready();
    };
    if req.points.len() > MAX_BATCH {
        return bad(format!("at most {MAX_BATCH} points per batch"));
    }
    let radius_m = match radius(req.radius) {
        Ok(r) => r,
        Err(e) => return bad(e.into()),
    };
    let lang = match lang(idx, req.lang.as_deref()) {
        Ok(l) => l,
        Err(e) => return bad(e),
    };
    let rule = match street_rule(req.street.as_deref()) {
        Ok(r) => r,
        Err(e) => return bad(e),
    };
    st.metrics
        .batch_points
        .fetch_add(req.points.len() as u64, Ordering::Relaxed);
    let st2 = st.clone();
    let run = move || -> Response {
        let idx = st2.index.get().expect("checked above");
        let one = |p: &BatchPoint| -> (Option<Answer<'_>>, Option<&'static str>) {
            match Point::from_deg(p.lat, p.lon) {
                Some(pt) => {
                    let q = lookup::Query {
                        pt,
                        radius_m,
                        lang,
                        admin_only: req.admin_only,
                        street_rule: rule,
                    };
                    (Some(timed(&st2.metrics, idx, &q)), None)
                }
                None => (
                    None,
                    Some("lat must be in [-90, 90] and lon in [-180, 180]"),
                ),
            }
        };
        let answers: Vec<_> = if req.points.len() >= PARALLEL_FROM {
            req.points.par_iter().map(one).collect()
        } else {
            req.points.iter().map(one).collect()
        };
        let results = req
            .points
            .iter()
            .zip(answers)
            .map(|(p, (answer, error))| BatchItem {
                id: p.id.as_ref(),
                answer,
                error,
            })
            .collect();
        Json(BatchResponse { results }).into_response()
    };
    // Batches run on the blocking pool, so a 10k batch never stalls the
    // async workers that serve single lookups.
    tokio::task::spawn_blocking(run)
        .await
        .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "batch failed"))
}

#[derive(Deserialize)]
pub struct SnapParams {
    lat: Option<f64>,
    lon: Option<f64>,
    radius: Option<f64>,
    lang: Option<String>,
    roads: Option<String>,
}

/// `roads=vehicle` (default) skips pedestrian ways; `roads=all` keeps them.
fn vehicle_only(v: Option<&str>) -> Result<bool, String> {
    match v {
        None | Some("vehicle") => Ok(true),
        Some("all") => Ok(false),
        Some(o) => Err(format!("roads must be vehicle or all, not {o}")),
    }
}

#[derive(Deserialize)]
pub struct SnapBatchRequest {
    points: Vec<BatchPoint>,
    radius: Option<f64>,
    lang: Option<String>,
    roads: Option<String>,
}

#[derive(Serialize)]
struct SnapItem<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a serde_json::Value>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    snap: Option<lookup::Snapped<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
}

const NO_ROAD: &str = "no road within radius";
const BAD_COORDS: &str = "lat must be in [-90, 90] and lon in [-180, 180]";

async fn snap(State(st): St, Query(p): Query<SnapParams>) -> Response {
    st.metrics.snap.fetch_add(1, Ordering::Relaxed);
    let Some(idx) = st.index.get() else {
        return not_ready();
    };
    let bad = |m: String| {
        st.metrics.bad_requests.fetch_add(1, Ordering::Relaxed);
        error(StatusCode::BAD_REQUEST, m)
    };
    let (Some(lat), Some(lon)) = (p.lat, p.lon) else {
        return bad("lat and lon are required".into());
    };
    let Some(pt) = Point::from_deg(lat, lon) else {
        return bad(BAD_COORDS.into());
    };
    let radius_m = match radius(p.radius) {
        Ok(r) => r,
        Err(e) => return bad(e.into()),
    };
    let lang = match lang(idx, p.lang.as_deref()) {
        Ok(l) => l,
        Err(e) => return bad(e),
    };
    let vehicle = match vehicle_only(p.roads.as_deref()) {
        Ok(v) => v,
        Err(e) => return bad(e),
    };
    let t = Instant::now();
    let r = idx.snap(pt, radius_m, lang, vehicle);
    st.metrics.observe(t.elapsed().as_micros() as u64);
    match r {
        Some(s) => Json(s).into_response(),
        None => error(StatusCode::NOT_FOUND, NO_ROAD),
    }
}

async fn snap_batch(
    State(st): St,
    body: Result<Json<SnapBatchRequest>, JsonRejection>,
) -> Response {
    st.metrics.snap_batch.fetch_add(1, Ordering::Relaxed);
    let bad = |m: String| {
        st.metrics.bad_requests.fetch_add(1, Ordering::Relaxed);
        error(StatusCode::BAD_REQUEST, m)
    };
    let req = match body {
        Ok(Json(r)) => r,
        Err(e) => return bad(e.body_text()),
    };
    let Some(idx) = st.index.get() else {
        return not_ready();
    };
    if req.points.len() > MAX_BATCH {
        return bad(format!("at most {MAX_BATCH} points per batch"));
    }
    let radius_m = match radius(req.radius) {
        Ok(r) => r,
        Err(e) => return bad(e.into()),
    };
    let lang = match lang(idx, req.lang.as_deref()) {
        Ok(l) => l,
        Err(e) => return bad(e),
    };
    let vehicle = match vehicle_only(req.roads.as_deref()) {
        Ok(v) => v,
        Err(e) => return bad(e),
    };
    st.metrics
        .batch_points
        .fetch_add(req.points.len() as u64, Ordering::Relaxed);
    let st2 = st.clone();
    let run = move || -> Response {
        let idx = st2.index.get().expect("checked above");
        let one = |p: &BatchPoint| -> (Option<lookup::Snapped<'_>>, Option<&'static str>) {
            match Point::from_deg(p.lat, p.lon) {
                Some(pt) => {
                    let t = Instant::now();
                    let r = idx.snap(pt, radius_m, lang, vehicle);
                    st2.metrics.observe(t.elapsed().as_micros() as u64);
                    match r {
                        Some(s) => (Some(s), None),
                        None => (None, Some(NO_ROAD)),
                    }
                }
                None => (None, Some(BAD_COORDS)),
            }
        };
        let answers: Vec<_> = if req.points.len() >= PARALLEL_FROM {
            req.points.par_iter().map(one).collect()
        } else {
            req.points.iter().map(one).collect()
        };
        let results: Vec<SnapItem> = req
            .points
            .iter()
            .zip(answers)
            .map(|(p, (snap, error))| SnapItem {
                id: p.id.as_ref(),
                snap,
                error,
            })
            .collect();
        Json(serde_json::json!({ "results": results })).into_response()
    };
    tokio::task::spawn_blocking(run)
        .await
        .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "batch failed"))
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(st): St) -> Response {
    match st.index.get() {
        Some(_) => "ready".into_response(),
        None => not_ready(),
    }
}

async fn info(State(st): St) -> Response {
    match st.index.get() {
        Some(idx) => Json(idx.meta().clone()).into_response(),
        None => not_ready(),
    }
}

async fn metrics(State(st): St) -> Response {
    use std::fmt::Write;
    let m = &st.metrics;
    let mut s = String::new();
    let c = |s: &mut String, name: &str, help: &str, v: u64| {
        let _ = writeln!(s, "# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}");
    };
    c(
        &mut s,
        "waypost_reverse_requests_total",
        "GET /v1/reverse requests.",
        m.reverse.load(Ordering::Relaxed),
    );
    c(
        &mut s,
        "waypost_batch_requests_total",
        "POST /v1/reverse/batch requests.",
        m.batch.load(Ordering::Relaxed),
    );
    c(
        &mut s,
        "waypost_snap_requests_total",
        "GET /v1/snap requests.",
        m.snap.load(Ordering::Relaxed),
    );
    c(
        &mut s,
        "waypost_snap_batch_requests_total",
        "POST /v1/snap/batch requests.",
        m.snap_batch.load(Ordering::Relaxed),
    );
    c(
        &mut s,
        "waypost_batch_points_total",
        "Points received in batches.",
        m.batch_points.load(Ordering::Relaxed),
    );
    c(
        &mut s,
        "waypost_bad_requests_total",
        "Requests rejected with 400.",
        m.bad_requests.load(Ordering::Relaxed),
    );
    let _ = writeln!(
        s,
        "# HELP waypost_lookup_seconds Time of one lookup.\n# TYPE waypost_lookup_seconds histogram"
    );
    let mut cum = 0;
    for (i, b) in BUCKETS_US.iter().enumerate() {
        cum += m.buckets[i].load(Ordering::Relaxed);
        let le = if *b == u64::MAX {
            "+Inf".to_string()
        } else {
            format!("{}", *b as f64 / 1e6)
        };
        let _ = writeln!(s, "waypost_lookup_seconds_bucket{{le=\"{le}\"}} {cum}");
    }
    let _ = writeln!(
        s,
        "waypost_lookup_seconds_sum {}",
        m.lookup_us_sum.load(Ordering::Relaxed) as f64 / 1e6
    );
    let _ = writeln!(
        s,
        "waypost_lookup_seconds_count {}",
        m.lookups.load(Ordering::Relaxed)
    );
    let _ = writeln!(
        s,
        "# HELP waypost_ready 1 when the index is loaded.\n# TYPE waypost_ready gauge\nwaypost_ready {}",
        st.index.get().is_some() as u8
    );
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], s).into_response()
}

pub fn app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/reverse", get(reverse))
        .route("/v1/reverse/batch", post(batch))
        .route("/reverse", get(compat::reverse))
        .route("/snap", get(compat::snap))
        .route("/v1/snap", get(snap))
        .route("/v1/snap/batch", post(snap_batch))
        .route("/v1/info", get(info))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        // 10k points of about 60 bytes each fit well within 2 MB.
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024))
        .with_state(state)
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let state = Arc::new(AppState {
        index: OnceLock::new(),
        metrics: Metrics::default(),
    });

    // Load in the background: /healthz answers at once, /readyz after warm-up.
    let st = state.clone();
    let path = args.index.clone();
    tokio::task::spawn_blocking(move || {
        let t = Instant::now();
        match Index::open(&path) {
            Ok(idx) => {
                idx.warm();
                let m = idx.meta();
                eprintln!(
                    "index {} loaded in {:.1}s: sources {:?}, data {:?}",
                    path.display(),
                    t.elapsed().as_secs_f64(),
                    m.sources,
                    m.data_date
                );
                let _ = st.index.set(idx);
            }
            Err(e) => {
                eprintln!("cannot open index {}: {e}", path.display());
                std::process::exit(1);
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(&args.listen)
        .await
        .unwrap_or_else(|e| {
            eprintln!("cannot listen on {}: {e}", args.listen);
            std::process::exit(1);
        });
    eprintln!("listening on {}", args.listen);
    axum::serve(listener, app(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server failed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn empty() -> Router {
        app(Arc::new(AppState {
            index: OnceLock::new(),
            metrics: Metrics::default(),
        }))
    }

    async fn status(r: Router, req: Request<Body>) -> StatusCode {
        r.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn not_ready_until_index_loads() {
        let get = |u: &str| Request::get(u).body(Body::empty()).unwrap();
        assert_eq!(status(empty(), get("/healthz")).await, StatusCode::OK);
        assert_eq!(
            status(empty(), get("/readyz")).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(empty(), get("/v1/reverse?lat=1&lon=1")).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(status(empty(), get("/metrics")).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn bad_batch_body_is_400() {
        let req = Request::post("/v1/reverse/batch")
            .header("content-type", "application/json")
            .body(Body::from("{\"points\": 5}"))
            .unwrap();
        assert_eq!(status(empty(), req).await, StatusCode::BAD_REQUEST);
    }
}
