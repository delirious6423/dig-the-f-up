#!/bin/bash
# bench-power.sh: CPU/GPU power (powermetrics) and battery state for three DigUp scenarios.
#
# Run it yourself in a Terminal window (powermetrics needs root):
#   sudo scripts/bench-power.sh OUT_DIR [LABEL] [SAMPLES]
# Full procedure, comparison matrix and table template: docs/POWER-BENCHMARK.md
#
# Scenarios (run in this order unless SCENARIOS says otherwise):
#   idle   the app is open with the watcher on and nothing changes in the watched folders
#   edits  20 small text edits, one every SAMPLES*5/20 seconds, inside EDIT_DIR
#   pdf    one large PDF copied into EDIT_DIR, so the index works on it
# Each scenario runs `sudo powermetrics --samplers cpu_power,gpu_power -i 5000 -n SAMPLES` and `pmset -g batt`
# before and after. The script only uses macOS tools plus awk.
#
# Output in OUT_DIR (created if missing; use one folder per configuration):
#   <scenario>.powermetrics.txt  raw powermetrics output (check it if a value shows n/a)
#   <scenario>.batt-before.txt / <scenario>.batt-after.txt, <scenario>.uptime-before.txt / -after.txt
#   summary.csv                  one row per scenario, appended on every run (header written once)
# and a short summary table on stdout.

set -u

usage() {
    cat >&2 <<'EOF'
usage: sudo scripts/bench-power.sh OUT_DIR [LABEL] [SAMPLES]
  OUT_DIR  folder for the output files (created if missing)
  LABEL    free text stored in each row, e.g. "watcher=on policy=plugged_in model=resident" (default: run)
  SAMPLES  powermetrics samples per scenario, 5 s each (default 24, i.e. 2 min per scenario)
Environment:
  EDIT_DIR    folder inside a watched root, for the edits and pdf scenarios (required for those two)
  PDF_SRC     large PDF to copy into EDIT_DIR for the pdf scenario (required for that scenario)
  SCENARIOS   space-separated subset of: idle edits pdf (default: "idle edits pdf")
Example:
  sudo EDIT_DIR="$HOME/Documents/digup-bench" PDF_SRC="$HOME/Downloads/big.pdf" \
    scripts/bench-power.sh "$HOME/digup-power/watch-on-plugged" "watcher=on policy=plugged_in model=resident" 24
EOF
    exit 2
}

[ $# -ge 1 ] || usage
case "$1" in -h|--help) usage ;; esac
OUT_DIR=$1
LABEL=${2:-run}
SAMPLES=${3:-24}
SCENARIOS=${SCENARIOS:-idle edits pdf}
EDIT_DIR=${EDIT_DIR:-}
PDF_SRC=${PDF_SRC:-}

case "$SAMPLES" in ''|*[!0-9]*) echo "SAMPLES must be a positive integer" >&2; exit 2 ;; esac
[ "$SAMPLES" -ge 4 ] || { echo "SAMPLES must be at least 4" >&2; exit 2; }

if [ "$(id -u)" -ne 0 ]; then
    echo "powermetrics needs root. Run: sudo $0 $*" >&2
    exit 1
fi
for tool in powermetrics pmset uptime awk seq; do
    command -v "$tool" >/dev/null 2>&1 || { echo "missing macOS tool: $tool" >&2; exit 1; }
done

for s in $SCENARIOS; do
    case "$s" in
        idle) ;;
        edits) [ -n "$EDIT_DIR" ] && [ -d "$EDIT_DIR" ] || { echo "scenario edits needs EDIT_DIR (existing folder)" >&2; exit 2; } ;;
        pdf)
            [ -n "$EDIT_DIR" ] && [ -d "$EDIT_DIR" ] || { echo "scenario pdf needs EDIT_DIR (existing folder)" >&2; exit 2; }
            [ -f "$PDF_SRC" ] || { echo "scenario pdf needs PDF_SRC (a PDF file)" >&2; exit 2; }
            ;;
        *) echo "unknown scenario: $s" >&2; exit 2 ;;
    esac
done

mkdir -p "$OUT_DIR" || exit 1
SUMMARY="$OUT_DIR/summary.csv"
if [ ! -f "$SUMMARY" ]; then
    echo "label,scenario,samples,cpu_mw_avg,gpu_mw_avg,combined_mw_avg,batt_before,batt_after,source_after,load1_before,load1_after,started" > "$SUMMARY"
fi

# Removes only the files this script creates (prefix digup-power-bench-).
cleanup_files() {
    [ -n "$EDIT_DIR" ] && rm -f "$EDIT_DIR"/digup-power-bench-* 2>/dev/null
}
trap 'cleanup_files; echo; echo "interrupted; bench files removed" >&2; exit 130' INT TERM

# Power source and 1-minute load from pmset and uptime.
batt_src() { pmset -g batt | head -1 | sed -e "s/.*'\(.*\)'.*/\1/"; }
load1() { uptime | awk -F'load averages?: ' '{split($2, a, /[ ,]+/); print a[1]}'; }

# Average CPU, GPU and combined mW from a powermetrics log. Prints "cpu gpu combined" or n/a.
summarise() {
    awk '
        /^CPU Power:/ { cpu += $3; nc++ }
        /^GPU Power:/ { gpu += $3; ng++ }
        /^Combined Power/ { comb += $(NF-1); nk++ }
        END {
            printf "%s %s %s\n",
                (nc ? sprintf("%.1f", cpu / nc) : "n/a"),
                (ng ? sprintf("%.1f", gpu / ng) : "n/a"),
                (nk ? sprintf("%.1f", comb / nk) : "n/a")
        }
    ' "$1"
}

# Writes 20 new files in EDIT_DIR, then appends one line to each, evenly spread over the window.
run_edits() {
    window=$1
    gap=$(( window / 20 ))
    [ "$gap" -ge 1 ] || gap=1
    i=1
    while [ "$i" -le 20 ]; do
        printf 'digup power bench, file %02d, initial line\n' "$i" > "$EDIT_DIR/digup-power-bench-$(printf %02d "$i").txt"
        i=$(( i + 1 ))
    done
    sleep 2
    i=1
    while [ "$i" -le 20 ]; do
        printf 'edit %02d at %s\n' "$i" "$(date +%H:%M:%S)" >> "$EDIT_DIR/digup-power-bench-$(printf %02d "$i").txt"
        sleep "$gap"
        i=$(( i + 1 ))
    done
}

run_scenario() {
    s=$1
    window=$(( SAMPLES * 5 ))
    echo
    echo "== scenario: $s ($LABEL, $SAMPLES samples = ${window} s) =="
    case "$s" in
        idle)  echo "Leave DigUp open with the watcher on. Do not touch the Mac or the watched folders." ;;
        edits) echo "The script writes 20 small text files in $EDIT_DIR, then edits each one during the window." ;;
        pdf)   echo "The script copies $PDF_SRC into $EDIT_DIR during the window, then removes it." ;;
    esac
    printf 'Press Enter to start the scenario (Ctrl-C to abort): '
    read -r _

    uptime > "$OUT_DIR/$s.uptime-before.txt"
    pmset -g batt > "$OUT_DIR/$s.batt-before.txt"
    started=$(date '+%Y-%m-%d %H:%M:%S')
    load_before=$(load1)

    case "$s" in
        idle)  : ;;
        edits)
            # Files are created at the start, so the watcher's first index pass is part of the window.
            run_edits "$window" &
            ;;
        pdf)
            ( sleep 5; cp "$PDF_SRC" "$EDIT_DIR/digup-power-bench-large.pdf" ) &
            ;;
    esac
    powermetrics --samplers cpu_power,gpu_power -i 5000 -n "$SAMPLES" > "$OUT_DIR/$s.powermetrics.txt" 2> "$OUT_DIR/$s.powermetrics.stderr.txt"
    rc=$?
    wait
    [ "$s" = pdf ] && rm -f "$EDIT_DIR/digup-power-bench-large.pdf"
    [ "$s" = edits ] && rm -f "$EDIT_DIR"/digup-power-bench-*.txt

    uptime > "$OUT_DIR/$s.uptime-after.txt"
    pmset -g batt > "$OUT_DIR/$s.batt-after.txt"
    load_after=$(load1)
    if [ "$rc" -ne 0 ]; then
        echo "powermetrics exited with $rc, see $OUT_DIR/$s.powermetrics.stderr.txt" >&2
    fi

    set -- $(summarise "$OUT_DIR/$s.powermetrics.txt")
    cpu=$1; gpu=$2; comb=$3
    bb=$(awk -v f="$OUT_DIR/$s.batt-before.txt" 'BEGIN{ while ((getline l < f) > 0) if (match(l, /[0-9]+%/)) { print substr(l, RSTART, RLENGTH); exit } }')
    ba=$(awk -v f="$OUT_DIR/$s.batt-after.txt" 'BEGIN{ while ((getline l < f) > 0) if (match(l, /[0-9]+%/)) { print substr(l, RSTART, RLENGTH); exit } }')
    src=$(batt_src)
    printf '%s,%s,%s,%s,%s,%s,%s,%s,"%s",%s,%s,%s\n' \
        "$LABEL" "$s" "$SAMPLES" "$cpu" "$gpu" "$comb" "$bb" "$ba" "$src" "$load_before" "$load_after" "$started" >> "$SUMMARY"
    SCEN_ROWS="${SCEN_ROWS:-}$s cpu=${cpu} mW gpu=${gpu} mW combined=${comb} mW batt=${bb:-?}->${ba:-?} src=${src} load1=${load_before}->${load_after}
"
}

SCEN_ROWS=""
for s in $SCENARIOS; do
    run_scenario "$s"
done
trap - INT TERM

echo
echo "Summary for \"$LABEL\" (rows appended to $SUMMARY):"
printf '%s' "$SCEN_ROWS"
echo "Averages are over the powermetrics samples. Load is the 1-minute load average before and after each scenario."
echo "A high load raises CPU power from other apps: compare only runs with similar load, or mark them."
