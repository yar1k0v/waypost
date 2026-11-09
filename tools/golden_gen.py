"""Generates a synthetic golden set of coordinates from a waypost index.

Points are sampled near streets, address points and settlements, so they
look like real GPS positions. Output: JSON lines {"id", "lat", "lon", "kind"}.

Usage: python tools/golden_gen.py --index data/ch.wpi --n 10000 --out data/points.jsonl
"""
import argparse
import json
import math
import random

from wpi import Index

M_PER_DEG = 111_195.0


def offset(lat, lon, max_m, rnd):
    """Moves a point by up to max_m metres in a random direction."""
    d = max_m * math.sqrt(rnd.random())
    a = rnd.random() * 2 * math.pi
    dlat = d * math.cos(a) / M_PER_DEG
    dlon = d * math.sin(a) / (M_PER_DEG * math.cos(math.radians(lat)))
    return lat + dlat, lon + dlon


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--index", required=True)
    ap.add_argument("--n", type=int, default=10_000)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    rnd = random.Random(args.seed)
    idx = Index(args.index)
    n_pieces, n_addrs, n_places = idx.count("pieces"), idx.count("addrs"), idx.count("places")
    mix = [("street", 0.5), ("address", 0.3), ("place", 0.2)]
    with open(args.out, "w", encoding="utf-8", newline="\n") as out:
        for i in range(args.n):
            r, kind = rnd.random(), "place"
            acc = 0.0
            for k, w in mix:
                acc += w
                if r < acc:
                    kind = k
                    break
            if kind == "street":
                node_off, _, n = idx.record("pieces", rnd.randrange(n_pieces))
                j = node_off + rnd.randrange(n - 1)
                a, b = idx.record("nodes", j), idx.record("nodes", j + 1)
                t = rnd.random()
                lat = (a[0] + t * (b[0] - a[0])) / 1e7
                lon = (a[1] + t * (b[1] - a[1])) / 1e7
                lat, lon = offset(lat, lon, 40, rnd)
            elif kind == "address":
                rec = idx.record("addrs", rnd.randrange(n_addrs))
                lat, lon = offset(rec[0] / 1e7, rec[1] / 1e7, 25, rnd)
            else:
                rec = idx.record("places", rnd.randrange(n_places))
                lat, lon = offset(rec[0] / 1e7, rec[1] / 1e7, 1500, rnd)
            out.write(json.dumps({"id": i, "lat": round(lat, 7), "lon": round(lon, 7), "kind": kind}) + "\n")


if __name__ == "__main__":
    main()
