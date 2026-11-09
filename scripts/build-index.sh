#!/bin/sh
# Builds a waypost index from Geofabrik extracts. Runs in the builder image.
#
# Env:
#   PBF_URLS       space-separated Geofabrik extract URLs (required)
#   WAYPOST_LANGS  name languages, comma-separated (default en)
#   INDEX_OUT      index file to write (default /index/waypost.wpi)
#   REBUILD        1 forces a rebuild
#
# The build is skipped when INDEX_OUT exists and was built from the same
# PBF_URLS and WAYPOST_LANGS (recorded in INDEX_OUT.settings).
# Exit code: 0 on success or skip, non-zero on any failure. On failure
# INDEX_OUT is left as it was.
#
# The index is written to a temporary name first and renamed when complete,
# so a running server never sees a half-written file.
set -eu
: "${PBF_URLS:?set PBF_URLS}"
langs=${WAYPOST_LANGS:-en}
out=${INDEX_OUT:-/index/waypost.wpi}
work=${WORK_DIR:-/work}

settings="urls=$PBF_URLS langs=$langs"
if [ -f "$out" ] && [ "${REBUILD:-0}" != 1 ] \
    && [ "$(cat "$out.settings" 2>/dev/null)" = "$settings" ]; then
    echo "index is up to date: $out (set REBUILD=1 to rebuild)"
    exit 0
fi
mkdir -p "$work/pbf" "$work/prep" "$(dirname "$out")"

prefixes=""
for url in $PBF_URLS; do
    name=$(basename "$url" .osm.pbf)
    echo "download $url"
    curl -fSL --retry 3 -o "$work/pbf/$name.osm.pbf.part" "$url"
    mv "$work/pbf/$name.osm.pbf.part" "$work/pbf/$name.osm.pbf"
    prep.sh "$work/pbf/$name.osm.pbf" "$work/prep"
    rm "$work/pbf/$name.osm.pbf"
    prefixes="$prefixes $work/prep/$name"
done

# shellcheck disable=SC2086 # prefixes is a word list on purpose
waypost-builder --out "$out.tmp" --langs "$langs" $prefixes
mv "$out.tmp" "$out"
printf '%s\n' "$settings" > "$out.settings"
rm -rf "$work/prep"
echo "wrote $out"
