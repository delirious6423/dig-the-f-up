# Power benchmark: watcher, power policy and resident model

Status: kit written, **not yet run**. The measurement needs `sudo` (`powermetrics`), so the owner runs it in a
Terminal window. Nothing in this file is a measured result until a table below is filled from a real run.

Script: `scripts/bench-power.sh` (POSIX-style bash, macOS tools plus awk, no dependencies).

## What it measures

For each scenario it samples `sudo powermetrics --samplers cpu_power,gpu_power -i 5000 -n SAMPLES` (one sample
every 5 s) and records `pmset -g batt` and `uptime` before and after:

| Scenario | What happens during the window |
|---|---|
| `idle` | DigUp is open with the watcher on; nothing changes in the watched folders |
| `edits` | 20 small text files are written in `EDIT_DIR`, then each gets one more line, spread evenly over the window |
| `pdf` | one large PDF (`PDF_SRC`) is copied into `EDIT_DIR` and indexed |

Output per run: raw `<scenario>.powermetrics.txt`, battery and load snapshots, and one row per scenario in
`summary.csv` (average CPU mW, GPU mW, combined mW, battery % before and after, power source, 1-minute load before
and after).

## Before you start

1. Quit other heavy apps (builds, browsers with many tabs, sync clients). Note the load: `uptime`. The load average
   is a confound: other processes add CPU power. Mark any run with load above about 4 as loaded.
2. Pick one folder inside a watched root for the edits and PDF scenarios, for example
   `~/Documents/dtfu-power`. It must be inside the folder DigUp watches, or the watcher will not react.
3. Pick one large PDF, 50 MB or more if you can, for example `~/Downloads/big.pdf`. Copy it nowhere; the script
   copies it itself.
4. Make sure the index is idle: wait until DigUp shows no indexing activity before starting.

## Steps

Run one configuration at a time. Each full run is about 6 minutes (3 scenarios x 2 min, plus the prompts).

```bash
cd ~/Claude/dig-the-f-up        # or wherever the repo is
sudo EDIT_DIR="$HOME/Documents/dtfu-power" PDF_SRC="$HOME/Downloads/big.pdf" \
  scripts/bench-power.sh "$HOME/dtfu-power/<config-name>" "<label>" 24
```

- `OUT_DIR`: a new folder per configuration, for example `~/dtfu-power/watch-on-plugged-resident`.
- `LABEL`: the configuration in words, for example `watcher=on policy=plugged_in model=resident`.
- `SAMPLES`: 24 gives 2 minutes per scenario (the default). Use 48 for a longer, steadier average.
- The script waits for Enter before each scenario, so you can set the app up first.
- Run only one scenario with `SCENARIOS=idle` (or `edits`, or `pdf`) in front of the command.

Outputs: `summary.csv` in `OUT_DIR` (appended to on every run in that folder) and a short table on stdout.

## Configurations to compare

Three factors. Run the full set for each row you care about; the minimum useful comparison is the first two rows.

| # | Watcher | Power policy | Model | Purpose |
|---|---|---|---|---|
| 1 | off | plugged_in | not resident | baseline (no background work) |
| 2 | on | plugged_in | resident | cost of the watcher with the model loaded and AC power |
| 3 | on | live | resident | what the watcher costs on battery under the live policy |
| 4 | on | plugged_in | not resident | cost of keeping the model in memory versus loading it per query. **No switch exists yet**: DigUp has no setting that unloads the model between queries. Skip this row unless one is added, and then record the change in the label |

"Power policy" is the DigUp watch policy stored in the settings file as `live`, `plugged_in` or `manual`
(`crates/dtfu-engine/src/lib.rs`). `plugged_in` indexes only on AC power, `live` whenever the Mac is awake. Set it in
the app before each run. `manual` is not in the matrix: the watcher does nothing on its own, so it is the same as
"watcher off" for this purpose. Keep the Mac on AC power for the `plugged_in` runs and on
battery for the `live` runs, and record which in the label.

Rule for reading the results: compare only runs with a similar load average (within about 2 of each other). A run
with a higher load average is not comparable, even if the configuration matches.

## Table template

Fill one row per scenario and configuration from `summary.csv`. Use averages over the samples; do not compare
battery percentages across runs unless they started at the same level.

| Config | Scenario | Samples | CPU mW (avg) | GPU mW (avg) | Combined mW (avg) | Battery before -> after | Source | Load before -> after |
|---|---|---|---|---|---|---|---|---|
| 1 watcher off, plugged_in, not resident | idle | | | | | | | |
| 1 | edits | | | | | | | |
| 1 | pdf | | | | | | | |
| 2 watcher on, plugged_in, resident | idle | | | | | | | |
| 2 | edits | | | | | | | |
| 2 | pdf | | | | | | | |
| 3 watcher on, live, resident | idle | | | | | | | |
| 3 | edits | | | | | | | |
| 3 | pdf | | | | | | | |
| 4 watcher on, plugged_in, not resident | idle | | | | | | | |
| 4 | edits | | | | | | | |
| 4 | pdf | | | | | | | |

Write the result up in `docs/BENCHMARKS.md` as a new dated section, with the machine (`sysctl -n hw.model
hw.memsize`), the exact command, the load averages and this table. Label any run that had load at or above 4 as
loaded.

## Checks before you trust a row

- `<scenario>.powermetrics.txt` contains `CPU Power:` and `GPU Power:` lines. If the summary shows `n/a`, open the
  raw file: the powermetrics output format changes between macOS versions, so the parser may need a new pattern.
- The `pdf` scenario actually indexed the PDF: check the DigUp status during the window. A PDF that finishes in
  the first 30 seconds makes the window mostly idle.
- The `edits` scenario changed the files: `EDIT_DIR` held 20 `dtfu-power-bench-*.txt` files while it ran.
- Battery percentage barely moves in 2 minutes. Read the power source and the CPU/GPU figures, not the battery.
- The script removes only files it created (`dtfu-power-bench-*`) and the copied PDF. Check `EDIT_DIR` afterwards.

## Verdict

Not yet measured. Fill in after the owner's run.
