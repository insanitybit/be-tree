#!/usr/bin/env python3
"""Turn Cachegrind summary lines into stable per-operation TSV.

Every workload subtracts an identical setup-only process so process startup and
fixture construction are not attributed to the operation under measurement.
"""

from pathlib import Path
from statistics import median
import sys

EVENTS = ["Ir", "I1mr", "ILmr", "Dr", "D1mr", "DLmr", "Dw", "D1mw", "DLmw", "Bc", "Bcm", "Bi", "Bim"]
RUNS = {
    "get-many": 100,
    "scan": 2,
    "scan-stream": 2,
    "apply": 25,
    "hash": 1000,
    "decode": 1000,
}


def read(path: Path) -> dict[str, int]:
    events = None
    summary = None
    for line in path.read_text().splitlines():
        if line.startswith("events: "):
            events = line.split()[1:]
        elif line.startswith("summary: "):
            summary = [int(value) for value in line.split()[1:]]
    if events is None or summary is None:
        raise SystemExit(f"missing Cachegrind summary in {path}")
    return dict(zip(events, summary, strict=True))


directory = Path(sys.argv[1])
repeats = sorted(
    int(path.name.split(".")[1]) for path in directory.glob("setup.*.cachegrind")
)
if not repeats:
    raise SystemExit(f"no repeated Cachegrind runs in {directory}")
baseline_names = {
    "get-many": "setup-get-many",
    "scan": "setup",
    "scan-stream": "setup",
    "apply": "setup",
    "hash": "setup-hash",
    "decode": "setup-decode",
}
rows = []
variability = []
for scenario, iterations in RUNS.items():
    samples = []
    for repeat in repeats:
        values = read(directory / f"{scenario}.{repeat}.cachegrind")
        baseline = read(directory / f"{baseline_names[scenario]}.{repeat}.cachegrind")
        samples.append(
            {event: (values[event] - baseline[event]) / iterations for event in EVENTS}
        )
    medians = {event: median(sample[event] for sample in samples) for event in EVENTS}
    rows.append((scenario, iterations, len(repeats), *(medians[event] for event in EVENTS)))
    for event in ("Ir", "D1mr", "D1mw", "Bcm"):
        values = [sample[event] for sample in samples]
        variability.append((scenario, event, min(values), medians[event], max(values)))

header = ("scenario", "iterations", "repeats", *EVENTS)
lines = ["\t".join(header)]
for row in rows:
    lines.append(
        "\t".join(
            (row[0], str(row[1]), str(row[2]), *(f"{value:.2f}" for value in row[3:]))
        )
    )
result = "\n".join(lines) + "\n"
(directory / "cachegrind.tsv").write_text(result)
print(result, end="")

variation_lines = ["scenario\tevent\tmin\tmedian\tmax"]
variation_lines.extend(
    f"{scenario}\t{event}\t{low:.2f}\t{middle:.2f}\t{high:.2f}"
    for scenario, event, low, middle, high in variability
)
(directory / "cachegrind-variability.tsv").write_text("\n".join(variation_lines) + "\n")
