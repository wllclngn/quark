#!/usr/bin/env python3
"""
amphetamine test orchestrator.

Unified test suite following PANDEMONIUM's bench-* pattern.
Subcommands: test-patch, test-build, test-bypass, test-package, test-compat, test-all.

Usage:
    ./tests/amphetamine-tests.py test-patch --wine-dir /tmp/proton-wine
    ./tests/amphetamine-tests.py test-build --wine-dir /tmp/proton-wine
    ./tests/amphetamine-tests.py test-bypass
    ./tests/amphetamine-tests.py test-package
    ./tests/amphetamine-tests.py test-all --wine-dir /tmp/proton-wine
"""

import argparse
import io
import json
import mmap
import os
import re
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import time
from datetime import datetime
from pathlib import Path

SCRIPT_DIR = Path(__file__).parent.resolve()
PROJECT_DIR = SCRIPT_DIR.parent
PATCHES_DIR = PROJECT_DIR / "patches" / "wine"

DEFAULT_WINE_DIR = Path.home() / ".local" / "share" / "amphetamine" / "wine-src"
TOOL_DIR = Path.home() / ".steam" / "root" / "compatibilitytools.d" / "amphetamine"
FILES_DIR = TOOL_DIR / "files"
BIN_DIR = FILES_DIR / "bin"
LIB_DIR = FILES_DIR / "lib"
LIB_WIN = LIB_DIR / "wine" / "x86_64-windows"
LIB_UNIX = LIB_DIR / "wine" / "x86_64-unix"

LOG_DIR = Path.home() / ".cache" / "amphetamine"
LOG_FILE = LOG_DIR / "tests.log"

SHM_MAGIC = 0x54524953
SHM_VERSION = 1
MAX_THREADS = 256
HEADER_SIZE = 64
QUEUE_SIZE = 24896

# Cross-thread PostMessage test: worker thread posts 100 messages to UI thread.
# This exercises the bypass because cross-thread PostMessage goes through
# send_message (server opcode), which triskelion.c intercepts.
TEST_SOURCE = r"""
#include <windows.h>
#include <stdio.h>

#define WM_TEST  (WM_USER + 1)
#define NUM_MSGS 100

static HWND    g_hwnd;
static HANDLE  g_ready;

static LRESULT CALLBACK wnd_proc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    return DefWindowProcW(hwnd, msg, wp, lp);
}

static DWORD WINAPI poster_thread(void *arg)
{
    WaitForSingleObject(g_ready, INFINITE);

    int sent = 0;
    for (int i = 0; i < NUM_MSGS; i++) {
        if (PostMessageW(g_hwnd, WM_TEST, (WPARAM)i, 0))
            sent++;
    }
    *(int *)arg = sent;
    return 0;
}

int main(void)
{
    WNDCLASSW wc = {0};
    wc.lpfnWndProc   = wnd_proc;
    wc.lpszClassName  = L"triskelion_test";
    wc.hInstance      = GetModuleHandleW(NULL);
    RegisterClassW(&wc);

    g_hwnd = CreateWindowW(L"triskelion_test", L"t", 0,
                           0, 0, 1, 1, NULL, NULL, wc.hInstance, NULL);
    if (!g_hwnd) return 1;

    MSG msg;
    PeekMessageW(&msg, NULL, 0, 0, PM_NOREMOVE);
    Sleep(10);

    int sent = 0;
    g_ready = CreateEventW(NULL, TRUE, FALSE, NULL);
    HANDLE thread = CreateThread(NULL, 0, poster_thread, &sent, 0, NULL);

    SetEvent(g_ready);
    WaitForSingleObject(thread, INFINITE);
    CloseHandle(thread);
    CloseHandle(g_ready);

    int received = 0;
    while (PeekMessageW(&msg, NULL, 0, 0, PM_REMOVE)) {
        if (msg.message == WM_TEST) received++;
        DispatchMessageW(&msg);
    }

    FILE *f = fopen("Z:\\tmp\\triskelion_test_result.txt", "w");
    if (f) {
        fprintf(f, "sent=%d received=%d\n", sent, received);
        fclose(f);
    }

    DestroyWindow(g_hwnd);
    if (received == NUM_MSGS) return 0;
    if (received > 0) return 3;
    return 2;
}
"""

passed = 0
failed = 0
skipped = 0
errors = []
log_buf = io.StringIO()
verbose = False


def log(msg):
    ts = datetime.now().strftime("%H:%M:%S")
    line = f"[{ts}] {msg}"
    print(line)
    log_buf.write(line + "\n")


def ok(name, detail=""):
    global passed
    passed += 1
    extra = f"  ({detail})" if detail else ""
    log(f"  PASS  {name}{extra}")


def fail(name, reason):
    global failed
    failed += 1
    errors.append((name, reason))
    log(f"  FAIL  {name}: {reason}")


def skip(name, reason):
    global skipped
    skipped += 1
    log(f"  SKIP  {name}: {reason}")


def summary(label):
    total = passed + failed
    log("")
    log(f"  {label}: {passed}/{total} passed, {failed} failed, {skipped} skipped")
    if errors:
        log("")
        for name, reason in errors:
            log(f"  FAIL  {name}: {reason}")
    log("")


def flush_log():
    LOG_DIR.mkdir(parents=True, exist_ok=True)
    with open(LOG_FILE, "a") as f:
        ts = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        f.write(f"\namphetamine tests -- {ts}\n\n")
        f.write(log_buf.getvalue())
    log(f"  Log: {LOG_FILE}")


def reset_counters():
    global passed, failed, skipped, errors
    passed = 0
    failed = 0
    skipped = 0
    errors = []


# test-patch: verify install.py patches applied correctly

def cmd_test_patch(args):
    wine_dir = Path(args.wine_dir)
    log("")
    log("  test-patch: verifying Wine source patches")
    log(f"  Wine dir: {wine_dir}")
    log("")

    if not wine_dir.exists():
        fail("wine dir exists", f"{wine_dir} not found")
        summary("test-patch")
        return 1

    # ntdll: triskelion.c
    tc = wine_dir / "dlls" / "ntdll" / "unix" / "triskelion.c"
    if tc.exists():
        ok("triskelion.c exists")
        src = PATCHES_DIR / "dlls" / "ntdll" / "unix" / "triskelion.c"
        if src.exists() and tc.read_bytes() == src.read_bytes():
            ok("triskelion.c content matches source")
        elif src.exists():
            fail("triskelion.c content matches source", "files differ")
        else:
            skip("triskelion.c content matches source", "patch source not found")
    else:
        fail("triskelion.c exists", "not found")

    # ntdll: Makefile.in
    mf = wine_dir / "dlls" / "ntdll" / "Makefile.in"
    if mf.exists():
        text = mf.read_text()
        if "unix/triskelion.c" in text:
            ok("Makefile.in has triskelion.c")
            lines = text.splitlines()
            thread_idx = None
            triskelion_idx = None
            for i, line in enumerate(lines):
                if "unix/thread.c" in line:
                    thread_idx = i
                if "unix/triskelion.c" in line:
                    triskelion_idx = i
            if thread_idx is not None and triskelion_idx is not None and triskelion_idx > thread_idx:
                ok("Makefile.in ordering", "triskelion.c after thread.c")
            else:
                fail("Makefile.in ordering", f"thread={thread_idx}, triskelion={triskelion_idx}")
        else:
            fail("Makefile.in has triskelion.c", "not found in file")
    else:
        fail("Makefile.in exists", "not found")

    # ntdll: server.c
    sc = wine_dir / "dlls" / "ntdll" / "unix" / "server.c"
    if sc.exists():
        text = sc.read_text()
        if "triskelion_try_bypass" in text:
            ok("server.c has bypass call")
            lines = text.splitlines()
            bypass_idx = None
            ftrace_idx = None
            for i, line in enumerate(lines):
                if "triskelion_try_bypass" in line and bypass_idx is None:
                    bypass_idx = i
                if 'FTRACE_BLOCK_START("req %s"' in line and ftrace_idx is None:
                    ftrace_idx = i
            if bypass_idx is not None and ftrace_idx is not None and bypass_idx < ftrace_idx:
                ok("server.c bypass before FTRACE")
            else:
                fail("server.c bypass before FTRACE", f"bypass={bypass_idx}, ftrace={ftrace_idx}")
            if "STATUS_NOT_IMPLEMENTED" in text:
                ok("server.c bypass structure", "has STATUS_NOT_IMPLEMENTED check")
            else:
                fail("server.c bypass structure", "missing STATUS_NOT_IMPLEMENTED")
        else:
            fail("server.c has bypass call", "triskelion_try_bypass not found")
    else:
        fail("server.c exists", "not found")

    # ntdll: unix_private.h
    uh = wine_dir / "dlls" / "ntdll" / "unix" / "unix_private.h"
    if uh.exists():
        text = uh.read_text()
        if "triskelion_try_bypass" in text:
            ok("unix_private.h has declaration")
            lines = text.splitlines()
            scul_idx = None
            ttb_idx = None
            for i, line in enumerate(lines):
                if "server_call_unlocked" in line:
                    scul_idx = i
                if "triskelion_try_bypass" in line:
                    ttb_idx = i
            if scul_idx is not None and ttb_idx is not None and ttb_idx > scul_idx:
                ok("unix_private.h declaration after server_call_unlocked")
            else:
                fail("unix_private.h declaration ordering", f"scul={scul_idx}, ttb={ttb_idx}")
        else:
            fail("unix_private.h has declaration", "triskelion_try_bypass not found")
    else:
        fail("unix_private.h exists", "not found")

    # win32u: message.c
    mc = wine_dir / "dlls" / "win32u" / "message.c"
    if mc.exists():
        text = mc.read_text()
        if "triskelion_has_posted" in text:
            ok("message.c has triskelion_has_posted")
            if "static inline BOOL triskelion_has_posted( volatile void *queue_ptr )" in text:
                ok("message.c function signature correct")
            else:
                fail("message.c function signature", "unexpected signature")
            if "volatile ULONGLONG" in text:
                ok("message.c reads write_pos and read_pos")
            else:
                fail("message.c reads write_pos and read_pos", "volatile ULONGLONG not found")
            if "triskelion_has_posted(NtCurrentTeb()->glReserved2)" in text:
                ok("message.c peek_message has triskelion check")
            else:
                fail("message.c peek_message has triskelion check", "call site not found")
            # Verify ordering: triskelion check before filter->waited
            lines = text.splitlines()
            for i, line in enumerate(lines):
                if "triskelion_has_posted(NtCurrentTeb" in line:
                    if i + 1 < len(lines) and "filter->waited" in lines[i + 1]:
                        ok("message.c triskelion check before filter->waited")
                    else:
                        fail("message.c ordering", "filter->waited not on next line")
                    break
            if "disable check_queue_bits and force the server call path" in text:
                ok("message.c comment block present")
            else:
                fail("message.c comment block", "expected comment not found")
        else:
            fail("message.c has triskelion_has_posted", "function not found")
    else:
        fail("message.c exists", "not found")

    summary("test-patch")
    flush_log()
    return 0 if failed == 0 else 1


# test-build: verify compiled Wine has triskelion symbols

def cmd_test_build(args):
    wine_dir = Path(args.wine_dir)
    log("")
    log("  test-build: verifying Wine build artifacts")
    log(f"  Wine dir: {wine_dir}")
    log("")

    if not wine_dir.exists():
        fail("wine dir exists", f"{wine_dir} not found")
        summary("test-build")
        return 1

    # ntdll.so
    ntdll_so = wine_dir / "dlls" / "ntdll" / "ntdll.so"
    if ntdll_so.exists():
        ok("ntdll.so exists")
        result = subprocess.run(["nm", "-D", str(ntdll_so)], capture_output=True, text=True)
        symbols = result.stdout
        if "triskelion_try_bypass" in symbols:
            ok("ntdll.so has triskelion_try_bypass")
        else:
            result2 = subprocess.run(["objdump", "-t", str(ntdll_so)], capture_output=True, text=True)
            if "triskelion" in result2.stdout:
                ok("ntdll.so has triskelion symbols (objdump)")
            else:
                fail("ntdll.so has triskelion symbols", "not in symbol table")
        if "do_triskelion" in symbols:
            ok("ntdll.so has do_triskelion")
        else:
            skip("ntdll.so has do_triskelion", "not in dynamic symbols (may be static)")
    else:
        fail("ntdll.so exists", "not found (Wine not built?)")

    # win32u.so
    win32u_so = wine_dir / "dlls" / "win32u" / "win32u.so"
    if win32u_so.exists():
        ok("win32u.so exists")
    else:
        fail("win32u.so exists", "not found")

    # Object files
    triskelion_o = wine_dir / "dlls" / "ntdll" / "unix" / "triskelion.o"
    if triskelion_o.exists():
        ok("triskelion.o compiled", f"{triskelion_o.stat().st_size} bytes")
    else:
        fail("triskelion.o compiled", "not found")

    message_o = wine_dir / "dlls" / "win32u" / "message.o"
    if message_o.exists():
        ok("message.o compiled", f"{message_o.stat().st_size} bytes")
    else:
        fail("message.o compiled", "not found")

    summary("test-build")
    flush_log()
    return 0 if failed == 0 else 1


# test-bypass: full PostMessage/GetMessage bypass verification

def make_wine_env(pfx):
    env = dict(os.environ)
    env["WINEPREFIX"] = pfx
    env["WINEDLLPATH"] = str(LIB_DIR / "wine")
    env["WINEDEBUG"] = "-all"
    env["WINE_TRISKELION"] = "1"
    env["PATH"] = f"{BIN_DIR}:{env.get('PATH', '')}"
    env["LD_LIBRARY_PATH"] = f"{LIB_DIR}:{env.get('LD_LIBRARY_PATH', '')}"
    return env


def kill_wineserver(pfx):
    ws = BIN_DIR / "wineserver"
    if ws.exists() and pfx:
        env = dict(os.environ)
        env["WINEPREFIX"] = pfx
        subprocess.run([str(ws), "-k"], env=env, capture_output=True, timeout=10)
        time.sleep(1)


def parse_server_traces(stderr, label):
    if not stderr:
        return
    lines = stderr.splitlines()
    counts = {}
    msg_types = {}
    for line in lines:
        if ": " in line and "(" in line:
            after_colon = line.split(": ", 1)
            if len(after_colon) == 2:
                op = after_colon[1].split("(")[0].strip()
                if op and not op.startswith("*") and op[0].isalpha():
                    counts[op] = counts.get(op, 0) + 1
                    if op == "send_message" and "type=" in line:
                        t = line.split("type=")[1].split(",")[0].strip()
                        msg_types[t] = msg_types.get(t, 0) + 1
    total = sum(counts.values())
    if verbose:
        log(f"  Server calls ({label}): {total} total")
        for op, cnt in sorted(counts.items(), key=lambda x: -x[1])[:10]:
            log(f"    {cnt:6d}  {op}")
    return counts, msg_types


def cmd_test_bypass(args):
    log("")
    log("  test-bypass: PostMessage/GetMessage shared-memory bypass verification")
    log("")

    # Phase 1: static checks
    log("  [static checks]")
    ntdll_so = LIB_UNIX / "ntdll.so"
    if ntdll_so.exists():
        result = subprocess.run(["nm", "-D", str(ntdll_so)], capture_output=True, text=True)
        if "triskelion_try_bypass" in result.stdout:
            ok("ntdll.so has triskelion symbol")
        else:
            result2 = subprocess.run(["objdump", "-t", str(ntdll_so)], capture_output=True, text=True)
            if "triskelion" in result2.stdout:
                ok("ntdll.so has triskelion symbol (objdump)")
            else:
                fail("ntdll.so has triskelion symbol", "not in symbol table")
    else:
        fail("ntdll.so exists", "not found")

    api_dll = LIB_WIN / "apisetschema.dll"
    if api_dll.exists():
        ok("apisetschema.dll exists", f"{api_dll.stat().st_size} bytes")
    else:
        fail("apisetschema.dll exists", "missing")

    # Phase 2: runtime init
    log("")
    log("  [runtime init]")
    wine64 = BIN_DIR / "wine64"
    if not wine64.exists():
        fail("wine64 exists", "not found")
        summary("test-bypass")
        flush_log()
        return 1
    ok("wine64 exists")

    tmpdir = tempfile.mkdtemp(prefix="triskelion_test_")
    pfx = os.path.join(tmpdir, "pfx")
    env = make_wine_env(pfx)
    env["WINEDEBUG"] = "+server"

    trace_path = os.path.join(tmpdir, "wineboot_trace.log")
    try:
        with open(trace_path, "w") as tf:
            result = subprocess.run(
                [str(wine64), "wineboot", "--init"],
                env=env, timeout=60, stdout=subprocess.DEVNULL, stderr=tf,
            )
        time.sleep(0.5)
        if os.path.exists(os.path.join(pfx, "system.reg")):
            ok("wineboot init succeeds", f"rc={result.returncode}")
        else:
            fail("wineboot init", f"system.reg not created, rc={result.returncode}")
    except subprocess.TimeoutExpired:
        fail("wineboot init", "timed out after 60s")
        summary("test-bypass")
        flush_log()
        return 1

    kill_wineserver(pfx)

    # Phase 3: PostMessage bypass
    log("")
    log("  [PostMessage bypass]")

    skip_compile = getattr(args, "skip_compile", False)
    bypass_only = getattr(args, "bypass_only", False)

    cc_result = subprocess.run(["which", "x86_64-w64-mingw32-gcc"], capture_output=True, text=True)
    if cc_result.returncode != 0:
        fail("mingw compiler", "x86_64-w64-mingw32-gcc not found")
        summary("test-bypass")
        flush_log()
        return 1
    cc = cc_result.stdout.strip()
    ok("mingw compiler available", cc)

    src_path = os.path.join(tmpdir, "test_post.c")
    exe_path = os.path.join(tmpdir, "test_post.exe")

    if not skip_compile:
        with open(src_path, "w") as f:
            f.write(TEST_SOURCE)
        comp = subprocess.run([cc, "-O2", src_path, "-o", exe_path, "-luser32"], capture_output=True, text=True)
        if comp.returncode != 0:
            fail("compile test_post.exe", comp.stderr.strip())
            summary("test-bypass")
            flush_log()
            return 1
        ok("test_post.exe compiles")

    runs = [True] if bypass_only else [False, True]
    for bypass_on in runs:
        label = "bypass=ON" if bypass_on else "bypass=OFF"
        trace = os.path.join(tmpdir, f"postmsg_{'on' if bypass_on else 'off'}.log")
        run_env = make_wine_env(pfx)
        run_env["WINEDEBUG"] = "+msg,+server"
        run_env["WINE_TRISKELION"] = "1" if bypass_on else "0"

        kill_wineserver(pfx)

        try:
            with open(trace, "w") as tf:
                result = subprocess.run(
                    [str(wine64), exe_path],
                    env=run_env, timeout=30, stdout=subprocess.DEVNULL, stderr=tf,
                )
            time.sleep(0.5)

            with open(trace, "r") as f:
                stderr = f.read()

            result_file = "/tmp/triskelion_test_result.txt"
            result_info = ""
            if os.path.exists(result_file):
                with open(result_file) as rf:
                    result_info = rf.read().strip()
                os.unlink(result_file)

            if result.returncode == 0:
                ok(f"test runs ({label})", f"rc=0, {result_info}")
            else:
                fail(f"test runs ({label})", f"rc={result.returncode}, {result_info}")

            if bypass_on:
                shm_files = [f for f in os.listdir("/dev/shm") if f.startswith("triskelion-")]
                if shm_files:
                    ok("shm created", f"{shm_files}")
                else:
                    fail("shm created", "no triskelion-* in /dev/shm")

            # Check for WM_TEST in server traces
            msg401_lines = [l for l in stderr.splitlines()
                           if "send_message" in l and "0401" in l]

            if bypass_on:
                if len(msg401_lines) == 0:
                    ok("bypass intercepts WM_TEST", "0 WM_TEST send_message calls hit server")
                else:
                    fail("bypass intercepts WM_TEST", f"{len(msg401_lines)} leaked to server")
            else:
                if len(msg401_lines) > 0:
                    ok("control: WM_TEST hits server", f"{len(msg401_lines)} calls")
                else:
                    fail("control: WM_TEST hits server", "no WM_TEST in server trace")

            if verbose:
                parse_server_traces(stderr, label)

        except subprocess.TimeoutExpired:
            fail(f"test runs ({label})", "timed out after 30s")

    # Phase 4: shared memory inspection
    log("")
    log("  [shared memory inspection]")

    st = os.stat(pfx)
    shm_name = f"triskelion-{st.st_dev:x}{st.st_ino:x}"
    shm_path = f"/dev/shm/{shm_name}"

    if os.path.exists(shm_path):
        size = os.path.getsize(shm_path)
        expected = HEADER_SIZE + MAX_THREADS * QUEUE_SIZE
        ok("shm segment found", shm_name)
        if size == expected:
            ok("shm size correct", f"{size:,} bytes")
        else:
            fail("shm size correct", f"got {size:,}, expected {expected:,}")

        try:
            fd = os.open(shm_path, os.O_RDONLY)
            mm = mmap.mmap(fd, size, access=mmap.ACCESS_READ)
            os.close(fd)

            magic, version, max_threads, queue_size, next_slot = struct.unpack_from("<IIIII", mm, 0)

            if magic == SHM_MAGIC:
                ok("shm magic", f"0x{magic:08X}")
            else:
                fail("shm magic", f"0x{magic:08X}, expected 0x{SHM_MAGIC:08X}")

            if version == SHM_VERSION:
                ok("shm version", str(version))
            else:
                fail("shm version", f"{version}, expected {SHM_VERSION}")

            if max_threads == MAX_THREADS:
                ok("shm max_threads", str(max_threads))
            else:
                fail("shm max_threads", f"{max_threads}, expected {MAX_THREADS}")

            if queue_size == QUEUE_SIZE:
                ok("shm queue_size", f"{queue_size:,}")
            else:
                fail("shm queue_size", f"{queue_size:,}, expected {QUEUE_SIZE:,}")

            if next_slot > 0:
                first_tid_offset = HEADER_SIZE + 24848
                first_tid = struct.unpack_from("<I", mm, first_tid_offset)[0]
                if 0x20 <= first_tid < 0x10000:
                    ok("shm thread registered", f"slot 0 tid=0x{first_tid:x}, {next_slot} slots used")
                elif first_tid == 0:
                    fail("shm thread registered", "slot 0 tid=0 (uninitialized)")
                else:
                    fail("shm thread registered", f"slot 0 tid=0x{first_tid:x} (unexpected range)")
            else:
                fail("shm thread registered", "next_slot == 0")

            if verbose:
                log(f"  Slot details ({next_slot} allocated):")
                for i in range(min(next_slot, 8)):
                    off = HEADER_SIZE + i * QUEUE_SIZE
                    pw = struct.unpack_from("<Q", mm, off)[0]
                    pr = struct.unpack_from("<Q", mm, off + 64)[0]
                    tid = struct.unpack_from("<I", mm, off + 24848)[0]
                    log(f"    slot {i}: tid=0x{tid:08x} posted={pw - pr} (w={pw} r={pr})")

            mm.close()
        except Exception as e:
            fail("shm inspection", str(e))
    else:
        shm_files = [f for f in os.listdir("/dev/shm") if f.startswith("triskelion-")]
        if shm_files:
            fail("shm segment found", f"expected {shm_name}, found: {shm_files}")
        else:
            fail("shm segment found", "no triskelion-* in /dev/shm")

    # Cleanup
    kill_wineserver(pfx)

    # Remove the shm segment for this test prefix
    if os.path.exists(pfx):
        try:
            st = os.stat(pfx)
            test_shm = f"/dev/shm/triskelion-{st.st_dev:x}{st.st_ino:x}"
            if os.path.exists(test_shm):
                os.unlink(test_shm)
                log(f"  Cleaned up: {test_shm}")
        except OSError:
            pass

    keep = getattr(args, "keep_on_fail", False)
    if failed > 0 and keep:
        log(f"  Traces preserved: {tmpdir}")
    else:
        shutil.rmtree(tmpdir, ignore_errors=True)

    summary("test-bypass")
    flush_log()
    return 0 if failed == 0 else 1


# test-package: delegate to existing test_package.py

def cmd_test_package(args):
    log("")
    log("  test-package: delegating to test_package.py")
    log("")

    test_script = SCRIPT_DIR / "test_package.py"
    if not test_script.exists():
        fail("test_package.py exists", "not found")
        summary("test-package")
        flush_log()
        return 1

    cmd = [sys.executable, str(test_script)]
    smoke = getattr(args, "smoke", None)
    if smoke:
        cmd.extend(["--smoke", smoke])

    result = subprocess.run(cmd, cwd=PROJECT_DIR)

    if result.returncode == 0:
        ok("test_package.py", "all tests passed")
    else:
        fail("test_package.py", f"exit code {result.returncode}")

    summary("test-package")
    flush_log()
    return result.returncode


# test-all: run everything in sequence

def cmd_test_all(args):
    log("")
    log("  test-all: running full test suite")
    log("")

    results = {}
    commands = [
        ("test-patch", cmd_test_patch),
        ("test-build", cmd_test_build),
        ("test-bypass", cmd_test_bypass),
        ("test-package", cmd_test_package),
    ]

    total_pass = 0
    total_fail = 0
    total_skip = 0

    for name, func in commands:
        reset_counters()
        log(f"  {'=' * 40}")
        rc = func(args)
        results[name] = (rc, passed, failed, skipped)
        total_pass += passed
        total_fail += failed
        total_skip += skipped

    log(f"  {'=' * 40}")
    log("")
    log("  test-all summary:")
    for name, (rc, p, f, s) in results.items():
        status = "PASS" if rc == 0 else "FAIL"
        log(f"    {status}  {name}: {p} passed, {f} failed, {s} skipped")
    log("")
    log(f"  Total: {total_pass} passed, {total_fail} failed, {total_skip} skipped")
    log("")
    flush_log()
    return 0 if total_fail == 0 else 1


# test-compat: dynamic game discovery + automated compatibility testing

COMPAT_DIR = Path("/tmp/amphetamine/compat")
STEAM_ROOT = Path.home() / ".steam" / "root"
STEAMAPPS = STEAM_ROOT / "steamapps"
INFRA_KEYWORDS = ["Runtime", "Proton", "Redistributable"]

ACF_KV_RE = re.compile(r'"([^"]+)"\s+"([^"]*)"')


def parse_acf(path):
    """Parse a Steam ACF/VDF manifest. Returns dict of top-level key-value pairs."""
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    result = {}
    depth = 0
    for line in text.splitlines():
        stripped = line.strip()
        if stripped == "{":
            depth += 1
            continue
        if stripped == "}":
            depth -= 1
            continue
        if depth == 1:
            m = ACF_KV_RE.match(stripped)
            if m:
                result[m.group(1)] = m.group(2)
    return result if result else None


def parse_compat_tool_mapping():
    """Parse Steam config.vdf for CompatToolMapping. Returns {appid: tool_name}."""
    config_path = STEAM_ROOT / "config" / "config.vdf"
    if not config_path.exists():
        return {}
    try:
        text = config_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return {}
    mapping = {}
    in_section = False
    section_depth = 0
    current_appid = None
    for line in text.splitlines():
        stripped = line.strip()
        if not in_section:
            if '"CompatToolMapping"' in stripped:
                in_section = True
            continue
        if stripped == "{":
            section_depth += 1
            continue
        if stripped == "}":
            section_depth -= 1
            if section_depth == 1:
                current_appid = None
            if section_depth <= 0:
                break
            continue
        if section_depth == 1:
            m = re.match(r'"(\d+)"', stripped)
            if m:
                current_appid = m.group(1)
        elif section_depth == 2 and current_appid:
            m = ACF_KV_RE.match(stripped)
            if m and m.group(1) == "name":
                mapping[current_appid] = m.group(2)
    return mapping


def discover_steam_games():
    """Discover all installed Steam games. Returns list of dicts."""
    seen_resolved = set()
    library_paths = []
    if STEAMAPPS.exists():
        library_paths.append(STEAMAPPS)
        seen_resolved.add(STEAMAPPS.resolve())

    lf_path = STEAMAPPS / "libraryfolders.vdf"
    if lf_path.exists():
        try:
            text = lf_path.read_text()
            for m in re.finditer(r'"path"\s+"([^"]+)"', text):
                p = Path(m.group(1)) / "steamapps"
                if p.exists() and p.resolve() not in seen_resolved:
                    library_paths.append(p)
                    seen_resolved.add(p.resolve())
        except OSError:
            pass

    compat_mapping = parse_compat_tool_mapping()
    games = []

    for lib_path in library_paths:
        for manifest in sorted(lib_path.glob("appmanifest_*.acf")):
            data = parse_acf(manifest)
            if not data or "appid" not in data:
                continue
            app_id = data["appid"]
            name = data.get("name", f"Unknown ({app_id})")
            is_infra = any(kw in name for kw in INFRA_KEYWORDS)
            if not is_infra and data.get("LastPlayed") == "0" and data.get("LastOwner") == "0":
                is_infra = True
            games.append({
                "app_id": app_id,
                "name": name,
                "size_on_disk": int(data.get("SizeOnDisk", "0")),
                "last_played": int(data.get("LastPlayed", "0")),
                "has_compatdata": (lib_path / "compatdata" / app_id).is_dir(),
                "compat_tool": compat_mapping.get(app_id, ""),
                "is_infrastructure": is_infra,
            })

    return games


def compat_discover():
    """Print table of discovered Steam games."""
    games = discover_steam_games()
    if not games:
        log("  No Steam games found")
        return 0

    real_games = [g for g in games if not g["is_infrastructure"]]
    infra = [g for g in games if g["is_infrastructure"]]

    log("")
    log("  Installed Steam Games")
    log("")
    log(f"  {'AppID':<10} {'Name':<35} {'Compat Tool':<15} {'Size':<12} {'Last Played':<12}")

    for g in sorted(real_games, key=lambda x: x["name"]):
        size_mb = g["size_on_disk"] / (1024 * 1024)
        size_str = f"{size_mb:.0f} MB" if size_mb < 1024 else f"{size_mb / 1024:.1f} GB"
        lp = datetime.fromtimestamp(g["last_played"]).strftime("%Y-%m-%d") if g["last_played"] > 0 else "never"
        tool = g["compat_tool"] if g["compat_tool"] else "(default)"
        log(f"  {g['app_id']:<10} {g['name']:<35} {tool:<15} {size_str:<12} {lp:<12}")

    log("")
    log(f"  {len(real_games)} games, {len(infra)} infrastructure apps filtered")
    amphetamine_count = sum(1 for g in real_games if g["compat_tool"] == "amphetamine")
    log(f"  {amphetamine_count}/{len(real_games)} configured for amphetamine")
    log("")
    return 0


def find_wine_pid(app_id):
    """Find a wine process whose WINEPREFIX contains this app_id's compatdata."""
    try:
        result = subprocess.run(["pgrep", "-a", "wine64-preloader"],
                                capture_output=True, text=True, timeout=5)
        if result.returncode != 0:
            return None
        for line in result.stdout.strip().splitlines():
            pid_str = line.split()[0]
            pid = int(pid_str)
            try:
                environ = Path(f"/proc/{pid}/environ").read_bytes()
                if f"compatdata/{app_id}".encode() in environ:
                    return pid
            except (OSError, PermissionError):
                continue
        # Fallback: return first wine64-preloader pid
        first_line = result.stdout.strip().splitlines()[0]
        return int(first_line.split()[0])
    except (subprocess.TimeoutExpired, ValueError, IndexError):
        return None


def compat_kill_game(app_id, pfx):
    """Kill game processes for an app_id and verify cleanup."""
    try:
        result = subprocess.run(["pgrep", "-a", "wine"],
                                capture_output=True, text=True, timeout=5)
        if result.returncode == 0:
            for line in result.stdout.strip().splitlines():
                pid_str = line.split()[0]
                pid = int(pid_str)
                try:
                    environ = Path(f"/proc/{pid}/environ").read_bytes()
                    if f"compatdata/{app_id}".encode() in environ:
                        os.kill(pid, signal.SIGTERM)
                except (OSError, PermissionError, ProcessLookupError):
                    continue
    except subprocess.TimeoutExpired:
        pass

    kill_wineserver(str(pfx))

    for _ in range(10):
        time.sleep(1)
        try:
            result = subprocess.run(["pgrep", "-a", "wine"],
                                    capture_output=True, text=True, timeout=5)
            if result.returncode != 0:
                return True
            found = False
            for line in result.stdout.strip().splitlines():
                pid_str = line.split()[0]
                try:
                    environ = Path(f"/proc/{int(pid_str)}/environ").read_bytes()
                    if f"compatdata/{app_id}".encode() in environ:
                        found = True
                        break
                except (OSError, PermissionError):
                    continue
            if not found:
                return True
        except subprocess.TimeoutExpired:
            pass
    return False


def compat_smoke_test(game, timeout=45):
    """Automated smoke test for a single game. Returns result dict."""
    app_id = game["app_id"]
    name = game["name"]
    pfx = STEAMAPPS / "compatdata" / app_id / "pfx"

    result = {
        "app_id": app_id,
        "name": name,
        "date": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
        "mode": "auto",
        "launch": None,
        "survived": None,
        "survival_seconds": 0,
        "shm_active": False,
        "clean_exit": None,
        "compat_tool": game["compat_tool"],
        "notes": "",
    }

    if not game["has_compatdata"]:
        result["notes"] = "no compatdata (not a Proton game)"
        skip(f"{name}", "no compatdata")
        return result

    if game["compat_tool"] != "amphetamine":
        result["notes"] = f"using {game['compat_tool'] or 'default'}, not amphetamine"
        skip(f"{name}", result["notes"])
        return result

    # Snapshot shm before launch
    shm_before = set(f for f in os.listdir("/dev/shm") if f.startswith("triskelion-"))

    # Launch via Steam
    log(f"  [{name}] launching via steam://rungameid/{app_id}")
    subprocess.Popen(["steam", f"steam://rungameid/{app_id}"],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    # Wait for wine process (up to 30s)
    wine_pid = None
    for _ in range(15):
        time.sleep(2)
        wine_pid = find_wine_pid(app_id)
        if wine_pid:
            break

    if not wine_pid:
        result["launch"] = False
        fail(f"{name}: launch", "no wine process after 30s")
        save_compat_result(result, name)
        return result

    result["launch"] = True
    ok(f"{name}: launch", f"wine pid={wine_pid}")

    # Survival check
    start = time.monotonic()
    alive = True
    while time.monotonic() - start < timeout:
        time.sleep(5)
        try:
            os.kill(wine_pid, 0)
        except ProcessLookupError:
            alive = False
            break

    elapsed = int(time.monotonic() - start)
    result["survival_seconds"] = elapsed
    result["survived"] = alive
    if alive:
        ok(f"{name}: survival", f"alive after {elapsed}s")
    else:
        fail(f"{name}: survival", f"died after {elapsed}s")

    # SHM check
    shm_after = set(f for f in os.listdir("/dev/shm") if f.startswith("triskelion-"))
    new_shm = shm_after - shm_before
    result["shm_active"] = len(new_shm) > 0
    if new_shm:
        ok(f"{name}: shm bypass", f"{len(new_shm)} new segments")
    else:
        fail(f"{name}: shm bypass", "no new triskelion-* segments")

    # Clean exit
    cleanup_ok = compat_kill_game(app_id, pfx)
    result["clean_exit"] = cleanup_ok
    if cleanup_ok:
        ok(f"{name}: clean exit")
    else:
        fail(f"{name}: clean exit", "processes lingered after kill")

    save_compat_result(result, name)
    return result


def save_compat_result(result, name):
    COMPAT_DIR.mkdir(parents=True, exist_ok=True)
    safe_name = re.sub(r'[^\w\-]', '_', name)
    result_file = COMPAT_DIR / f"{safe_name}.json"
    with open(result_file, "w") as f:
        json.dump(result, f, indent=2)
    log(f"  [{name}] result saved: {result_file}")


def compat_run_all(timeout=45):
    """Smoke test all amphetamine-configured Proton games."""
    games = discover_steam_games()
    testable = [g for g in games
                if not g["is_infrastructure"]
                and g["has_compatdata"]
                and g["compat_tool"] == "amphetamine"]

    if not testable:
        log("  No games configured for amphetamine")
        other = [g for g in games if g["has_compatdata"] and not g["is_infrastructure"]]
        if other:
            log("  Games with compatdata but NOT using amphetamine:")
            for g in other:
                log(f"    {g['app_id']}  {g['name']}  (tool: {g['compat_tool'] or 'default'})")
        return 1

    # Verify Steam is running
    steam_check = subprocess.run(["pgrep", "-x", "steam"], capture_output=True)
    if steam_check.returncode != 0:
        log("  Steam is not running. Start Steam first.")
        return 1

    log("")
    log(f"  test-compat --all: smoke testing {len(testable)} games ({timeout}s timeout each)")
    log("")

    results = []
    for i, game in enumerate(testable, 1):
        log(f"  [{i}/{len(testable)}] {game['name']} ({game['app_id']})")
        reset_counters()
        result = compat_smoke_test(game, timeout=timeout)
        results.append(result)
        log("")
        if i < len(testable):
            log("  Waiting 10s before next game...")
            time.sleep(10)

    # Summary table
    log("")
    log("  Smoke Test Results")
    log("")
    log(f"  {'Name':<30} {'Launch':<8} {'Alive':<8} {'SHM':<8} {'Exit':<8} {'Time'}")

    def s(v):
        if v is True: return "OK"
        if v is False: return "FAIL"
        return "-"

    for r in results:
        log(f"  {r['name']:<30} {s(r['launch']):<8} {s(r['survived']):<8} "
            f"{s(r['shm_active']):<8} {s(r['clean_exit']):<8} {r['survival_seconds']}s")

    log("")
    total_pass = sum(1 for r in results
                     if r["launch"] and r["survived"] and r.get("shm_active"))
    log(f"  {total_pass}/{len(results)} games passed smoke test")
    log("")
    flush_log()
    return 0 if total_pass == len(results) else 1


def cmd_test_compat(args):
    if getattr(args, "list", False):
        return compat_list()

    if getattr(args, "discover", False):
        return compat_discover()

    if getattr(args, "all", False):
        timeout = getattr(args, "timeout", 45)
        return compat_run_all(timeout=timeout)

    # Single-game interactive test
    app_id = getattr(args, "app_id", None)
    name = getattr(args, "name", None)

    if not app_id:
        log("  Usage: test-compat --app-id <id> --name <name>")
        log("         test-compat --discover")
        log("         test-compat --all [--timeout 45]")
        log("         test-compat --list")
        return 1

    if not name:
        # Look up name from Steam manifests
        games = discover_steam_games()
        match = [g for g in games if g["app_id"] == app_id]
        name = match[0]["name"] if match else f"game_{app_id}"

    log("")
    log(f"  test-compat: {name} (app_id={app_id})")
    log("")
    log("  This test is interactive. You will be prompted at each stage.")
    log("")

    result = {
        "app_id": app_id,
        "name": name,
        "date": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
        "mode": "interactive",
        "launch": None,
        "menu": None,
        "gameplay": None,
        "exit": None,
        "notes": "",
    }

    stages = [
        ("launch", "Game launched successfully?"),
        ("menu", "Main menu rendered and interactive?"),
        ("gameplay", "In-game play works (5+ min)?"),
        ("exit", "Game exited cleanly?"),
    ]

    for stage, prompt in stages:
        print(f"\n  [{stage}] {prompt}")
        print("  (y)es / (n)o / (s)kip: ", end="", flush=True)
        try:
            answer = input().strip().lower()
        except (EOFError, KeyboardInterrupt):
            log("  Aborted")
            return 1

        if answer in ("y", "yes"):
            result[stage] = True
            ok(f"{name}: {stage}")
        elif answer in ("n", "no"):
            result[stage] = False
            fail(f"{name}: {stage}", "user reported failure")
        else:
            result[stage] = None
            skip(f"{name}: {stage}", "skipped")

    print("\n  Notes (optional, press Enter to skip): ", end="", flush=True)
    try:
        result["notes"] = input().strip()
    except (EOFError, KeyboardInterrupt):
        pass

    save_compat_result(result, name)

    all_passed = all(result[s] is True for s, _ in stages if result[s] is not None)
    status_str = "PASS" if all_passed else "FAIL"
    log(f"  {name}: {status_str}")

    summary("test-compat")
    flush_log()
    return 0 if failed == 0 else 1


def compat_list():
    if not COMPAT_DIR.exists():
        log("  No compatibility results found")
        return 0

    results = sorted(COMPAT_DIR.glob("*.json"))
    if not results:
        log("  No compatibility results found")
        return 0

    log("")
    log("  Game Compatibility Results")
    log("")
    log(f"  {'Name':<30} {'AppID':<10} {'Mode':<8} {'Launch':<8} {'Alive':<8} {'SHM':<8} {'Exit':<8} {'Date':<12}")

    def s(v):
        if v is True: return "OK"
        if v is False: return "FAIL"
        return "-"

    for path in results:
        try:
            with open(path) as f:
                r = json.load(f)
            mode = r.get("mode", "?")[:6]
            log(f"  {r.get('name','?'):<30} {r.get('app_id','?'):<10} {mode:<8} "
                f"{s(r.get('launch')):<8} {s(r.get('survived')):<8} "
                f"{s(r.get('shm_active')):<8} {s(r.get('clean_exit')):<8} "
                f"{r.get('date','?')[:10]:<12}")
        except (json.JSONDecodeError, KeyError):
            log(f"  {path.name}: corrupt")

    log("")
    return 0


# Argument parsing

def main():
    global verbose

    parser = argparse.ArgumentParser(
        description="amphetamine test orchestrator",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    sub = parser.add_subparsers(dest="command")

    # test-patch
    p_patch = sub.add_parser("test-patch", help="Verify install.py patches applied to Wine source")
    p_patch.add_argument("--wine-dir", type=str, default=str(DEFAULT_WINE_DIR))
    p_patch.add_argument("--verbose", action="store_true")

    # test-build
    p_build = sub.add_parser("test-build", help="Verify patched Wine compiled correctly")
    p_build.add_argument("--wine-dir", type=str, default=str(DEFAULT_WINE_DIR))
    p_build.add_argument("--verbose", action="store_true")

    # test-bypass
    p_bypass = sub.add_parser("test-bypass", help="PostMessage/GetMessage bypass verification")
    p_bypass.add_argument("--skip-compile", action="store_true", help="Skip mingw compilation")
    p_bypass.add_argument("--bypass-only", action="store_true", help="Skip control (bypass=OFF) run")
    p_bypass.add_argument("--keep-on-fail", action="store_true", help="Keep temp files on failure")
    p_bypass.add_argument("--verbose", action="store_true")

    # test-package
    p_pkg = sub.add_parser("test-package", help="Package validation (delegates to test_package.py)")
    p_pkg.add_argument("--smoke", type=str, default=None, help="Steam app ID for smoke test")
    p_pkg.add_argument("--verbose", action="store_true")

    # test-compat
    p_compat = sub.add_parser("test-compat", help="Game compatibility testing")
    p_compat.add_argument("--discover", action="store_true", help="Discover installed Steam games")
    p_compat.add_argument("--all", action="store_true", help="Smoke test all amphetamine-configured games")
    p_compat.add_argument("--app-id", type=str, default=None, help="Steam app ID (single-game test)")
    p_compat.add_argument("--name", type=str, default=None, help="Game name")
    p_compat.add_argument("--list", action="store_true", help="List all recorded results")
    p_compat.add_argument("--timeout", type=int, default=45, help="Survival check duration in seconds")
    p_compat.add_argument("--verbose", action="store_true")

    # test-all
    p_all = sub.add_parser("test-all", help="Run all test suites")
    p_all.add_argument("--wine-dir", type=str, default=str(DEFAULT_WINE_DIR))
    p_all.add_argument("--skip-compile", action="store_true")
    p_all.add_argument("--bypass-only", action="store_true")
    p_all.add_argument("--keep-on-fail", action="store_true")
    p_all.add_argument("--smoke", type=str, default=None)
    p_all.add_argument("--verbose", action="store_true")

    args = parser.parse_args()
    verbose = getattr(args, "verbose", False)

    if not args.command:
        parser.print_help()
        return 1

    dispatch = {
        "test-patch": cmd_test_patch,
        "test-build": cmd_test_build,
        "test-bypass": cmd_test_bypass,
        "test-package": cmd_test_package,
        "test-compat": cmd_test_compat,
        "test-all": cmd_test_all,
    }

    return dispatch[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
