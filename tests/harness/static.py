"""The going-to-PROD static pass: montauk's static lane over quark.

`montauk --static` lexes the launcher, the installer and the suite into an
artifact of branches, basic blocks, edges, declarations and references;
`montauk --analyze --json` reads it as three reports: guards (the lexer's
record of every branch and the conditions that reach it), cfg (each
function's control flow graph) and declarations (the static analyzer's
reachability from entry points). This module only calls the CLI and renders
its JSON. A block unreachable from entry fails the release.
"""

import json
import re
import shutil
import subprocess
from pathlib import Path

from quark_common import REPO_ROOT, SUITE_DIR, log_error, log_info

TESTS = REPO_ROOT / "tests"
ARTIFACT = SUITE_DIR / "quark.guards"


def scanned() -> list[Path]:
    """The launcher, the installer and the suite; the research tree is not quark's."""
    suite = sorted(p for p in TESTS.iterdir()
                   if p.name not in ("research", "__pycache__") and (p.is_dir() or p.suffix == ".py"))
    return [REPO_ROOT / "rust" / "src", REPO_ROOT / "install.py", REPO_ROOT / "quark_common.py", *suite]


def _where(item: dict) -> str:
    """`file:line`, the file relative to the repo; a guards row names no file."""
    line = item.get("line")
    if "file" not in item:
        return f"line {line}" if line is not None else ""
    path = Path(item["file"])
    rel = path.relative_to(REPO_ROOT) if path.is_relative_to(REPO_ROOT) else path
    return f"{rel}:{line}" if line is not None else str(rel)


def analyze() -> dict | None:
    """The three reports by name, or None when the lane could not run."""
    if not shutil.which("montauk"):
        log_error("static: montauk is not on PATH")
        return None
    SUITE_DIR.mkdir(parents=True, exist_ok=True)
    for args in (["--static", *map(str, scanned()), "-o", str(ARTIFACT)],
                 ["--analyze", str(ARTIFACT), "--json"]):
        r = subprocess.run(["montauk", *args], capture_output=True, text=True)
        if r.returncode != 0:
            lines = r.stderr.strip().splitlines()
            log_error(f"static: montauk {args[0]}: {lines[-1] if lines else f'exit {r.returncode}'}")
            return None
    return {rep["name"]: rep for rep in json.loads(r.stdout)["reports"]}


def judge(reports: dict) -> int:
    """One status line per report; an unreachable block is the verdict."""
    log_info(f"STATIC LEXER: {reports['guards']['verdict']}")
    log_info(f"STATIC CFG: {reports['cfg']['verdict']}")
    log_info(f"STATIC DECLARATIONS: {reports['declarations']['verdict']}")
    unreachable = reports["cfg"].get("unreachable", [])
    for u in unreachable:
        log_error(f"STATIC: {_where(u)} in {u['function']}: {u['kind']} unreachable from entry")
    return 1 if unreachable else 0


def render_md(reports: dict) -> str:
    g, c, d = reports["guards"], reports["cfg"], reports["declarations"]
    out = ["## Static analysis", "",
           "`montauk --static` over rust/src, install.py, quark_common.py and tests/, read with "
           f"`montauk --analyze`; the artifact is `{ARTIFACT}`.", "",
           "### Lexer: guards", "", g["verdict"], "",
           "| Depth | Function | Where | Guard |", "|---|---|---|---|"]
    out += [f"| {x['depth']} | {x['function']} | {_where(x)} | `{x['guard']}` |" for x in g.get("deepest", [])]
    out += ["", "### Control flow graph", "", c["verdict"], ""]
    if c.get("unreachable"):
        out += ["Unreachable from entry:", "", "| Function | Where | Kind |", "|---|---|---|"]
        out += [f"| {x['function']} | {_where(x)} | {x['kind']} |" for x in c["unreachable"]]
        out.append("")
    out += ["| Most complex | Where | Cyclomatic | Blocks |", "|---|---|---|---|"]
    out += [f"| {x['function']} | {_where(x)} | {x['cyclomatic']} | {x['blocks']} |"
            for x in c.get("most_complex", [])]
    out += ["", "| Weakest seam | Where | Blocks | Conductance |", "|---|---|---|---|"]
    out += [f"| {x['function']} | {_where(x)} | {x['blocks']} | {x['conductance']:.4f} |"
            for x in c.get("weakest_seams", [])]
    out += ["", "### Static analyzer: declarations", "", d["verdict"], ""]
    if d.get("unreached"):
        out += ["| Where | Kind | Name | Named |", "|---|---|---|---|"]
        out += [f"| {_where(x)} | {x['kind']} | {x['name']} | {x['named']} |" for x in d["unreached"]]
        out.append("")
    if d.get("caveat"):
        out += [d["caveat"], ""]
    return "\n".join(out) + "\n"


def gauges(reports: dict):
    """Every gauge montauk reported, for the scoreboard's .prom: (metric
    without the montauk_static_ prefix, value, help, labels). montauk
    renders a gauge's labels as `k="v",...`; they come back as a dict."""
    for rep in reports.values():
        for gauge in rep.get("gauges", []):
            labels = dict(re.findall(r'(\w+)="([^"]*)"', gauge.get("labels") or ""))
            yield (gauge["name"].removeprefix("montauk_static_"), gauge["value"],
                   gauge.get("help", ""), labels or None)
