#!/bin/bash
# Regenerates the screenshots used by the module documentation of perf-viz (docs/overview.md).
#
#   docs/screenshots.sh [BIG_RECORDING] [SMALL_RECORDING]
#
# Needs the `scripted` feature (screenshots and scripted clicks) and a recording big enough that
# loading takes a few seconds. Window coordinates are in points: 1500x900 window, scale factor 1.333.
set -euo pipefail
cd "$(dirname "$0")/../.."
BIG=${1:-perf-large.data}
OUT=viz/docs/images
RAW=$(mktemp -d)
CFGDIR=$(mktemp -d)
CFG=$CFGDIR/config.toml
FRESH=$CFGDIR/fresh.toml   # no settings yet: the first-run question is asked
printf 'theme = "dark"\ntuning_prompted = true\n' > "$CFG"
printf 'theme = "dark"\n' > "$FRESH"
export PERF_VIZ_CONFIG=$CFG
# Every run gets a time limit, so a stuck window can never hang this script.
LIMIT=${LIMIT:-300}
run() { timeout "$LIMIT" "$VIZ" "$@"; }
cargo build --release -q -p perf-viz --features scripted
VIZ=target/release/perf-viz
evict() { python3 -c "import os,sys;fd=os.open(sys.argv[1],os.O_RDONLY);os.posix_fadvise(fd,0,0,os.POSIX_FADV_DONTNEED)" "$BIG"; }

# 1. Loading: the file is cold, so the progress bar is visible for a few seconds.
evict
run "$BIG" --size 1300x700 --load-screenshot "$RAW/loading.png" 4

# 1b. The first-run question.
PERF_VIZ_CONFIG=$FRESH run "$BIG" --size 1300x700 --screenshot "$RAW/prompt.png"

# 2. Summary, with the read speed.
run "$BIG" --size 1500x900 --tab summary --screenshot "$RAW/summary.png"

# 3. Flame graph.
run "$BIG" --size 1500x900 --tab flame --screenshot "$RAW/flame.png"

# 4. The tuner. Its window is embedded in the main one so the screenshot includes it; the run is
#    shortened (scale 0.3) and starts by itself, and the window is opened by a scripted step
#    rather than by clicking for it, so no coordinates have to be right.
PERF_VIZ_EMBED_VIEWPORTS=1 PERF_VIZ_TUNE_AUTOSTART=1 PERF_VIZ_TUNE_SCALE=0.3 \
  run "$BIG" --size 1500x900 --tab summary --tune-window --pause "${TUNE_WAIT:-150}" --screenshot "$RAW/tuning.png"

# Shrink for the documentation: 1100 px wide, 128-colour palette.
python3 - "$RAW" "$OUT" <<'PY'
import sys, os
from PIL import Image
raw, out = sys.argv[1:3]
for name in sorted(os.listdir(raw)):
    im = Image.open(os.path.join(raw, name)).convert("RGB")
    if name in ("loading.png", "prompt.png"):
        # Mostly empty windows: keep the middle, where the content is.
        cx, cy = im.width // 2, im.height // 2
        im = im.crop((cx - 450, cy - 200, cx + 450, cy + 200))
    w = min(1100, im.width)
    im = im.resize((w, round(im.height * w / im.width)), Image.LANCZOS)
    im = im.quantize(colors=128, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE)
    im.save(os.path.join(out, name), optimize=True)
    print(name, os.path.getsize(os.path.join(out, name)) // 1024, "KiB")
PY
rm -rf "$RAW"

# Inline the images into the markdown that main.rs includes as its documentation.
python3 viz/docs/embed.py
