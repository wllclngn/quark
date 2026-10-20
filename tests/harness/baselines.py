"""The scoreboard's archive and montauk's verdict over it.

Every going-to-PROD run leaves one .prom per run in ARCHIVE_DIR, each holding
that run's raw value for every cell, so montauk counts runs, not medians.
montauk judges the archive: `--pairs vs-best` calls each version best,
tied or not-best per cell, and `--trajectory` scans the version order for a
slow decline. Its report goes into scoreboard.md as it is; the Proton cells
sit beside quark's, so a box that drifted reads as drift. quark judges
nothing itself.
"""

import subprocess

from quark_common import CACHE_DIR, PrometheusBuilder

ARCHIVE_DIR = CACHE_DIR / "archive"

# A rate reads better higher, so montauk judges it in its own pass.
RATES = {"fps_1pct_low"}


def metric_name(column: str) -> str:
    return f"rate_{column}" if column in RATES else f"cell_{column}"


def archive_runs(cell_runs: dict, columns, helps: dict, version: str, commit: str, stamp: str) -> list:
    """One .prom per run index: Run k of every cell, labelled by game, tool
    and scheduler. `cell_runs` maps (appid, tool, scheduler) to its runs."""
    ARCHIVE_DIR.mkdir(parents=True, exist_ok=True)
    written = []
    for k in range(max((len(r) for r in cell_runs.values()), default=0)):
        pb = PrometheusBuilder("scoreboard")
        pb.info(version=version, git_commit=commit.removesuffix("-dirty"),
                git_dirty=commit.endswith("-dirty"))
        for (appid, tool, sched), runs in cell_runs.items():
            if k >= len(runs):
                continue
            labels = {"game": appid, "tool": tool, "scheduler": sched}
            for c in columns:
                if runs[k].get(c) is not None:
                    pb.gauge(metric_name(c), runs[k][c], help=helps[c], labels=labels)
        path = ARCHIVE_DIR / f"{version}-{stamp}-{k + 1}.prom"
        path.write_text(pb.render())
        written.append(path)
    return written


def _analyze(*args: str) -> str:
    r = subprocess.run(["montauk", "--analyze", str(ARCHIVE_DIR), "--by", "version", "--no-emit", *args],
                       capture_output=True, text=True)
    return (r.stderr + r.stdout).strip()


def render_md(rates: bool) -> str:
    """montauk's verdicts over the archive, verbatim; the rates pass only when
    the runs carried a rate (frame times)."""
    out = ["## Baselines", "",
           f"montauk over every archived run in `{ARCHIVE_DIR}`, by version: Each version "
           "best, tied or not-best per cell (vs-best), then the version-ordered "
           "change-point scan (trajectory). Read a quark cell beside its Proton cell; "
           "both moving together is the box, not quark.", ""]
    passes = [("Against the best version", ("--metric", "scoreboard_cell_", "--pairs", "vs-best"))]
    if rates:
        passes.append(("Rates, higher better",
                       ("--metric", "scoreboard_rate_", "--pairs", "vs-best", "--higher-better")))
    passes.append(("Trajectory", ("--metric", "scoreboard_cell_", "--trajectory")))
    for title, args in passes:
        out += [f"### {title}", "", "```", _analyze(*args), "```", ""]
    return "\n".join(out) + "\n"
