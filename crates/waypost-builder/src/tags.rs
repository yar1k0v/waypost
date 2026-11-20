//! Tag access shared by the OSM passes.

use waypost_core::model::{NONE, NameId, StrRef};

use crate::Tables;

/// A small tag list copied out of one OSM element.
pub struct Tags<'a>(pub Vec<(&'a str, &'a str)>);

impl<'a> Tags<'a> {
    pub fn get(&self, key: &str) -> Option<&'a str> {
        self.0
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| *v)
            .filter(|v| !v.trim().is_empty())
    }
}

/// Builds a name from `name` and `name:<lang>`. Falls back to `fallback_key`
/// (for example `ref` on roads) when there is no `name`. Returns `NONE` when
/// there is nothing to name the feature with.
pub fn name_of(
    tables: &mut Tables,
    get: impl Fn(&str) -> Option<String>,
    fallback_key: Option<&str>,
) -> NameId {
    let default = get("name").or_else(|| fallback_key.and_then(&get));
    let Some(default) = default else {
        return NONE;
    };
    let default = tables.strings.add(default.trim());
    let mut alts: Vec<(u32, StrRef)> = Vec::new();
    for (i, lang) in tables.langs.clone().iter().enumerate() {
        if let Some(v) = get(&format!("name:{lang}")) {
            let s = tables.strings.add(v.trim());
            if s != default {
                alts.push((i as u32, s));
            }
        }
    }
    tables.names.add(default, &alts)
}

/// Parses `maxspeed` into km/h. Returns 0 when absent or not a plain number.
/// Takes the first value of a `;` or `|` list and understands `mph`, `knots`
/// and `zone:code` values such as `DE:urban`.
pub fn maxspeed_kmh(v: Option<&str>) -> u16 {
    let Some(v) = v else { return 0 };
    let v = v.trim();
    if let Some(mph) = v.strip_suffix("mph") {
        return mph
            .trim()
            .parse::<f64>()
            .map_or(0, |x| (x * 1.609_344).round() as u16);
    }
    if let Some(kn) = v.strip_suffix("knots") {
        return kn
            .trim()
            .parse::<f64>()
            .map_or(0, |x| (x * 1.852).round() as u16);
    }
    let first = v.split([';', '|']).next().unwrap_or(v).trim();
    if let Some((country, zone)) = first.split_once(':') {
        // Countries whose legal defaults differ from the common 50/100/130
        // (OSM wiki "Default speed limits", passenger cars).
        return match (country, zone) {
            ("RU", "urban") | ("KZ", "urban") => 60,
            ("RU", "rural") => 90,
            ("KZ", "rural") => 110,
            ("RU", "motorway") => 110,
            ("KZ", "motorway") => 140,
            (_, "urban") => 50,
            (_, "rural") => 100,
            (_, "motorway") => 130,
            (_, "living_street") => 10,
            _ => 0,
        };
    }
    first
        .trim_end_matches("km/h")
        .trim()
        .parse::<u16>()
        .unwrap_or(0)
}

/// Maps the OSM `oneway` tag: 0 = both directions, 1 = forward only,
/// 2 = backward only. Without a `oneway` tag, roundabouts
/// (`junction=roundabout|circular`) and `highway=motorway` are one-way,
/// as OSM implies.
pub fn oneway(tags: &Tags) -> u8 {
    match tags.get("oneway") {
        Some("yes" | "true" | "1") => 1,
        Some("-1" | "reverse") => 2,
        Some(_) => 0,
        None if matches!(tags.get("junction"), Some("roundabout" | "circular"))
            || tags.get("highway") == Some("motorway") =>
        {
            1
        }
        None => 0,
    }
}

/// First integer in a house number, for interpolation endpoints ("12a" -> 12).
pub fn leading_number(v: &str) -> Option<u32> {
    let digits: String = v
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok().filter(|&n| n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maxspeed() {
        assert_eq!(maxspeed_kmh(Some("60")), 60);
        assert_eq!(maxspeed_kmh(Some("30 mph")), 48);
        assert_eq!(maxspeed_kmh(Some("30 knots")), 56);
        assert_eq!(maxspeed_kmh(Some("50;70")), 50);
        assert_eq!(maxspeed_kmh(Some("DE:urban")), 50);
        assert_eq!(maxspeed_kmh(Some("RU:urban")), 60);
        assert_eq!(maxspeed_kmh(Some("RU:rural")), 90);
        assert_eq!(maxspeed_kmh(Some("KZ:rural")), 110);
        assert_eq!(maxspeed_kmh(Some("KZ:motorway")), 140);
        assert_eq!(maxspeed_kmh(None), 0);
    }

    #[test]
    fn oneway_values() {
        let ow = |tags: &[(&'static str, &'static str)]| oneway(&Tags(tags.to_vec()));
        assert_eq!(ow(&[("oneway", "yes")]), 1);
        assert_eq!(ow(&[("oneway", "true")]), 1);
        assert_eq!(ow(&[("oneway", "1")]), 1);
        assert_eq!(ow(&[("oneway", "-1")]), 2);
        assert_eq!(ow(&[("oneway", "reverse")]), 2);
        assert_eq!(ow(&[("oneway", "no")]), 0);
        assert_eq!(ow(&[]), 0);
        // Implied one-way, and an explicit tag overriding it.
        assert_eq!(ow(&[("junction", "roundabout")]), 1);
        assert_eq!(ow(&[("junction", "circular")]), 1);
        assert_eq!(ow(&[("highway", "motorway")]), 1);
        assert_eq!(ow(&[("highway", "motorway_link")]), 0);
        assert_eq!(ow(&[("highway", "motorway"), ("oneway", "no")]), 0);
        assert_eq!(ow(&[("junction", "roundabout"), ("oneway", "-1")]), 2);
    }

    #[test]
    fn numbers() {
        assert_eq!(leading_number("12a"), Some(12));
        assert_eq!(leading_number("a12"), None);
        assert_eq!(leading_number("0"), None);
    }
}
