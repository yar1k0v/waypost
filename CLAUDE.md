# waypost

Rust reverse-geocoding and road-snapping service. README.md has the quick start, layout and API. docs/ has architecture, API and operations.

## Commands

1. Check: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`.
2. Run locally: `docker compose up -d` (settings in `waypost.env`), or build from source per docs/operations.md.
3. Accuracy: see docs/operations.md "Accuracy checks". Test data lives in `data/` (git-ignored).

## Gotchas

1. Record layouts in `crates/waypost-core/src/model.rs` are the file format. Changing one needs `FORMAT_VERSION` bumped in `format.rs` and the size test updated. `tools/wpi.py` mirrors some layouts.
2. CI fails a pull request when any answer field changes on more than 0.5% of 10,000 test points. For an intended change, raise `--max-drop` in `.github/workflows/accuracy.yml`.
3. Never commit OSM data, test answers or real vehicle positions. Keep them in `data/`.
