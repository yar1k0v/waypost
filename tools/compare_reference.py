"""Field-by-field diff of waypost against stored answers of a reference geocoder.

Input: JSON lines {lat, lon, status, body}, where body is the reference
service's response in the legacy /reverse format (display_name, address{road,
city, county, ...}). Keep real location data in data/, never in the repo.

Usage: python tools/compare_reference.py data/reference-reverse.jsonl http://127.0.0.1:8088 [--radius 75] [--house nearest]
"""
import argparse
import json
import urllib.request

FIELDS = {
    "house_number": "house_number",
    "street": "road",
    "suburb": "suburb",
    "locality": "city",
    "district": "county",
    "state": "state",
    "postcode": "postcode",
    "country_code": "country_code",
}


def norm(v):
    return v.strip().casefold() if isinstance(v, str) and v.strip() else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("reference")
    ap.add_argument("waypost")
    ap.add_argument("--radius", type=float, default=75)
    ap.add_argument("--house", choices=["exact", "nearest"], help="house matching rule, sent as `house`")
    args = ap.parse_args()
    tr = [json.loads(line) for line in open(args.reference, encoding="utf-8")]
    body = json.dumps({
        "points": [{"lat": t["lat"], "lon": t["lon"]} for t in tr],
        "radius": args.radius,
        **({"house": args.house} if args.house else {}),
    }).encode()
    req = urllib.request.Request(args.waypost.rstrip("/") + "/v1/reverse/batch", data=body,
                                 headers={"content-type": "application/json"})
    wp = json.load(urllib.request.urlopen(req, timeout=120))["results"]
    n = len(tr)
    print(f"{n} points")
    print(f"{'field':<13}{'waypost%':>9}{'ref%':>9}{'agree%':>8}{'only_wp':>8}{'only_tr':>8}{'differ':>7}")
    for wf, tf in FIELDS.items():
        a = [norm(w.get("address", {}).get(wf)) for w in wp]
        b = [norm(t["body"].get("address", {}).get(tf)) for t in tr]
        both = [(x, y) for x, y in zip(a, b) if x and y]
        agree = sum(x == y for x, y in both)
        only_wp = sum(1 for x, y in zip(a, b) if x and not y)
        only_tr = sum(1 for x, y in zip(a, b) if y and not x)
        pct = 100 * agree / len(both) if both else 0.0
        print(f"{wf:<13}{100 * sum(1 for x in a if x) / n:>9.1f}{100 * sum(1 for y in b if y) / n:>9.1f}"
              f"{pct:>8.1f}{only_wp:>8}{only_tr:>8}{len(both) - agree:>7}")
    hd = sorted(w["house_distance_m"] for w in wp if w.get("house_distance_m") is not None)
    if hd:
        q = lambda f: hd[min(len(hd) - 1, int(f * len(hd)))]
        print(f"house_distance_m: {len(hd)} houses ({100 * len(hd) / n:.1f}% of points), "
              f"median {q(0.5):.1f} m, p90 {q(0.9):.1f} m, max {hd[-1]:.1f} m")


if __name__ == "__main__":
    main()
