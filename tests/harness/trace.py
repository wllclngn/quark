"""The montauk capture around a launch: start, stop, decode, and the numbers
read back from it (ntsync ioctls, helper execs)."""
import json
import shutil
import subprocess
import time
from pathlib import Path
from quark_common import LOG_DIR, log_info, log_warn


MONTAUK_BIN = shutil.which("montauk") or "/usr/local/bin/montauk"


TRACE_BASE = LOG_DIR / "trace"
CLASSES: str | None = None  # set by --trace-classes; montauk's own list, every class otherwise


def start_trace(appid: str, tool: str, ts_prefix: str, pattern: str) -> tuple:
    """Spawn montauk in headless trace mode. Returns (process, log_dir) or
    (None, None) on failure. Never raises — tracing is best-effort."""
    if not Path(MONTAUK_BIN).exists():
        log_warn(f"trace: {MONTAUK_BIN} not found; skipping")
        return (None, None)
    log_dir = TRACE_BASE / f"{appid}_{ts_prefix}_{tool}"
    log_dir.mkdir(parents=True, exist_ok=True)
    # --trace-out: Raw binary event stream, every class unless CLASSES names
    # some, read back through montauk --analyze (decode_trace).
    trace_out = log_dir / "trace.bin"
    classes = ["--trace-classes", CLASSES] if CLASSES else []
    try:
        proc = subprocess.Popen(
            [MONTAUK_BIN, "--trace", pattern,
             "--trace-out", str(trace_out), *classes,
             "--log", str(log_dir), "--log-interval-ms", "500",
             "--headless"],
            stdout=subprocess.DEVNULL,
            stderr=open(log_dir / "montauk.stderr", "w"),
        )
    except OSError as e:
        log_warn(f"trace: spawn failed: {e}")
        return (None, None)
    # montauk needs ~500ms to attach BPF programs before the workload starts.
    time.sleep(0.7)
    if proc.poll() is not None:
        log_warn(f"trace: montauk exited early (code {proc.returncode}); "
                 f"check {log_dir}/montauk.stderr")
        return (None, log_dir)
    log_info(f"trace: montauk pid={proc.pid} pattern={pattern!r} → {log_dir}")
    return (proc, log_dir)


def stop_trace(proc) -> None:
    """Best-effort montauk shutdown. SIGTERM, then SIGKILL after 2s."""
    if proc is None:
        return
    try:
        proc.terminate()
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=2)
    except Exception as e:
        log_warn(f"trace: stop failed: {e}")


KEEP_TRACE = False  # set by --keep-trace; otherwise the raw capture goes once analyzed


def decode_trace(log_dir: Path) -> None:
    """Write the analyzer's full report set beside the capture as
    trace.json. That is what a capture gets read through; no decoded text
    or CSV, which came to 600 MB per launch on a RAM-backed /tmp."""
    if log_dir is None:
        return
    trace_bin = log_dir / "trace.bin"
    if not trace_bin.exists() or trace_bin.stat().st_size < 100:
        # < 100 bytes means just the header, no events; setcap is likely missing.
        return
    montauk = shutil.which("montauk")
    if montauk is None:
        log_warn(f"trace: montauk not found; trace.bin preserved at {trace_bin}")
        return
    report = log_dir / "trace.json"
    try:
        with open(report, "w") as f:
            subprocess.run([montauk, "--analyze", str(trace_bin), "--json"], stdout=f, timeout=600)
        log_info(f"trace: analyzed, report set in {report.name} ({report.stat().st_size} bytes)")
    except subprocess.TimeoutExpired:
        log_warn("trace: analysis timed out (trace.bin preserved)")
    except Exception as e:
        log_warn(f"trace: analysis failed: {e}")


def trim_trace(log_dir: Path) -> None:
    """Drop the raw capture once its numbers are read, unless --keep-trace.
    A capture is 200 MB and /tmp is memory."""
    if log_dir is None or KEEP_TRACE:
        return
    trace_bin = Path(log_dir) / "trace.bin"
    if trace_bin.exists():
        size_mb = trace_bin.stat().st_size / 1e6
        trace_bin.unlink()
        log_info(f"trace: raw capture dropped ({size_mb:.0f} MB); --keep-trace keeps it")


def _capture_count(trace_bin: Path, *select: str) -> int | None:
    cmd = [MONTAUK_BIN, "--analyze", str(trace_bin), "--select", *select, "--count", "--json"]
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=600)
        rows = json.loads(r.stdout).get("rows") or []
        return int(rows[0][0]) if rows else 0
    except (subprocess.TimeoutExpired, ValueError, OSError, IndexError, TypeError):
        return None


def _capture_stats(trace_dir) -> dict:
    """What the montauk capture says about the launch: ntsync ioctls, which
    proves the stock wineserver uses the device, and nvidia-modprobe execs,
    which should read 0 once the launch runs outside a user namespace."""
    out = {"ntsync_ops": None, "helper_execs": None}
    if not trace_dir:
        return out
    trace_bin = Path(trace_dir) / "trace.bin"
    if not trace_bin.exists():
        return out
    out["ntsync_ops"] = _capture_count(trace_bin, "ntsync")
    out["helper_execs"] = _capture_count(trace_bin, "exec", "--where", "comm=nvidia-modprobe")
    return out
