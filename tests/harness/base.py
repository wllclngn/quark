"""Shared constants and small helpers for the harness: the clocks, the
log paths, the default game and the syscall names a snapshot prints."""
import time
from dataclasses import dataclass, field
from quark_common import LOG_DIR


def clock_raw_ns() -> int:
    """CLOCK_MONOTONIC_RAW in nanoseconds — hardware TSC, no NTP slew."""
    return time.clock_gettime_ns(time.CLOCK_MONOTONIC_RAW)


def ns_to_ms(ns: int) -> float:
    return ns / 1_000_000


WINE_INIT_GLOB = str(LOG_DIR / "wine_init_*.log")


DEFAULT_APPID = "2379780"  # Balatro


DEFAULT_TIMEOUT = 30


# x86_64 syscall numbers → names
SYSCALL_NAMES = {
    0: "read", 1: "write", 3: "close", 7: "poll", 8: "lseek",
    16: "ioctl", 23: "select", 35: "nanosleep", 44: "sendto",
    45: "recvfrom", 46: "sendmsg", 47: "recvmsg", 56: "clone",
    59: "execve", 61: "wait4", 202: "futex", 228: "clock_gettime",
    230: "clock_nanosleep", 232: "epoll_wait", 270: "ppoll",
    281: "epoll_pwait", 435: "epoll_pwait2",
}


@dataclass
class ProcSnapshot:
    pid: int
    comm: str = ""
    state: str = ""
    wchan: str = ""
    syscall_nr: int = -1
    syscall_name: str = "?"
    syscall_args: str = ""
    fd_count: int = 0
    key_fds: list = field(default_factory=list)
    cmdline: str = ""


# Stderr analysis
WINE_STDERR_LOG = LOG_DIR / "wine_stderr.log"


WINE_STDOUT_LOG = LOG_DIR / "wine_stdout.log"


LAUNCHER_STDERR_LOG = LOG_DIR / "launcher_stderr.log"
