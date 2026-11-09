# API reference

HTTP/JSON on port 8080. Examples come from a Switzerland index (OSM data of 2026-11-08).

⚠ No authentication. Keep the port on a private network, or put an authenticating proxy in front.

## GET /v1/reverse

| Parameter | Required | Default | Meaning |
|---|---|---|---|
| `lat`, `lon` | yes | | WGS84 degrees, lat in [-90, 90], lon in [-180, 180] |
| `radius` | no | 100 | Search radius for houses and streets, metres, max 1000 |
| `lang` | no | | Language code from `/v1/info`; falls back to `name` |
| `street` | no | `hybrid` | `hybrid` or `road`, see [architecture](architecture.md#lookup) |
| `admin_only` | no | false | Return admin areas only |

```sh
curl 'http://127.0.0.1:8088/v1/reverse?lat=46.2044&lon=6.1432&lang=fr'
```

```json
{"match":"address","distance_m":0.8,
 "address":{"house_number":"2","street":"Place de Bel-Air","road_class":"residential","suburb":"Cité",
            "locality":"Genève","state":"Genève","postcode":"1204","country":"Suisse","country_code":"CH"},
 "display":"Place de Bel-Air 2, Cité, Genève, 1204, Suisse",
 "source":{"osm_type":"node","osm_id":1083432136,"data_date":"2026-11-08T21:21:00Z"}}
```

| Field | Meaning |
|---|---|
| `match` | `address`, `interpolated`, `street`, `place` (settlement only), `admin` (areas only), `none` |
| `distance_m` | Metres to the matched road, house or settlement. 0 when an area that contains the point decided the match. Absent for `none` |
| `address.*` | `house_number`, `street`, `road_class`, `suburb`, `locality`, `district`, `state`, `postcode`, `country`, `country_code`; absent fields are omitted |
| `display` | One-line address |
| `source` | `osm_type` (`node`, `way`, `relation`) and `osm_id` of the object that decided `match`, plus `data_date` of the index's OSM data. `osm_*` absent for `none` |

## POST /v1/reverse/batch

```json
{"points":[{"lat":46.2044,"lon":6.1432,"id":"veh-1"},{"lat":46.9466,"lon":7.4441}],
 "radius":75, "lang":"de", "street":"hybrid", "admin_only":false}
```

- At most 10,000 points; body at most 2 MB.
- `results` keeps input order. Each item echoes `id` when given.
- A point with bad coordinates gets `"error"` in its slot; the others still answer.

## GET /v1/snap

| Parameter | Required | Default | Meaning |
|---|---|---|---|
| `lat`, `lon` | yes | | WGS84 degrees |
| `radius` | no | 100 | Metres, max 1000 |
| `lang` | no | | Road name language |
| `roads` | no | `vehicle` | `vehicle` skips pedestrian ways; `all` keeps them |

```json
{"lat":46.9479729,"lon":7.4474197,"distance_m":3.4,
 "road":{"name":"Marktgasse","class":"residential","maxspeed_kmh":30,"osm_way_id":691907770},
 "source":{"osm_type":"way","osm_id":691907770,"data_date":"2026-11-08T21:21:00Z"}}
```

404 `{"error":"no road within radius"}` when no road is that close.

## POST /v1/snap/batch

Same body as the reverse batch, with `roads` instead of `street` and `admin_only`. A point with no road gets `"error"`.

## Legacy-compatible routes

For clients written against the older geocoder contract. They answer in that contract's shape.

| Route | Parameters | Response |
|---|---|---|
| `GET /reverse` | `lat`, `lon`, `distance` (metres), `key` (ignored), plus `lang`, `street` | `{"display_name", "address":{house_number, road, suburb, city, state, county, postcode, country, country_code}}` |
| `GET /snap` | `lat`, `lon`, `distance` | `{"lat","lon","distance"}`, 404 when no road is near |

- Nothing near (including 0,0): `/reverse` answers 200 `{"address":{}}`.
- Field mapping: `road` = `street`, `city` = `locality`, `county` = `district`.

## Service endpoints

| Endpoint | Answer |
|---|---|
| `GET /v1/info` | Index metadata: builder version, build time, sources, data date, settings |
| `GET /healthz` | 200 `ok` while the process runs |
| `GET /readyz` | 200 after the index is loaded and warmed, else 503 |
| `GET /metrics` | Prometheus text: request counters, `waypost_lookup_seconds` histogram, `waypost_ready` |

## Errors

| Status | When |
|---|---|
| 400 | Missing, unparsable or out-of-range `lat`/`lon`, bad `radius` or `admin_only`, unknown `lang`, `street` or `roads`, batch over 10,000 points, bad JSON |
| 404 | `/v1/snap` and `/snap`: no road within the radius |
| 503 | Index still loading |

Error body: `{"error":"<reason>"}`.
