#!/bin/sh
# Prepares one OSM extract for waypost-builder with osmium-tool.
# Usage: prep.sh <input.osm.pbf> <out-dir>
# Writes <out-dir>/<name>.streets.pbf (ways carry node locations),
#        <out-dir>/<name>.areas.geojsonseq (assembled boundary and place polygons),
#        <out-dir>/<name>.date (OSM data timestamp, may be empty).
# OSMIUM overrides the osmium command, for example a docker wrapper.
set -eu
in=$1
out=$2
osmium=${OSMIUM:-osmium}
name=$(basename "$in" .osm.pbf)
mkdir -p "$out"

$osmium tags-filter "$in" \
  nw/addr:housenumber w/addr:interpolation w/highway n/place \
  -o "$out/$name.streets-raw.pbf" --overwrite
$osmium add-locations-to-ways "$out/$name.streets-raw.pbf" \
  -o "$out/$name.streets.pbf.tmp" -f pbf --overwrite
mv "$out/$name.streets.pbf.tmp" "$out/$name.streets.pbf"
rm "$out/$name.streets-raw.pbf"

$osmium tags-filter "$in" \
  wr/boundary=administrative,postal_code wr/place \
  -o "$out/$name.areas-raw.pbf" --overwrite
$osmium export "$out/$name.areas-raw.pbf" \
  --geometry-types=polygon -f geojsonseq --attributes=type,id \
  -o "$out/$name.areas.geojsonseq.tmp" --overwrite
mv "$out/$name.areas.geojsonseq.tmp" "$out/$name.areas.geojsonseq"
rm "$out/$name.areas-raw.pbf"

$osmium fileinfo -g header.option.osmosis_replication_timestamp "$in" > "$out/$name.date" || true
