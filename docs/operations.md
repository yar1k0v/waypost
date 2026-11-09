# Operations

## Run with docker compose

Settings live in `waypost.env`:

| Setting | Default | Meaning |
|---|---|---|
| `PBF_URLS` | Switzerland | Geofabrik extract URLs, space-separated. Several countries go into one index |
| `WAYPOST_LANGS` | `en,de,fr,it` | Languages stored besides `name` |
| `REBUILD` | `0` | `1` forces a rebuild, for example to pick up fresh OSM data |
| `WAYPOST_PORT` | `8088` | Host port. Set it in the shell or in `.env`, not in `waypost.env` |

The index rebuilds by itself when `PBF_URLS`, `WAYPOST_LANGS` or the builder image changes.

```sh
docker compose up -d                  # first run downloads and builds
docker compose logs -f index          # build progress
docker compose up -d --force-recreate # after a change in waypost.env
```

- The `index` service runs once, then exits. The `server` service starts after it succeeds.
- The index rebuilds by itself when `PBF_URLS` or `WAYPOST_LANGS` change. Otherwise the build is skipped.
- A failed build leaves the old index in place and the server does not start. Check `docker compose logs index`.

## Build from source

Needs Rust 1.98+ and osmium-tool. On Windows or macOS without osmium-tool, run `docker compose build index` once and prefix the prep step with `OSMIUM=scripts/osmium-docker.sh`.

```sh
curl -fSL -o data/switzerland-latest.osm.pbf https://download.geofabrik.de/europe/switzerland-latest.osm.pbf
scripts/prep.sh data/switzerland-latest.osm.pbf data/prep
cargo run --release -p waypost-builder -- --out data/ch.wpi --langs en,de,fr,it data/prep/switzerland-latest
cargo run --release -p waypost-server -- --index data/ch.wpi --listen 127.0.0.1:8088
```

| Builder flag | Default | Meaning |
|---|---|---|
| `--out` | | Index file to write |
| `--langs` | `en` | Languages stored besides `name` |
| `--street-cell` | 0.005 | Street grid cell, degrees |
| `--admin-cell` | 0.05 | Admin grid cell, degrees |

| Server setting | Flag | Env | Default |
|---|---|---|---|
| Index file | `--index` | `WAYPOST_INDEX` | `/index/waypost.wpi` in the image |
| Listen address | `--listen` | `WAYPOST_LISTEN` | `0.0.0.0:8080` |

Both images run as uid 10001. A bind-mounted index directory must be writable by that uid for the builder and readable for the server.

## Sizing

Measured on Switzerland, OSM data of 2026-11-08:

| Item | Value | Rule |
|---|---|---|
| Extract download | 550 MB | |
| osmium prep | about 1 min | Dominates build time |
| Index build | 10 s | |
| Index file | 369 MB | Grows with roads and addresses |
| Server memory, warmed | about the index size | Index size plus a small heap. The page cache counts toward a container memory limit |
| Lookup latency | `cargo run --release -p waypost-core --example bench -- <index> <points.jsonl>` | Prints p50, p99 and lookups per second on one thread |

Set a server memory limit of index size + 256 MB or more.

## Accuracy checks

| Check | Command |
|---|---|
| Test points from an index | `python tools/golden_gen.py --index data/ch.wpi --n 10000 --out data/points.jsonl` |
| Save answers as a baseline | `python tools/compare.py --golden data/points.jsonl --waypost http://127.0.0.1:8088 --baseline data/baseline.jsonl --write-baseline` |
| Compare with the baseline | `python tools/compare.py --golden data/points.jsonl --waypost http://127.0.0.1:8088 --baseline data/baseline.jsonl` |
| Two servers (A/B) | `python tools/compare.py --golden data/points.jsonl --waypost http://127.0.0.1:8088 --against http://127.0.0.1:8089` |
| Against another geocoder's stored answers | `python tools/compare_reference.py data/reference-reverse.jsonl http://127.0.0.1:8088` |

- CI (`.github/workflows/accuracy.yml`) builds the base branch and the pull request on the same Switzerland index. It fails when any field changes on more than 0.5% of points.
- It runs on pull requests that change `crates/`, `tools/`, `scripts/`, `Cargo.toml`, `Cargo.lock` or the workflow itself.
- Test points and answers stay in `data/` (git-ignored). The repository holds no OSM data.
- ⚠ Keep real location data in `data/` only. Never commit it.

## Release

Releases are automatic. Nobody edits `version` or pushes tags by hand.

1. Write commits as [Conventional Commits](https://www.conventionalcommits.org): `type(scope): summary`.
2. When `ci` passes on `main`, `.github/workflows/release.yml` runs `tools/release.py next`. It reads the commits since the last `vX.Y.Z` tag.
3. If a release is due, the workflow bumps `Cargo.toml` and `Cargo.lock`, commits `chore(release): vX.Y.Z`, tags it, and pushes both.
4. It builds and pushes `ghcr.io/yar1k0v/waypost-server` and `ghcr.io/yar1k0v/waypost-builder` with the tags `X.Y.Z` and `latest`, then creates a GitHub release.

| Commit | Version 0.x | Version 1.x and later |
|---|---|---|
| `type!:` or `BREAKING CHANGE:` footer | minor | major |
| `feat` | patch | minor |
| `fix`, `perf` | patch | patch |
| `docs`, `build`, `ci`, `chore`, `test`, `refactor`, `style` | no release | no release |

- Version 1.0.0 is a manual step: set `version` to `1.0.0` in `Cargo.toml`, `Cargo.lock` and the crate path dependencies, then tag `v1.0.0`.
- If the image build fails, the tag already exists. Run the `release` workflow by hand with `tag` set to `vX.Y.Z` to rebuild only the images.
- Test the version logic with `python tools/test_release.py`. Preview the next version with `python tools/release.py next`.

## When it breaks

| Symptom | Check |
|---|---|
| `/readyz` stays 503 | Server log: `cannot open index` names the reason (missing file, wrong format version, corrupt section) |
| Server killed for memory | Memory limit below index size + 256 MB |
| Empty addresses in one region | `/v1/info` sources: is that country's extract in the index? |
| `lang xx is not in this index` | Add the language to `WAYPOST_LANGS` and rebuild |
| Build fails | `docker compose logs index`. The old index stays in place until a build completes |
