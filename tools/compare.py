"""Runs the golden set against waypost and, optionally, a reference geocoder with the legacy /reverse contract.

Prints, per field, how often each service fills it and how often they agree,
plus points per second. With --baseline it also gates on regressions against
a stored snapshot of waypost answers, and exits 1 when they exceed --max-drop.

Usage:
  python tools/compare.py --golden data/points.jsonl --waypost http://127.0.0.1:8088 \
      [--reference http://127.0.0.1:3000 --reference-key KEY] \
      [--baseline data/baseline.jsonl [--write-baseline] [--max-drop 0.5]] \
      [--against http://127.0.0.1:8089 [--max-drop 0.5]]       [--report data/report.json]

--against runs the same points on a second waypost (for example the base
branch in CI) and gates on the share of changed answers per field.
"""
import argparse
import concurrent.futures as cf
import json
import sys
import time
import urllib.parse
import urllib.request

# waypost field -> legacy contract field
FIELDS = {
    "house_number": "house_number",
    "street": "road",
    "locality": "city",
    "district": "county",
    "state": "state",
    "postcode": "postcode",
    "country_code": "country_code",
}


def norm(v):
    return v.strip().casefold() if isinstance(v, str) and v.strip() else None


def post_json(url, body, timeout=120):
    req = urllib.request.Request(url, data=json.dumps(body).encode(), headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)


def get_json(url, timeout=30):
    with urllib.request.urlopen(url, timeout=timeout) as r:
        return json.load(r)


def run_waypost(base, points, chunk=10_000):
    out, t0 = [], time.perf_counter()
    for i in range(0, len(points), chunk):
        part = [{"id": p["id"], "lat": p["lat"], "lon": p["lon"]} for p in points[i:i + chunk]]
        res = post_json(base.rstrip("/") + "/v1/reverse/batch", {"points": part})["results"]
        out.extend(r.get("address", {}) for r in res)
    return out, len(points) / (time.perf_counter() - t0)


def run_reference(base, key, points, threads):
    def one(p):
        q = {"lat": p["lat"], "lon": p["lon"]}
        if key:
            q["key"] = key
        try:
            return get_json(base.rstrip("/") + "/reverse?" + urllib.parse.urlencode(q)).get("address", {})
        except Exception as e:  # noqa: BLE001 - count failures, keep going
            return {"_error": str(e)}
    t0 = time.perf_counter()
    with cf.ThreadPoolExecutor(threads) as ex:
        out = list(ex.map(one, points))
    return out, len(points) / (time.perf_counter() - t0)


def field_stats(wp, tr):
    n = len(wp)
    stats = {}
    for wf, tf in FIELDS.items():
        a = [norm(x.get(wf)) for x in wp]
        b = [norm(x.get(tf)) for x in tr]
        both = sum(1 for x, y in zip(a, b) if x and y)
        agree = sum(1 for x, y in zip(a, b) if x and y and x == y)
        stats[wf] = {
            "waypost_filled": round(100 * sum(1 for x in a if x) / n, 1),
            "reference_filled": round(100 * sum(1 for y in b if y) / n, 1),
            "agree_when_both": round(100 * agree / both, 1) if both else None,
            "only_waypost": round(100 * sum(1 for x, y in zip(a, b) if x and not y) / n, 1),
            "only_reference": round(100 * sum(1 for x, y in zip(a, b) if y and not x) / n, 1),
        }
    return stats


def regressions(now, base):
    """Share of points, per field, where the baseline had a value and now differs."""
    out = {}
    for f in FIELDS:
        had = [(norm(b.get(f)), norm(n.get(f))) for n, b in zip(now, base) if norm(b.get(f))]
        lost = sum(1 for b, n in had if b != n)
        out[f] = round(100 * lost / len(had), 2) if had else 0.0
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--golden", required=True)
    ap.add_argument("--waypost", required=True)
    ap.add_argument("--reference")
    ap.add_argument("--reference-key")
    ap.add_argument("--threads", type=int, default=32)
    ap.add_argument("--against", help="second waypost URL to gate against")
    ap.add_argument("--baseline")
    ap.add_argument("--write-baseline", action="store_true")
    ap.add_argument("--max-drop", type=float, default=0.5, help="max %% of points that may change per field")
    ap.add_argument("--report")
    args = ap.parse_args()

    points = [json.loads(line) for line in open(args.golden, encoding="utf-8")]
    wp, wp_pps = run_waypost(args.waypost, points)
    report = {"points": len(points), "waypost_points_per_s": round(wp_pps)}
    print(f"waypost: {len(points)} points, {wp_pps:,.0f} points/s (batch)")

    if args.reference:
        tr, tr_pps = run_reference(args.reference, args.reference_key, points, args.threads)
        errors = sum(1 for x in tr if "_error" in x)
        report.update(reference_points_per_s=round(tr_pps), reference_errors=errors, fields=field_stats(wp, tr))
        print(f"reference: {tr_pps:,.0f} points/s ({args.threads} threads), {errors} errors")
        print(f"{'field':<14}{'waypost%':>9}{'ref%':>9}{'agree%':>8}{'only_wp%':>9}{'only_tr%':>9}")
        for f, s in report["fields"].items():
            ag = "-" if s["agree_when_both"] is None else s["agree_when_both"]
            print(f"{f:<14}{s['waypost_filled']:>9}{s['reference_filled']:>9}{ag:>8}{s['only_waypost']:>9}{s['only_reference']:>9}")
    else:
        filled = {f: round(100 * sum(1 for x in wp if norm(x.get(f))) / len(wp), 1) for f in FIELDS}
        report["waypost_filled"] = filled
        print("filled %:", filled)

    code = 0
    gates = []
    if args.against:
        ref, _ = run_waypost(args.against, points)
        gates.append(("against " + args.against, ref))
    if args.baseline and not args.write_baseline:
        gates.append(("baseline", [json.loads(line) for line in open(args.baseline, encoding="utf-8")]))
    for name, ref in gates:
        reg = regressions(wp, ref)
        report.setdefault("changed_pct", {})[name] = reg
        bad = {f: v for f, v in reg.items() if v > args.max_drop}
        print(f"changed vs {name} %:", reg)
        if bad:
            print(f"FAIL: fields over {args.max_drop}% vs {name}: {bad}", file=sys.stderr)
            code = 1
    if args.baseline and args.write_baseline:
        if args.write_baseline:
            with open(args.baseline, "w", encoding="utf-8", newline="\n") as f:
                for x in wp:
                    f.write(json.dumps(x, ensure_ascii=False, sort_keys=True) + "\n")
            print(f"baseline written: {args.baseline}")
    if args.report:
        with open(args.report, "w", encoding="utf-8", newline="\n") as f:
            json.dump(report, f, ensure_ascii=False, indent=1)
    sys.exit(code)


if __name__ == "__main__":
    main()
