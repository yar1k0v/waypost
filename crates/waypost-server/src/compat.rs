//! Routes compatible with the legacy geocoder contract, so existing clients switch without
//! code changes: `GET /reverse` and `GET /snap` with `lat`, `lon`,
//! `distance` (metres) and `key` (accepted and ignored).
//!
//! The display name format reimplements `format_rules` and `format_address`
//! from github.com/traccar/traccar-geocoder (Apache-2.0), kept here as license
//! attribution. Like the legacy service, `/reverse` answers 200
//! with an empty `address` when nothing is near; `/snap` answers 404.

use std::sync::atomic::Ordering;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use waypost_core::geo::Point;
use waypost_core::lookup::{self, Address};

use crate::{St, error, lang, not_ready, radius, street_rule};

#[derive(Deserialize)]
pub struct Params {
    lat: Option<f64>,
    lon: Option<f64>,
    distance: Option<f64>,
    #[allow(dead_code)]
    key: Option<String>,
    lang: Option<String>,
    street: Option<String>,
}

#[derive(Serialize, Default)]
struct Details<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    house_number: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    road: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    suburb: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    city: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    county: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    postcode: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    country: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    country_code: Option<&'a str>,
}

#[derive(Serialize)]
struct Reverse<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    address: Details<'a>,
}

#[derive(Serialize)]
struct Snap {
    lat: f64,
    lon: f64,
    distance: f64,
}

fn details<'a>(a: &'a Address<'_>) -> Details<'a> {
    Details {
        house_number: a.house_number.as_deref(),
        road: a.street,
        suburb: a.suburb,
        city: a.locality,
        state: a.state,
        county: a.district,
        postcode: a.postcode,
        country: a.country,
        country_code: a.country_code.as_deref(),
    }
}

/// (number after street, postcode before city, include state) by country.
fn format_rules(cc: Option<&str>) -> (bool, bool, bool) {
    match cc {
        Some(
            "US" | "CA" | "AU" | "NZ" | "GB" | "IE" | "ZA" | "IN" | "NG" | "KE" | "GH" | "PK"
            | "PH" | "TH" | "MY",
        ) => (false, false, true),
        Some("JP" | "KR" | "CN" | "TW") => (false, true, true),
        _ => (true, true, false),
    }
}

fn display_name(d: &Details<'_>) -> Option<String> {
    if d.road.is_none() && d.city.is_none() && d.country.is_none() {
        return None;
    }
    let (number_after, postcode_first, with_state) = format_rules(d.country_code);
    let mut parts: Vec<String> = Vec::new();
    if let Some(road) = d.road {
        parts.push(match d.house_number {
            Some(hn) if number_after => format!("{road} {hn}"),
            Some(hn) => format!("{hn} {road}"),
            None => road.to_string(),
        });
    }
    let mut city = String::new();
    if postcode_first {
        if let Some(pc) = d.postcode {
            city.push_str(pc);
            city.push(' ');
        }
        if let Some(c) = d.city {
            city.push_str(c);
        }
        if with_state && let Some(s) = d.state {
            if !city.is_empty() {
                city.push_str(", ");
            }
            city.push_str(s);
        }
        let c = city.trim();
        if !c.is_empty() {
            parts.push(c.to_string());
        }
    } else {
        if let Some(c) = d.city {
            city.push_str(c);
        }
        if with_state && let Some(s) = d.state {
            if !city.is_empty() {
                city.push_str(", ");
            }
            city.push_str(s);
        }
        if let Some(pc) = d.postcode {
            if !city.is_empty() {
                city.push(' ');
            }
            city.push_str(pc);
        }
        if !city.is_empty() {
            parts.push(city);
        }
    }
    if let Some(c) = d.country {
        parts.push(c.to_string());
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Parses the shared parameters. `Err` is a ready 400 response.
#[allow(clippy::result_large_err)] // A Response is the natural error here.
fn parse(
    st: &crate::AppState,
    idx: &lookup::Index,
    p: &Params,
) -> Result<(Point, f64, Option<u32>, lookup::StreetRule), Response> {
    let bad = |m: String| {
        st.metrics.bad_requests.fetch_add(1, Ordering::Relaxed);
        error(StatusCode::BAD_REQUEST, m)
    };
    let (Some(lat), Some(lon)) = (p.lat, p.lon) else {
        return Err(bad("lat and lon are required".into()));
    };
    let pt = Point::from_deg(lat, lon)
        .ok_or_else(|| bad("lat must be in [-90, 90] and lon in [-180, 180]".into()))?;
    let r = radius(p.distance).map_err(|e| bad(e.into()))?;
    let l = lang(idx, p.lang.as_deref()).map_err(bad)?;
    let rule = street_rule(p.street.as_deref()).map_err(bad)?;
    Ok((pt, r, l, rule))
}

pub async fn reverse(State(st): St, Query(p): Query<Params>) -> Response {
    st.metrics.reverse.fetch_add(1, Ordering::Relaxed);
    let Some(idx) = st.index.get() else {
        return not_ready();
    };
    let (pt, radius_m, lang, street_rule) = match parse(&st, idx, &p) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let q = lookup::Query {
        pt,
        radius_m,
        lang,
        admin_only: false,
        street_rule,
    };
    let ans = crate::timed(&st.metrics, idx, &q);
    let address = details(&ans.address);
    Json(Reverse {
        display_name: display_name(&address),
        address,
    })
    .into_response()
}

pub async fn snap(State(st): St, Query(p): Query<Params>) -> Response {
    st.metrics.snap.fetch_add(1, Ordering::Relaxed);
    let Some(idx) = st.index.get() else {
        return not_ready();
    };
    let (pt, radius_m, lang, _) = match parse(&st, idx, &p) {
        Ok(v) => v,
        Err(r) => return r,
    };
    match idx.snap(pt, radius_m, lang, true) {
        Some(s) => Json(Snap {
            lat: s.lat,
            lon: s.lon,
            distance: s.distance_m,
        })
        .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_like_legacy() {
        let d = Details {
            road: Some("Rue du Rhône"),
            country: Some("Suisse"),
            country_code: Some("CH"),
            ..Default::default()
        };
        assert_eq!(display_name(&d).unwrap(), "Rue du Rhône, Suisse");
        let d = Details {
            road: Some("Bundesplatz"),
            house_number: Some("3"),
            postcode: Some("3011"),
            city: Some("Bern"),
            state: Some("Bern"),
            country: Some("Switzerland"),
            country_code: Some("CH"),
            ..Default::default()
        };
        assert_eq!(
            display_name(&d).unwrap(),
            "Bundesplatz 3, 3011 Bern, Switzerland"
        );
        assert!(display_name(&Details::default()).is_none());
    }
}
