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
use waypost_core::lookup::{self, Answer, Index, MAX_RADIUS_M, Snapped};

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
        Some(_) => Err("radius must be in (0, 1000] metres".into()),
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

/// `roads=vehicle` (default) skips pedestrian ways; `roads=all` keeps them.
fn vehicle_only(v: Option<&str>) -> Result<bool, String> {
    match v {
        None | Some("vehicle") => Ok(true),
        Some("all") => Ok(false),
        Some(o) => Err(format!("roads must be vehicle or all, not {o}")),
    }
}

fn batch_size(n: usize) -> Result<(), String> {
    if n > MAX_BATCH {
        return Err(format!("at most {MAX_BATCH} points per batch"));
    }
    Ok(())
}

/// Runs one lookup or snap and records its latency.
fn timed<T>(m: &Metrics, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let r = f();
    m.observe(t.elapsed().as_micros() as u64);
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
}

#[derive(Deserialize)]
pub struct SnapParams {
    lat: Option<f64>,
    lon: Option<f64>,
    radius: Option<f64>,
    lang: Option<String>,
    roads: Option<String>,
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

#[derive(Deserialize)]
pub struct SnapBatchRequest {
    points: Vec<BatchPoint>,
    radius: Option<f64>,
    lang: Option<String>,
    roads: Option<String>,
}

/// One kind of per-point work in a batch.
trait BatchOp: Send + Sync + 'static {
    type Out<'a>: Serialize + Send;
    fn run<'a>(&self, idx: &'a Index, pt: Point) -> Result<Self::Out<'a>, &'static str>;
}

/// A reverse lookup; `pt` is replaced per point.
struct ReverseOp(lookup::Query);

impl BatchOp for ReverseOp {
    type Out<'a> = Answer<'a>;
    fn run<'a>(&self, idx: &'a Index, pt: Point) -> Result<Answer<'a>, &'static str> {
        Ok(idx.lookup(&lookup::Query { pt, ..self.0 }))
    }
}

struct SnapOp {
    radius_m: f64,
    lang: Option<u32>,
    vehicle: bool,
}

impl BatchOp for SnapOp {
    type Out<'a> = Snapped<'a>;
    fn run<'a>(&self, idx: &'a Index, pt: Point) -> Result<Snapped<'a>, &'static str> {
        idx.snap(pt, self.radius_m, self.lang, self.vehicle)
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
        let one = |p: &BatchPoint| match Point::from_deg(p.lat, p.lon) {
            Some(pt) => timed(&st.metrics, || op.run(idx, pt)),
            None => Err(BAD_COORDS),
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
        let idx = index(&st)?;
        let bad = |e: String| bad(&st, e);
        let q = lookup::Query {
            pt: point(p.lat, p.lon).map_err(bad)?,
            radius_m: radius(p.radius).map_err(bad)?,
            lang: lang(idx, p.lang.as_deref()).map_err(bad)?,
            admin_only: p.admin_only,
            street_rule: street_rule(p.street.as_deref()).map_err(bad)?,
        };
        Ok(Json(timed(&st.metrics, || idx.lookup(&q))).into_response())
    };
    run().unwrap_or_else(|e| e)
}

async fn batch(State(st): St, b: Result<Json<BatchRequest>, JsonRejection>) -> Response {
    st.metrics.batch.fetch_add(1, Ordering::Relaxed);
    let parse = || -> Result<(Vec<BatchPoint>, ReverseOp), Response> {
        let req = body(&st, b)?;
        let idx = index(&st)?;
        let bad = |e: String| bad(&st, e);
        batch_size(req.points.len()).map_err(bad)?;
        let q = lookup::Query {
            pt: Point { lat: 0, lon: 0 },
            radius_m: radius(req.radius).map_err(bad)?,
            lang: lang(idx, req.lang.as_deref()).map_err(bad)?,
            admin_only: req.admin_only,
            street_rule: street_rule(req.street.as_deref()).map_err(bad)?,
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
        let idx = index(&st)?;
        let bad = |e: String| bad(&st, e);
        let pt = point(p.lat, p.lon).map_err(bad)?;
        let op = SnapOp {
            radius_m: radius(p.radius).map_err(bad)?,
            lang: lang(idx, p.lang.as_deref()).map_err(bad)?,
            vehicle: vehicle_only(p.roads.as_deref()).map_err(bad)?,
        };
        Ok(match timed(&st.metrics, || op.run(idx, pt)) {
            Ok(s) => Json(s).into_response(),
            Err(e) => error(StatusCode::NOT_FOUND, e),
        })
    };
    run().unwrap_or_else(|e| e)
}

async fn snap_batch(State(st): St, b: Result<Json<SnapBatchRequest>, JsonRejection>) -> Response {
    st.metrics.snap_batch.fetch_add(1, Ordering::Relaxed);
    let parse = || -> Result<(Vec<BatchPoint>, SnapOp), Response> {
        let req = body(&st, b)?;
        let idx = index(&st)?;
        let bad = |e: String| bad(&st, e);
        batch_size(req.points.len()).map_err(bad)?;
        let op = SnapOp {
            radius_m: radius(req.radius).map_err(bad)?,
            lang: lang(idx, req.lang.as_deref()).map_err(bad)?,
            vehicle: vehicle_only(req.roads.as_deref()).map_err(bad)?,
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
