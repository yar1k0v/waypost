#!/bin/sh
# Runs osmium from the builder image with ./data mounted at /data, for local
# builds without osmium-tool installed. Build the image first: docker compose build index
# Paths passed to it must be relative to the repo root, for example data/x.osm.pbf.
export MSYS_NO_PATHCONV=1
exec docker run --rm --user "$(id -u):$(id -g)" --entrypoint osmium \
    -v "$(pwd -W 2>/dev/null || pwd)/data:/data" -w / waypost-builder:local "$@"
