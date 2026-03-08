#!/usr/bin/env python3
"""
triskelion kernel module — ioctl test harness.

Opens /dev/triskelion and exercises every implemented ioctl.
Outputs results in Prometheus exposition format.

Usage:
    sudo insmod triskelion_kmod.ko debug=1
    python3 test_triskelion.py
"""

import ctypes
import ctypes.util
import fcntl
import os
import struct
import sys
import time
from pathlib import Path

# ── ioctl number construction (matches _IOWR/_IOW/_IOR from asm-generic) ──

_IOC_NRBITS   = 8
_IOC_TYPEBITS = 8
_IOC_SIZEBITS = 14
_IOC_DIRBITS  = 2

_IOC_NRSHIFT   = 0
_IOC_TYPESHIFT  = _IOC_NRSHIFT + _IOC_NRBITS
_IOC_SIZESHIFT  = _IOC_TYPESHIFT + _IOC_TYPEBITS
_IOC_DIRSHIFT   = _IOC_SIZESHIFT + _IOC_SIZEBITS

_IOC_WRITE = 1
_IOC_READ  = 2

def _IOC(dir_, type_, nr, size):
    return (dir_ << _IOC_DIRSHIFT) | (type_ << _IOC_TYPESHIFT) | \
           (nr << _IOC_NRSHIFT) | (size << _IOC_SIZESHIFT)

def _IOWR(type_, nr, size): return _IOC(_IOC_READ | _IOC_WRITE, type_, nr, size)
def _IOW(type_, nr, size):  return _IOC(_IOC_WRITE, type_, nr, size)
def _IOR(type_, nr, size):  return _IOC(_IOC_READ, type_, nr, size)

MAGIC = ord('T')

# ── Argument structures (must match triskelion.h exactly) ──

class SemArgs(ctypes.Structure):
    _fields_ = [
        ("handle",     ctypes.c_uint32),
        ("count",      ctypes.c_uint32),
        ("max_count",  ctypes.c_uint32),
        ("prev_count", ctypes.c_uint32),
    ]

class MutexArgs(ctypes.Structure):
    _fields_ = [
        ("handle",     ctypes.c_uint32),
        ("owner_tid",  ctypes.c_uint32),
        ("count",      ctypes.c_uint32),
        ("prev_count", ctypes.c_uint32),
    ]

class EventArgs(ctypes.Structure):
    _fields_ = [
        ("handle",        ctypes.c_uint32),
        ("manual_reset",  ctypes.c_uint32),
        ("initial_state", ctypes.c_uint32),
        ("prev_state",    ctypes.c_uint32),
    ]

class Msg(ctypes.Structure):
    _fields_ = [
        ("msg",       ctypes.c_uint32),
        ("wparam_lo", ctypes.c_uint32),
        ("wparam_hi", ctypes.c_uint32),
        ("lparam_lo", ctypes.c_uint32),
        ("lparam_hi", ctypes.c_uint32),
        ("time",      ctypes.c_uint32),
        ("info",      ctypes.c_uint32),
    ]

class PostMsgArgs(ctypes.Structure):
    _fields_ = [
        ("target_tid", ctypes.c_uint32),
        ("msg",        Msg),
    ]

class GetMsgArgs(ctypes.Structure):
    _fields_ = [
        ("msg",         Msg),
        ("has_message", ctypes.c_uint32),
    ]

# ── ioctl numbers ──

IOC_CREATE_SEM    = _IOWR(MAGIC, 0x10, ctypes.sizeof(SemArgs))
IOC_CREATE_MUTEX  = _IOWR(MAGIC, 0x11, ctypes.sizeof(MutexArgs))
IOC_CREATE_EVENT  = _IOWR(MAGIC, 0x12, ctypes.sizeof(EventArgs))
IOC_RELEASE_SEM   = _IOWR(MAGIC, 0x18, ctypes.sizeof(SemArgs))
IOC_RELEASE_MUTEX = _IOWR(MAGIC, 0x19, ctypes.sizeof(MutexArgs))
IOC_SET_EVENT     = _IOWR(MAGIC, 0x1A, ctypes.sizeof(EventArgs))
IOC_RESET_EVENT   = _IOWR(MAGIC, 0x1B, ctypes.sizeof(EventArgs))
IOC_PULSE_EVENT   = _IOWR(MAGIC, 0x1C, ctypes.sizeof(EventArgs))
IOC_POST_MSG      = _IOW(MAGIC,  0x30, ctypes.sizeof(PostMsgArgs))
IOC_GET_MSG       = _IOR(MAGIC,  0x31, ctypes.sizeof(GetMsgArgs))
IOC_CLOSE         = _IOW(MAGIC,  0x40, ctypes.sizeof(ctypes.c_uint32))

DEVICE = "/dev/triskelion"

# ── Test runner ──

class Result:
    def __init__(self, name):
        self.name = name
        self.passed = False
        self.error = None
        self.duration_ns = 0
        self.details = {}

    def ok(self, **details):
        self.passed = True
        self.details = details
        return self

    def fail(self, err):
        self.passed = False
        self.error = str(err)
        return self


def timed(func):
    def wrapper(*args, **kwargs):
        t0 = time.monotonic_ns()
        result = func(*args, **kwargs)
        result.duration_ns = time.monotonic_ns() - t0
        return result
    return wrapper


@timed
def test_open():
    r = Result("device_open")
    try:
        fd = os.open(DEVICE, os.O_RDWR)
        os.close(fd)
        return r.ok()
    except OSError as e:
        return r.fail(e)


@timed
def test_create_semaphore(fd):
    r = Result("create_semaphore")
    try:
        args = SemArgs(handle=0, count=3, max_count=10, prev_count=0)
        fcntl.ioctl(fd, IOC_CREATE_SEM, args)
        if args.handle == 0:
            return r.fail("handle is 0 (invalid)")
        return r.ok(handle=args.handle)
    except OSError as e:
        return r.fail(e)


@timed
def test_release_semaphore(fd, handle):
    r = Result("release_semaphore")
    try:
        args = SemArgs(handle=handle, count=2, max_count=0, prev_count=0)
        fcntl.ioctl(fd, IOC_RELEASE_SEM, args)
        return r.ok(prev_count=args.prev_count, released=2)
    except OSError as e:
        return r.fail(e)


@timed
def test_create_mutex(fd):
    r = Result("create_mutex")
    try:
        tid = os.getpid()
        args = MutexArgs(handle=0, owner_tid=tid, count=0, prev_count=0)
        fcntl.ioctl(fd, IOC_CREATE_MUTEX, args)
        if args.handle == 0:
            return r.fail("handle is 0 (invalid)")
        return r.ok(handle=args.handle, owner_tid=tid)
    except OSError as e:
        return r.fail(e)


@timed
def test_release_mutex(fd, handle, tid):
    r = Result("release_mutex")
    try:
        args = MutexArgs(handle=handle, owner_tid=tid, count=0, prev_count=0)
        fcntl.ioctl(fd, IOC_RELEASE_MUTEX, args)
        return r.ok(prev_count=args.prev_count)
    except OSError as e:
        return r.fail(e)


@timed
def test_create_event(fd):
    r = Result("create_event")
    try:
        args = EventArgs(handle=0, manual_reset=1, initial_state=0, prev_state=0)
        fcntl.ioctl(fd, IOC_CREATE_EVENT, args)
        if args.handle == 0:
            return r.fail("handle is 0 (invalid)")
        return r.ok(handle=args.handle, manual_reset=1)
    except OSError as e:
        return r.fail(e)


@timed
def test_set_event(fd, handle):
    r = Result("set_event")
    try:
        args = EventArgs(handle=handle, manual_reset=0, initial_state=0, prev_state=0)
        fcntl.ioctl(fd, IOC_SET_EVENT, args)
        return r.ok(prev_state=args.prev_state)
    except OSError as e:
        return r.fail(e)


@timed
def test_reset_event(fd, handle):
    r = Result("reset_event")
    try:
        args = EventArgs(handle=handle, manual_reset=0, initial_state=0, prev_state=0)
        fcntl.ioctl(fd, IOC_RESET_EVENT, args)
        return r.ok(prev_state=args.prev_state)
    except OSError as e:
        return r.fail(e)


@timed
def test_pulse_event(fd, handle):
    r = Result("pulse_event")
    try:
        args = EventArgs(handle=handle, manual_reset=0, initial_state=0, prev_state=0)
        fcntl.ioctl(fd, IOC_PULSE_EVENT, args)
        return r.ok(prev_state=args.prev_state)
    except OSError as e:
        return r.fail(e)


@timed
def test_post_message(fd):
    r = Result("post_message")
    try:
        tid = os.getpid()
        msg = Msg(msg=0x0010, wparam_lo=42, wparam_hi=0,
                  lparam_lo=1337, lparam_hi=0, time=0, info=0)
        args = PostMsgArgs(target_tid=tid, msg=msg)
        fcntl.ioctl(fd, IOC_POST_MSG, args)
        return r.ok(target_tid=tid, msg_id=0x0010)
    except OSError as e:
        return r.fail(e)


@timed
def test_get_message(fd):
    r = Result("get_message")
    try:
        args = GetMsgArgs()
        fcntl.ioctl(fd, IOC_GET_MSG, args)
        if args.has_message == 0:
            return r.fail("no message in queue")
        if args.msg.msg != 0x0010:
            return r.fail(f"wrong msg id: {args.msg.msg:#x}")
        if args.msg.wparam_lo != 42:
            return r.fail(f"wrong wparam_lo: {args.msg.wparam_lo}")
        if args.msg.lparam_lo != 1337:
            return r.fail(f"wrong lparam_lo: {args.msg.lparam_lo}")
        return r.ok(msg_id=args.msg.msg, wparam_lo=args.msg.wparam_lo,
                     lparam_lo=args.msg.lparam_lo)
    except OSError as e:
        return r.fail(e)


@timed
def test_get_message_empty(fd):
    r = Result("get_message_empty")
    try:
        args = GetMsgArgs()
        fcntl.ioctl(fd, IOC_GET_MSG, args)
        if args.has_message != 0:
            return r.fail("expected empty queue")
        return r.ok()
    except OSError as e:
        return r.fail(e)


@timed
def test_close_handle(fd, handle, obj_type):
    r = Result(f"close_{obj_type}")
    try:
        h = ctypes.c_uint32(handle)
        fcntl.ioctl(fd, IOC_CLOSE, h)
        return r.ok(handle=handle)
    except OSError as e:
        return r.fail(e)


@timed
def test_close_invalid(fd):
    r = Result("close_invalid_handle")
    try:
        h = ctypes.c_uint32(0)  # TRISKELION_INVALID_HANDLE
        fcntl.ioctl(fd, IOC_CLOSE, h)
        return r.fail("should have returned error")
    except OSError as e:
        if e.errno == 22:  # EINVAL
            return r.ok(errno="EINVAL")
        return r.fail(e)


@timed
def test_sem_overflow(fd):
    r = Result("semaphore_overflow")
    try:
        args = SemArgs(handle=0, count=1, max_count=2, prev_count=0)
        fcntl.ioctl(fd, IOC_CREATE_SEM, args)
        handle = args.handle

        release = SemArgs(handle=handle, count=5, max_count=0, prev_count=0)
        try:
            fcntl.ioctl(fd, IOC_RELEASE_SEM, release)
            # clean up
            h = ctypes.c_uint32(handle)
            fcntl.ioctl(fd, IOC_CLOSE, h)
            return r.fail("should have returned EOVERFLOW")
        except OSError as e:
            h = ctypes.c_uint32(handle)
            fcntl.ioctl(fd, IOC_CLOSE, h)
            if e.errno == 75:  # EOVERFLOW
                return r.ok(errno="EOVERFLOW")
            return r.fail(e)
    except OSError as e:
        return r.fail(e)


@timed
def test_mutex_wrong_owner(fd):
    r = Result("mutex_wrong_owner")
    try:
        args = MutexArgs(handle=0, owner_tid=999999, count=0, prev_count=0)
        fcntl.ioctl(fd, IOC_CREATE_MUTEX, args)
        handle = args.handle

        release = MutexArgs(handle=handle, owner_tid=1, count=0, prev_count=0)
        try:
            fcntl.ioctl(fd, IOC_RELEASE_MUTEX, release)
            h = ctypes.c_uint32(handle)
            fcntl.ioctl(fd, IOC_CLOSE, h)
            return r.fail("should have returned EPERM")
        except OSError as e:
            h = ctypes.c_uint32(handle)
            fcntl.ioctl(fd, IOC_CLOSE, h)
            if e.errno == 1:  # EPERM
                return r.ok(errno="EPERM")
            return r.fail(e)
    except OSError as e:
        return r.fail(e)


def run_tests():
    results = []

    # Phase 0: device open
    r = test_open()
    results.append(r)
    if not r.passed:
        return results

    fd = os.open(DEVICE, os.O_RDWR)

    # Phase 1: create objects
    r_sem = test_create_semaphore(fd)
    results.append(r_sem)

    r_mtx = test_create_mutex(fd)
    results.append(r_mtx)

    r_evt = test_create_event(fd)
    results.append(r_evt)

    # Phase 2: operations
    if r_sem.passed:
        results.append(test_release_semaphore(fd, r_sem.details["handle"]))

    if r_mtx.passed:
        results.append(test_release_mutex(fd, r_mtx.details["handle"],
                                          r_mtx.details["owner_tid"]))

    if r_evt.passed:
        results.append(test_set_event(fd, r_evt.details["handle"]))
        results.append(test_reset_event(fd, r_evt.details["handle"]))
        results.append(test_pulse_event(fd, r_evt.details["handle"]))

    # Phase 3: message queue round-trip
    results.append(test_post_message(fd))
    results.append(test_get_message(fd))
    results.append(test_get_message_empty(fd))

    # Phase 4: error paths
    results.append(test_close_invalid(fd))
    results.append(test_sem_overflow(fd))
    results.append(test_mutex_wrong_owner(fd))

    # Phase 5: cleanup
    if r_sem.passed:
        results.append(test_close_handle(fd, r_sem.details["handle"], "semaphore"))
    if r_mtx.passed:
        results.append(test_close_handle(fd, r_mtx.details["handle"], "mutex"))
    if r_evt.passed:
        results.append(test_close_handle(fd, r_evt.details["handle"], "event"))

    os.close(fd)
    return results


# ── Prometheus exposition format ──

LOG_DIR = Path.home() / ".cache" / "triskelion"


def emit_prometheus(results, f):
    ts = int(time.time() * 1000)

    f.write("# HELP triskelion_test_passed Whether each ioctl test passed (1) or failed (0).\n")
    f.write("# TYPE triskelion_test_passed gauge\n")
    for r in results:
        v = 1 if r.passed else 0
        f.write(f'triskelion_test_passed{{test="{r.name}"}} {v} {ts}\n')

    f.write("\n")
    f.write("# HELP triskelion_test_duration_nanoseconds Time taken for each ioctl test.\n")
    f.write("# TYPE triskelion_test_duration_nanoseconds gauge\n")
    for r in results:
        f.write(f'triskelion_test_duration_nanoseconds{{test="{r.name}"}} {r.duration_ns} {ts}\n')

    f.write("\n")
    f.write("# HELP triskelion_tests_total Total number of tests executed.\n")
    f.write("# TYPE triskelion_tests_total counter\n")
    f.write(f"triskelion_tests_total {len(results)} {ts}\n")

    f.write("\n")
    f.write("# HELP triskelion_tests_passed_total Total number of tests that passed.\n")
    f.write("# TYPE triskelion_tests_passed_total counter\n")
    passed = sum(1 for r in results if r.passed)
    f.write(f"triskelion_tests_passed_total {passed} {ts}\n")

    f.write("\n")
    f.write("# HELP triskelion_tests_failed_total Total number of tests that failed.\n")
    f.write("# TYPE triskelion_tests_failed_total counter\n")
    failed = sum(1 for r in results if not r.passed)
    f.write(f"triskelion_tests_failed_total {failed} {ts}\n")

    f.write("\n")
    for r in results:
        if not r.passed:
            f.write(f"# FAIL {r.name}: {r.error}\n")


def find_latest_prom():
    """Find the most recent .prom file in LOG_DIR."""
    if not LOG_DIR.exists():
        return None
    proms = sorted(LOG_DIR.glob("triskelion-*.prom"))
    return proms[-1] if proms else None


def load_previous():
    """Parse most recent .prom file for duration comparison."""
    prev = {}
    path = find_latest_prom()
    if not path:
        return prev, None
    try:
        for line in path.read_text().splitlines():
            if line.startswith("triskelion_test_duration_nanoseconds{"):
                name = line.split('"')[1]
                ns = int(line.split("} ")[1].split()[0])
                prev[name] = ns
    except (ValueError, OSError):
        pass
    return prev, path


def print_comparison(results, prev):
    """Print before/after timing comparison."""
    if not prev:
        return

    print("\n── timing comparison (previous → current) ──\n")
    print(f"  {'test':<25} {'prev':>10} {'now':>10} {'delta':>10}")
    print(f"  {'─' * 25} {'─' * 10} {'─' * 10} {'─' * 10}")

    for r in results:
        if r.name in prev:
            old = prev[r.name]
            new = r.duration_ns
            diff = new - old
            sign = "+" if diff >= 0 else ""
            pct = ((new - old) / old * 100) if old else 0
            marker = " ◀" if diff < -500 else ""
            print(f"  {r.name:<25} {old:>8} ns {new:>8} ns {sign}{diff:>7} ns ({pct:+.0f}%){marker}")

    print()


if __name__ == "__main__":
    prev, prev_path = load_previous()

    results = run_tests()

    stamp = time.strftime("%Y%m%d-%H%M%S")
    header = f"# triskelion kernel module test — {time.strftime('%Y-%m-%d %H:%M:%S')}\n"
    header += f"# pid {os.getpid()}, uid {os.getuid()}\n\n"

    LOG_DIR.mkdir(parents=True, exist_ok=True)
    prom_path = LOG_DIR / f"triskelion-{stamp}.prom"
    with open(prom_path, "w") as f:
        f.write(header)
        emit_prometheus(results, f)

    # Also print to terminal
    sys.stdout.write(header)
    emit_prometheus(results, sys.stdout)

    passed = sum(1 for r in results if r.passed)
    total = len(results)

    if passed == total:
        print(f"simpatico: {passed}/{total} — {prom_path}")
    else:
        print(f"{passed}/{total} passed — {prom_path}")
        for r in results:
            if not r.passed:
                print(f"  FAIL  {r.name}: {r.error}")
        sys.exit(1)

    if prev_path:
        print(f"  comparing against: {prev_path.name}")
    print_comparison(results, prev)
