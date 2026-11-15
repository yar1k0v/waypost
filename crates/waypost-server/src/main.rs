//! waypost-server: HTTP API over a memory-mapped waypost index.

// Handlers return a ready 4xx/5xx `Response` as their error.
#![allow(clippy::result_large_err)]

mod compat;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use waypost_core::geo::Point;
use waypost_core::lookup::{
    self, Answer, DEFAULT_BEARING_TOL_DEG, Heading, Index, MAX_RADIUS_M, Snapped,
};

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
    /// Reverse lookup latency histogram, bucket upper bounds in microseconds.
    reverse_buckets: [AtomicU64; BUCKETS_US.len()],
    reverse_us_sum: AtomicU64,
    reverse_count: AtomicU64,
    /// Snap lookup latency histogram, bucket upper bounds in microseconds.
    snap_buckets: [AtomicU64; BUCKETS_US.len()],
    snap_us_sum: AtomicU64,
    snap_count: AtomicU64,
}

const BUCKETS_US: [u64; 8] = [5, 10, 25, 50, 100, 250, 1000, u64::MAX];

/// Which handler a latency sample belongs to.
#[derive(Clone, Copy)]
enum OpKind {
    Reverse,
    Snap,
}

impl Metrics {
    fn observe(&self, kind: OpKind, us: u64) {
        let i = BUCKETS_US
            .iter()
            .position(|&b| us <= b)
            .unwrap_or(BUCKETS_US.len() - 1);
        let (buckets, sum, count) = match kind {
            OpKind::Reverse => (
                &self.reverse_buckets,
                &self.reverse_us_sum,
                &self.reverse_count,
            ),
            OpKind::Snap => (&self.snap_buckets, &self.snap_us_sum, &self.snap_count),
        };
        buckets[i].fetch_add(1, Ordering::Relaxed);
        sum.fetch_add(us, Ordering::Relaxed);
        count.fetch_add(1, Ordering::Relaxed);
    }
}

pub struct AppState {
    pub index: OnceLock<Index>,
    pub metrics: Metrics,
}

type St = State<Arc<AppState>>;

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

fn not_ready() -> Response {
    error(StatusCode::SERVICE_UNAVAILABLE, "index is loading")
}

/// Counts a rejected request and answers 400 with a JSON error body.
fn bad(st: &AppState, msg: impl Into<String>) -> Response {
    st.metrics.bad_requests.fetch_add(1, Ordering::Relaxed);
    error(StatusCode::BAD_REQUEST, msg)
}

/// The query parameters, or a JSON 400 when the query string does not parse.
fn query<P>(st: &AppState, q: Result<Query<P>, QueryRejection>) -> Result<P, Response> {
    q.map(|Query(p)| p).map_err(|e| bad(st, e.body_text()))
}

/// The JSON body, or a JSON 400 when it does not parse.
fn body<B>(st: &AppState, b: Result<Json<B>, JsonRejection>) -> Result<B, Response> {
    b.map(|Json(b)| b).map_err(|e| bad(st, e.body_text()))
}

/// The loaded index, or 503 while it loads.
fn index(st: &AppState) -> Result<&Index, Response> {
    st.index.get().ok_or_else(not_ready)
}

pub const BAD_COORDS: &str = "lat must be in [-90, 90] and lon in [-180, 180]";
const BAD_BEARING: &str = "bearing must be in [0, 360)";
const BAD_RADIUS: &str = "radius must be in (0, 1000] metres";
const NO_ROAD: &str = "no road within radius";

fn point(lat: Option<f64>, lon: Option<f64>) -> Result<Point, String> {
    let (Some(lat), Some(lon)) = (lat, lon) else {
        return Err("lat and lon are required".into());
    };
    Point::from_deg(lat, lon).ok_or_else(|| BAD_COORDS.into())
}

fn radius(r: Option<f64>) -> Result<f64, String> {
    match r {
        None => Ok(lookup::DEFAULT_RADIUS_M),
        Some(r) if r.is_finite() && r > 0.0 && r <= MAX_RADIUS_M => Ok(r),
        Some(_) => Err(BAD_RADIUS.into()),
    }
}

/// Language index for the first code of a `,`-separated list that the index
/// knows, so `lang=de,fr` works when only one of them was built in.
fn lang(idx: &Index, code: Option<&str>) -> Result<Option<u32>, String> {
    match code {
        None | Some("") => Ok(None),
        Some(c) => c
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .find_map(|p| idx.lang_index(p))
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "lang {c} is not in this index; available: {:?}",
                    idx.meta().settings.langs
                )
            }),
    }
}

fn house_match(v: Option<&str>) -> Result<lookup::HouseMatch, String> {
    match v {
        None | Some("") => Ok(lookup::HouseMatch::default()),
        Some(v) => lookup::HouseMatch::parse(v)
            .ok_or_else(|| format!("house must be exact or nearest, not {v}")),
    }
}

fn street_rule(v: Option<&str>) -> Result<lookup::StreetRule, String> {
    match v {
        None | Some("") => Ok(lookup::StreetRule::default()),
        Some(v) => lookup::StreetRule::parse(v)
            .ok_or_else(|| format!("street must be hybrid or road, not {v}")),
    }
}

/// `roads=vehicle` (default) skips pedestrian ways; `roads=all` keeps them.
fn vehicle_only(v: Option<&str>) -> Result<bool, String> {
    match v {
        None | Some("vehicle") => Ok(true),
        Some("all") => Ok(false),
        Some(o) => Err(format!("roads must be vehicle or all, not {o}")),
    }
}

fn bearing_tol(t: Option<f64>) -> Result<f64, String> {
    match t {
        None => Ok(DEFAULT_BEARING_TOL_DEG),
        Some(t) if t > 0.0 && t <= 180.0 => Ok(t),
        Some(_) => Err("bearing_tol must be in (0, 180] degrees".into()),
    }
}

fn heading(bearing: Option<f64>, tol_deg: f64) -> Result<Option<Heading>, &'static str> {
    match bearing {
        None => Ok(None),
        Some(b) if (0.0..360.0).contains(&b) => Ok(Some(Heading {
            bearing_deg: b,
            tol_deg,
        })),
        Some(_) => Err(BAD_BEARING),
    }
}

fn batch_size(n: usize) -> Result<(), String> {
    if n > MAX_BATCH {
        return Err(format!("at most {MAX_BATCH} points per batch"));
    }
    Ok(())
}

/// Runs one lookup or snap and records its latency.
fn timed<T>(m: &Metrics, kind: OpKind, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let r = f();
    m.observe(kind, t.elapsed().as_micros() as u64);
    r
}

#[derive(Deserialize)]
pub struct ReverseParams {
    lat: Option<f64>,
    lon: Option<f64>,
    radius: Option<f64>,
    lang: Option<String>,
    #[serde(default)]
    admin_only: bool,
    street: Option<String>,
    house: Option<String>,
}

#[derive(Deserialize)]
pub struct SnapParams {
    lat: Option<f64>,
    lon: Option<f64>,
    radius: Option<f64>,
    lang: Option<String>,
    roads: Option<String>,
    bearing: Option<f64>,
    bearing_tol: Option<f64>,
}

#[derive(Deserialize)]
pub struct BatchPoint {
    lat: f64,
    lon: f64,
    #[serde(default)]
    id: Option<serde_json::Value>,
    /// Snap batches only: the vehicle heading at this point.
    #[serde(default)]
    bearing: Option<f64>,
    /// Search radius for this point, metres. Overrides the batch radius.
    #[serde(default)]
    radius: Option<f64>,
}

#[derive(Deserialize)]
pub struct BatchRequest {
    points: Vec<BatchPoint>,
    radius: Option<f64>,
    lang: Option<String>,
    #[serde(default)]
    admin_only: bool,
    street: Option<String>,
    house: Option<String>,
}

#[derive(Deserialize)]
pub struct SnapBatchRequest {
    points: Vec<BatchPoint>,
    radius: Option<f64>,
    lang: Option<String>,
    roads: Option<String>,
    bearing_tol: Option<f64>,
}

/// One kind of per-point work in a batch.
trait BatchOp: Send + Sync + 'static {
    type Out<'a>: Serialize + Send;
    fn kind(&self) -> OpKind;
    fn run<'a>(
        &self,
        idx: &'a Index,
        pt: Point,
        bearing: Option<f64>,
        radius: Option<f64>,
    ) -> Result<Self::Out<'a>, &'static str>;
}

/// A reverse lookup; `pt` is replaced per point.
struct ReverseOp(lookup::Query);

impl BatchOp for ReverseOp {
    type Out<'a> = Answer<'a>;

    fn kind(&self) -> OpKind {
        OpKind::Reverse
    }

    fn run<'a>(
        &self,
        idx: &'a Index,
        pt: Point,
        _: Option<f64>,
        radius: Option<f64>,
    ) -> Result<Answer<'a>, &'static str> {
        let q = lookup::Query {
            pt,
            radius_m: radius.unwrap_or(self.0.radius_m),
            ..self.0
        };
        Ok(idx.lookup(&q))
    }
}

struct SnapOp {
    radius_m: f64,
    lang: Option<u32>,
    vehicle: bool,
    bearing_tol: f64,
}

impl BatchOp for SnapOp {
    type Out<'a> = Snapped<'a>;

    fn kind(&self) -> OpKind {
        OpKind::Snap
    }

    fn run<'a>(
        &self,
        idx: &'a Index,
        pt: Point,
        bearing: Option<f64>,
        radius: Option<f64>,
    ) -> Result<Snapped<'a>, &'static str> {
        let h = heading(bearing, self.bearing_tol)?;
        idx.snap(
            pt,
            radius.unwrap_or(self.radius_m),
            self.lang,
            self.vehicle,
            h,
        )
        .ok_or(NO_ROAD)
    }
}

#[derive(Serialize)]
struct BatchItem<'a, T> {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a serde_json::Value>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    out: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
}

#[derive(Serialize)]
struct BatchResponse<T> {
    results: Vec<T>,
}

/// Runs a batch on the blocking pool, so a 10k batch never stalls the async
/// workers that serve single lookups. Large batches run in parallel.
async fn run_batch<O: BatchOp>(st: Arc<AppState>, points: Vec<BatchPoint>, op: O) -> Response {
    st.metrics
        .batch_points
        .fetch_add(points.len() as u64, Ordering::Relaxed);
    let run = move || -> Response {
        let Some(idx) = st.index.get() else {
            return not_ready();
        };
        let one = |p: &BatchPoint| {
            let Some(pt) = Point::from_deg(p.lat, p.lon) else {
                return Err(BAD_COORDS);
            };
            if p.radius
                .is_some_and(|r| !(r.is_finite() && r > 0.0 && r <= MAX_RADIUS_M))
            {
                return Err(BAD_RADIUS);
            }
            Ok(timed(&st.metrics, op.kind(), || {
                op.run(idx, pt, p.bearing, p.radius)
            }))
        };
        let outs: Vec<_> = if points.len() >= PARALLEL_FROM {
            points.par_iter().map(one).collect()
        } else {
            points.iter().map(one).collect()
        };
        let results = points
            .iter()
            .zip(outs)
            .map(|(p, r)| {
                let (out, error) = match r {
                    Ok(o) => (Some(o), None),
                    Err(e) => (None, Some(e)),
                };
                BatchItem {
                    id: p.id.as_ref(),
                    out,
                    error,
                }
            })
            .collect();
        Json(BatchResponse { results }).into_response()
    };
    tokio::task::spawn_blocking(run)
        .await
        .unwrap_or_else(|_| error(StatusCode::INTERNAL_SERVER_ERROR, "batch failed"))
}

async fn reverse(State(st): St, q: Result<Query<ReverseParams>, QueryRejection>) -> Response {
    st.metrics.reverse.fetch_add(1, Ordering::Relaxed);
    let run = || -> Result<Response, Response> {
        let p = query(&st, q)?;
        let bad = |e: String| bad(&st, e);
        // Everything that needs no index is checked before the ready check.
        let pt = point(p.lat, p.lon).map_err(bad)?;
        let radius_m = radius(p.radius).map_err(bad)?;
        let rule = street_rule(p.street.as_deref()).map_err(bad)?;
        let house = house_match(p.house.as_deref()).map_err(bad)?;
        let idx = index(&st)?;
        let q = lookup::Query {
            pt,
            radius_m,
            lang: lang(idx, p.lang.as_deref()).map_err(bad)?,
            admin_only: p.admin_only,
            street_rule: rule,
            house_match: house,
        };
        Ok(Json(timed(&st.metrics, OpKind::Reverse, || idx.lookup(&q))).into_response())
    };
    run().unwrap_or_else(|e| e)
}

async fn batch(State(st): St, b: Result<Json<BatchRequest>, JsonRejection>) -> Response {
    st.metrics.batch.fetch_add(1, Ordering::Relaxed);
    let parse = || -> Result<(Vec<BatchPoint>, ReverseOp), Response> {
        let req = body(&st, b)?;
        let bad = |e: String| bad(&st, e);
        // Everything that needs no index is checked before the ready check.
        batch_size(req.points.len()).map_err(bad)?;
        let radius_m = radius(req.radius).map_err(bad)?;
        let rule = street_rule(req.street.as_deref()).map_err(bad)?;
        let house = house_match(req.house.as_deref()).map_err(bad)?;
        let idx = index(&st)?;
        let q = lookup::Query {
            pt: Point { lat: 0, lon: 0 },
            radius_m,
            lang: lang(idx, req.lang.as_deref()).map_err(bad)?,
            admin_only: req.admin_only,
            street_rule: rule,
            house_match: house,
        };
        Ok((req.points, ReverseOp(q)))
    };
    match parse() {
        Ok((points, op)) => run_batch(st, points, op).await,
        Err(r) => r,
    }
}

async fn snap(State(st): St, q: Result<Query<SnapParams>, QueryRejection>) -> Response {
    st.metrics.snap.fetch_add(1, Ordering::Relaxed);
    let run = || -> Result<Response, Response> {
        let p = query(&st, q)?;
        let bad = |e: String| bad(&st, e);
        // Everything that needs no index is checked before the ready check.
        let pt = point(p.lat, p.lon).map_err(bad)?;
        let tol = bearing_tol(p.bearing_tol).map_err(bad)?;
        heading(p.bearing, tol).map_err(|e| bad(e.into()))?;
        let radius_m = radius(p.radius).map_err(bad)?;
        let vehicle = vehicle_only(p.roads.as_deref()).map_err(bad)?;
        let idx = index(&st)?;
        let op = SnapOp {
            radius_m,
            lang: lang(idx, p.lang.as_deref()).map_err(bad)?,
            vehicle,
            bearing_tol: tol,
        };
        Ok(
            match timed(&st.metrics, OpKind::Snap, || {
                op.run(idx, pt, p.bearing, None)
            }) {
                Ok(s) => Json(s).into_response(),
                Err(e) => error(StatusCode::NOT_FOUND, e),
            },
        )
    };
    run().unwrap_or_else(|e| e)
}

async fn snap_batch(State(st): St, b: Result<Json<SnapBatchRequest>, JsonRejection>) -> Response {
    st.metrics.snap_batch.fetch_add(1, Ordering::Relaxed);
    let parse = || -> Result<(Vec<BatchPoint>, SnapOp), Response> {
        let req = body(&st, b)?;
        let bad = |e: String| bad(&st, e);
        // Everything that needs no index is checked before the ready check.
        batch_size(req.points.len()).map_err(bad)?;
        let radius_m = radius(req.radius).map_err(bad)?;
        let vehicle = vehicle_only(req.roads.as_deref()).map_err(bad)?;
        let tol = bearing_tol(req.bearing_tol).map_err(bad)?;
        let idx = index(&st)?;
        let op = SnapOp {
            radius_m,
            lang: lang(idx, req.lang.as_deref()).map_err(bad)?,
            vehicle,
            bearing_tol: tol,
        };
        Ok((req.points, op))
    };
    match parse() {
        Ok((points, op)) => run_batch(st, points, op).await,
        Err(r) => r,
    }
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

/// Writes one Prometheus histogram in seconds, from microseconds.
fn write_histogram(
    s: &mut String,
    name: &str,
    help: &str,
    buckets: &[AtomicU64],
    sum_us: &AtomicU64,
    count: &AtomicU64,
) {
    use std::fmt::Write;
    let _ = writeln!(s, "# HELP {name} {help}\n# TYPE {name} histogram");
    let mut cum = 0;
    for (i, b) in BUCKETS_US.iter().enumerate() {
        cum += buckets[i].load(Ordering::Relaxed);
        let le = if *b == u64::MAX {
            "+Inf".to_string()
        } else {
            format!("{}", *b as f64 / 1e6)
        };
        let _ = writeln!(s, "{name}_bucket{{le=\"{le}\"}} {cum}");
    }
    let _ = writeln!(
        s,
        "{name}_sum {}",
        sum_us.load(Ordering::Relaxed) as f64 / 1e6
    );
    let _ = writeln!(s, "{name}_count {}", count.load(Ordering::Relaxed));
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
    write_histogram(
        &mut s,
        "waypost_reverse_seconds",
        "Reverse lookup latency.",
        &m.reverse_buckets,
        &m.reverse_us_sum,
        &m.reverse_count,
    );
    write_histogram(
        &mut s,
        "waypost_snap_seconds",
        "Snap lookup latency.",
        &m.snap_buckets,
        &m.snap_us_sum,
        &m.snap_count,
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
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server failed");
}

/// Resolves on Ctrl+C or SIGTERM. The server runs as PID 1 in its container,
/// where SIGTERM (`docker stop`, a Kubernetes rollout) is ignored unless handled.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
    eprintln!("shutting down");
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
    async fn bad_query_string_is_json_400() {
        for u in [
            "/v1/reverse?lat=abc&lon=1",
            "/v1/snap?lat=1&lon=1&radius=x",
            "/reverse?lat=1&lon=1&distance=x",
            "/v1/reverse?lat=1&lon=1&admin_only=maybe",
            "/v1/snap?lat=1&lon=1&bearing=400",
            "/v1/snap?lat=1&lon=1&bearing=90&bearing_tol=0",
        ] {
            let res = empty()
                .oneshot(Request::get(u).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{u}");
            let b = http_body_util::BodyExt::collect(res.into_body())
                .await
                .unwrap()
                .to_bytes();
            let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
            assert!(v["error"].is_string(), "{u}: {v}");
        }
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
