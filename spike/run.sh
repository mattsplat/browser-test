#!/bin/sh
# Render every prepared page with both engines. Requests to BLOCK_HOST are refused,
# to check that each engine's network hook really is a chokepoint.
# usage: ./run.sh [blitz|servo|gosub]   (default: blitz servo)
cd "$(dirname "$0")"
BLOCK_HOST=upload.wikimedia.org

# Gosub has no embedder hook for blocking requests, so it runs its own screenshot tool
# on the same JS-off pages Chrome gets (pages-ref/, with a <base> tag), unmodified.
if [ "$1" = gosub ]; then
  mkdir -p out/gosub
  bin=../vendor/gosub/target/release/gosub-screenshot
  for page in pages-ref/*.html; do
    name=$(basename "$page" .html)
    printf "%-6s %-12s " gosub "$name"
    start=$(python3 -c 'import time;print(time.time())')
    "$bin" "file://$PWD/$page" "out/gosub/$name.png" 1280 --min-height 1400 --nav-timeout 30 --render-timeout 60 > "out/gosub/$name.log" 2>&1
    code=$?
    echo "exit=$code ELAPSED_MS $(python3 -c "import time;print(int((time.time()-$start)*1000))")"
  done
  exit 0
fi

for engine in ${1:-blitz servo}; do
  bin="./$engine-shot/target/release/$engine-shot"
  for page in pages/*.html; do
    name=$(basename "$page" .html)
    url=$(cat "pages/$name.url")
    printf "%-6s %-12s " "$engine" "$name"
    "$bin" "$page" "$url" "out/$engine/$name.png" "$BLOCK_HOST" > "out/$engine/$name.log" 2>&1
    echo "exit=$? $(grep -c ALLOWED "out/$engine/$name.log") allowed, $(grep -c BLOCKED "out/$engine/$name.log") blocked, $(grep ELAPSED_MS "out/$engine/$name.log")"
  done
done
