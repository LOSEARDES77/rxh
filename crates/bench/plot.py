#!/usr/bin/env python3
"""Draws charts from an `rxh-bench --json` file.

Usage: python3 crates/bench/plot.py docs/bench/results.json docs/bench

Writes `rps.svg` (requests per second) and `p99.svg` (99th percentile
latency), one group of bars per scenario and one bar per proxy. Needs
matplotlib.
"""

import json
import sys
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

# Color follows the proxy, never its rank. Validated for color blindness.
COLORS = {"rxh": "#2a78d6", "caddy": "#eb6834", "nginx": "#1baf7a", "traefik": "#eda100"}
SURFACE, INK, MUTED, GRID, AXIS = "#fcfcfb", "#0b0b0b", "#52514e", "#e1e0d9", "#c3c2b7"


# Not requests of a small GET: 1 MiB bodies and idle memory would flatten the other bars.
# Their numbers are in the tables of docs/benchmarks.md.
SKIP = {"large-body", "memory"}


def chart(outcomes, value, title, unit, path):
    outcomes = [o for o in outcomes if o["scenario"] not in SKIP]
    scenarios = list(dict.fromkeys(o["scenario"] for o in outcomes))
    targets = [t for t in COLORS if any(o["target"] == t and o["stats"] for o in outcomes)]
    values = {(o["scenario"], o["target"]): value(o["stats"]) for o in outcomes if o["stats"]}

    figure, axes = plt.subplots(figsize=(14, 4.6), facecolor=SURFACE)
    axes.set_facecolor(SURFACE)
    width = 0.8 / len(targets)
    for index, target in enumerate(targets):
        xs = [x + index * width for x, s in enumerate(scenarios) if (s, target) in values]
        ys = [values[s, target] for s in scenarios if (s, target) in values]
        # The edge in the surface color leaves a gap between neighbours.
        axes.bar(xs, ys, width, label=target, color=COLORS[target], edgecolor=SURFACE, linewidth=2)
    # A proxy that cannot run a scenario gets a mark instead of a silent hole.
    for x, scenario in enumerate(scenarios):
        for index, target in enumerate(targets):
            if (scenario, target) not in values:
                axes.text(x + index * width, 0, "n/a", ha="center", va="bottom", fontsize=7,
                          color=MUTED, rotation=90)

    axes.set_xticks([x + width * (len(targets) - 1) / 2 for x in range(len(scenarios))])
    axes.set_xticklabels(scenarios, color=MUTED, fontsize=9)
    axes.tick_params(axis="both", length=0, labelcolor=MUTED, labelsize=9)
    axes.yaxis.grid(True, color=GRID, linewidth=0.8)
    axes.set_axisbelow(True)
    for side in ("top", "right", "left"):
        axes.spines[side].set_visible(False)
    axes.spines["bottom"].set_color(AXIS)
    axes.set_ylabel(unit, color=MUTED, fontsize=9)
    axes.set_title(title, color=INK, fontsize=12, loc="left", pad=28)
    axes.legend(ncols=len(targets), frameon=False, loc="lower left", bbox_to_anchor=(0, 1.0),
                labelcolor=MUTED, fontsize=9, handlelength=1, handleheight=1)
    figure.tight_layout()
    figure.savefig(path, facecolor=SURFACE)
    plt.close(figure)


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    outcomes = json.loads(Path(sys.argv[1]).read_text())
    out = Path(sys.argv[2])
    out.mkdir(parents=True, exist_ok=True)
    chart(outcomes, lambda s: s["rps"] / 1000, "Requests per second (higher is better)",
          "thousand requests / s", out / "rps.svg")
    chart(outcomes, lambda s: s["p99_us"] / 1000, "p99 latency (lower is better)",
          "milliseconds", out / "p99.svg")


if __name__ == "__main__":
    main()
