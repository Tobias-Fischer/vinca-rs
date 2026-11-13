#!/usr/bin/env bash
# vinca-rs vs vinca (Python) on RoboStack's humble, with hyperfine.
#
#   benchmarks/run.sh <robostack checkout> [vinca-rs binary]
#
# The checkout's pixi environment provides Python vinca (vinca, vinca-gha,
# vinca-snapshot) and hyperfine. Every command does its real work, network
# included (rosdistro cache, package.xml files, repodata), as it does in CI.
set -euo pipefail
ROBOSTACK=$(cd "$1" && pwd)
VINCA_RS=$(cd "$(dirname "${2:-target/release/vinca-rs}")" && pwd)/$(basename "${2:-target/release/vinca-rs}")
OUT=$(cd "$(dirname "$0")" && pwd)/results
mkdir -p "$OUT"
PIXI=(pixi run --manifest-path "$ROBOSTACK/pixi.toml")
HF=(pixi exec hyperfine --shell=default --style basic)

cd "$ROBOSTACK"
"${PIXI[@]}" python tools/robostack.py humble prepare >/dev/null
D=$ROBOSTACK/distros/humble
# all recipes (no skip_existing) and the CI configuration (with it)
for w in bench-all bench-skip; do
  rm -rf "$D/$w"; mkdir -p "$D/$w"
  cp "$D/work/conda_build_config.yaml" "$D/$w/"
done
sed '/^skip_existing:/,$d' "$D/work/vinca.yaml" > "$D/bench-all/vinca.yaml"
cp "$D/work/vinca.yaml" "$D/bench-skip/vinca.yaml"

"${HF[@]}" --warmup 1 --runs 5 --export-markdown "$OUT/generate.md" \
  -n "vinca -m" --prepare "rm -rf $D/bench-all/recipes && mkdir $D/bench-all/recipes" \
  "cd $D/bench-all && ${PIXI[*]} vinca -m --platform linux-64" \
  -n "vinca-rs generate --all" \
  "cd $D/bench-all && $VINCA_RS generate --platform linux-64 --all --out recipes"

"${HF[@]}" --warmup 1 --runs 5 --export-markdown "$OUT/skip-existing.md" \
  -n "vinca -m (skip_existing)" --prepare "rm -rf $D/bench-skip/recipes && mkdir $D/bench-skip/recipes" \
  "cd $D/bench-skip && ${PIXI[*]} vinca -m --platform linux-64" \
  -n "vinca-rs generate (skip_existing)" \
  "cd $D/bench-skip && $VINCA_RS generate --platform linux-64 --out recipes"

# the workflow of a full rebuild, from the same recipes
(cd "$D/bench-all" && $VINCA_RS generate --platform linux-64 --all --out recipes >/dev/null 2>&1)
mkdir -p "$D/bench-all/.scripts"
"${HF[@]}" --warmup 1 --runs 5 --export-markdown "$OUT/workflow.md" \
  -n "vinca-gha" "cd $D/bench-all && ${PIXI[*]} vinca-gha --platform linux-64 --trigger-branch b -d ./recipes --batch_size 25" \
  -n "vinca-rs workflow" "cd $D/bench-all && $VINCA_RS workflow --platform linux-64 --recipes recipes --json /dev/null"

"${HF[@]}" --runs 2 --export-markdown "$OUT/snapshot.md" \
  -n "vinca-snapshot" "cd $D/bench-all && ${PIXI[*]} vinca-snapshot -d humble --rosdistro-ref humble/2026-10-12 -o snap-py.yaml -q" \
  -n "vinca-rs snapshot" "cd $D/bench-all && $VINCA_RS snapshot -d humble --rosdistro-ref humble/2026-10-12 -o snap-rs.yaml"

rm -rf "$D/bench-all" "$D/bench-skip"
