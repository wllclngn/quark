#!/usr/bin/env python3
"""
QEMU guest for quark's automated suite: install quark for real, launch the
probes through it and run the test layers, in a throwaway VM. A failure in
here strands a disposable guest, never the host's Steam session or Wine.

Disk layers, all under ~/.cache/quark-qemu/:
  Arch-Linux-x86_64-cloudimg.qcow2  the official Arch cloud image,
                                    checksum-verified, never written
  provisioned.qcow2                 overlay on it: Wine, the build
                                    toolchain, mingw, Xvfb, lavapipe and a
                                    stand-in Steam root
  run.qcow2                         overlay on provisioned, recreated for
                                    every run, so each run starts clean
  build-cache.qcow2                 a second disk that persists across
                                    runs and provisions, mounted at the
                                    Wine build dir so install.py's manifest
                                    gate can reuse the patched Wine build
                                    and montauk's build is incremental

Nothing on the host needs root: KVM, user-mode networking (ssh forwarded
to a free localhost port) and cloud-init's NoCloud seed served over HTTP
to the guest at 10.0.2.2.

Usage:
  tests/run.py qemu fetch       download + verify the cloud image
  tests/run.py qemu provision   build provisioned.qcow2 (once)
  tests/run.py qemu test [-k NAME]
                                  push the repo, run tests/qemu/suite.py,
                                  copy LOG_DIR and SUITE_DIR back to
                                  SUITE_DIR/qemu/<stamp> on the host
  tests/run.py qemu shell       fresh run overlay + an interactive ssh
"""

import functools
import hashlib
import http.server
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import urllib.request
from datetime import datetime
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from quark_common import (
    REPO_ROOT, MONTAUK_SRC, LOG_DIR, SUITE_DIR, QEMU_CACHE as CACHE, QEMU_READY,
    log_info, log_error,
)

IMAGE_URL = "https://geo.mirror.pkgbuild.com/images/latest/Arch-Linux-x86_64-cloudimg.qcow2"
BASE = CACHE / "Arch-Linux-x86_64-cloudimg.qcow2"
SUMS = CACHE / "Arch-Linux-x86_64-cloudimg.qcow2.SHA256"
PROVISIONED = CACHE / "provisioned.qcow2"
RUN = CACHE / "run.qcow2"
BUILD_CACHE = CACHE / "build-cache.qcow2"
KEY = CACHE / "id_ed25519"
SERIAL_LOG = CACHE / "serial.log"

GUEST_USER = "arch"
GUEST_REPO = "quark"  # under the guest user's home
GUEST_MONTAUK = "montauk"  # beside it, where quark_common.MONTAUK_SRC resolves
# The build-cache disk's mount point: The guest user's own ~/.cache/quark,
# where the guest's install keeps wine-src and wine-obj.
GUEST_BUILD_DIR = f"/home/{GUEST_USER}/.cache/quark"

PACKAGES = [
    # quark build and install.py
    "base-devel", "git", "python", "rust", "clang", "lld", "llvm", "autoconf",
    "bison", "flex", "mingw-w64-gcc",
    # system Wine and its .NET runtime
    "wine", "wine-mono",
    # Wine build dependencies (install.py compiles the patched modules)
    "gnutls", "libxslt", "libgphoto2", "libpulse", "alsa-lib", "alsa-plugins",
    "openal", "libxcomposite", "libxinerama", "libxrandr", "libxcursor", "libxi",
    "libxxf86vm", "libxdamage", "libxfixes", "libxkbcommon", "wayland",
    "fontconfig", "freetype2", "sdl2-compat", "v4l-utils", "libusb",
    "gst-plugins-base", "gst-plugins-good",
    # software Vulkan + GL, so DXVK renders with no GPU
    "mesa", "vulkan-icd-loader", "vulkan-swrast", "vulkan-tools",
    "lib32-mesa", "lib32-vulkan-swrast", "lib32-vulkan-icd-loader",
    # headless display and an audio endpoint
    "xorg-server-xvfb", "xorg-xauth", "pulseaudio", "pulseaudio-alsa",
    # montauk, built in the guest to trace the probes
    "cmake", "libbpf", "bpf", "liburing",
]

MEMORY = "8192"
CPUS = "8"
BOOT_TIMEOUT = 240


def die(msg: str) -> None:
    log_error(msg)
    sys.exit(1)


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def ensure_key() -> str:
    if not KEY.exists():
        CACHE.mkdir(parents=True, exist_ok=True)
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "quark-qemu",
                        "-f", str(KEY)], check=True)
    return KEY.with_suffix(".pub").read_text().strip()


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def fetch() -> None:
    CACHE.mkdir(parents=True, exist_ok=True)
    if not SUMS.exists():
        urllib.request.urlretrieve(IMAGE_URL + ".SHA256", SUMS)
    want = SUMS.read_text().split()[0]
    if BASE.exists() and sha256(BASE) == want:
        log_info(f"cloud image present and verified: {BASE}")
        return
    part = BASE.with_suffix(".part")
    log_info(f"downloading {IMAGE_URL}")
    urllib.request.urlretrieve(IMAGE_URL, part)
    if sha256(part) != want:
        part.unlink()
        die("cloud image checksum mismatch; download discarded")
    part.rename(BASE)
    log_info(f"verified: {BASE}")


def overlay(path: Path, backing: Path) -> None:
    path.unlink(missing_ok=True)
    subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", "-b", str(backing), "-F", "qcow2",
                    str(path)], check=True)


class _QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args) -> None:
        pass


class SeedServer:
    """cloud-init NoCloud seed: The guest fetches user-data and meta-data
    from http://10.0.2.2:PORT/, which slirp maps to this host loopback."""

    def __init__(self, pubkey: str):
        self.dir = tempfile.TemporaryDirectory(prefix="quark-seed-")
        root = Path(self.dir.name)
        (root / "meta-data").write_text("instance-id: quark-qemu\nlocal-hostname: quark\n")
        (root / "user-data").write_text(f"#cloud-config\nssh_authorized_keys:\n  - {pubkey}\n")
        (root / "vendor-data").write_text("")
        handler = functools.partial(_QuietHandler, directory=str(root))
        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
        self.port = self.httpd.server_address[1]
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def close(self) -> None:
        self.httpd.shutdown()
        self.dir.cleanup()


class Guest:
    def __init__(self, disk: Path, seed: SeedServer | None = None):
        self.disk = disk
        self.seed = seed
        self.port = free_port()
        self.proc: subprocess.Popen | None = None

    def __enter__(self) -> "Guest":
        cmd = [
            "qemu-system-x86_64", "-enable-kvm", "-cpu", "host",
            "-m", MEMORY, "-smp", CPUS,
            "-drive", f"file={self.disk},if=virtio,format=qcow2",
            "-drive", f"file={BUILD_CACHE},if=virtio,format=qcow2",
            "-nic", f"user,model=virtio-net-pci,hostfwd=tcp:127.0.0.1:{self.port}-:22",
            "-display", "none", "-vga", "std",
            "-serial", f"file:{SERIAL_LOG}",
            "-no-reboot",
        ]
        if self.seed:
            cmd += ["-smbios", f"type=1,serial=ds=nocloud;s=http://10.0.2.2:{self.seed.port}/"]
        self.proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL)
        self.wait_ssh()
        return self

    def __exit__(self, *exc) -> None:
        if self.proc and self.proc.poll() is None:
            self.ssh("sudo systemctl poweroff", check=False, timeout=15)
            try:
                self.proc.wait(timeout=60)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()

    def ssh_base(self) -> list[str]:
        return ["ssh", "-i", str(KEY), "-p", str(self.port),
                "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
                "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=3", "-o", "BatchMode=yes",
                f"{GUEST_USER}@127.0.0.1"]

    def scp_base(self) -> list[str]:
        return ["scp", "-q", "-r", "-i", str(KEY), "-P", str(self.port),
                "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
                "-o", "LogLevel=ERROR", "-o", "BatchMode=yes"]

    def ssh(self, command: str, check: bool = True, timeout: float | None = None,
            capture: bool = False) -> subprocess.CompletedProcess:
        return subprocess.run(self.ssh_base() + [command], check=check, timeout=timeout,
                              text=True, capture_output=capture)

    def push(self, local: Path, remote: str) -> None:
        subprocess.run(self.scp_base() + [str(local), f"{GUEST_USER}@127.0.0.1:{remote}"], check=True)

    def pull(self, remote: str, local: Path) -> None:
        subprocess.run(self.scp_base() + [f"{GUEST_USER}@127.0.0.1:{remote}", str(local)], check=False)

    def wait_ssh(self) -> None:
        deadline = time.monotonic() + BOOT_TIMEOUT
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                die(f"qemu exited ({self.proc.returncode}); see {SERIAL_LOG}")
            r = subprocess.run(self.ssh_base() + ["true"], capture_output=True)
            if r.returncode == 0:
                log_info(f"guest up, ssh on 127.0.0.1:{self.port}")
                return
            time.sleep(2)
        die(f"guest never answered ssh within {BOOT_TIMEOUT}s; see {SERIAL_LOG}")


# The stand-in Steam root: What install.py and the launcher look for, with no
# Steam client. Its "Proton" prefix template is a stock `wineboot --init`
# prefix, so install.py's template step bakes a real prefix rather than being
# skipped.
STAND_IN_STEAM = r"""
set -e
steam="$HOME/.local/share/Steam"
mkdir -p "$steam/steamapps/common" "$steam/steamapps/compatdata" "$steam/config" \
         "$steam/compatibilitytools.d" "$HOME/.steam"
ln -sfn "$steam" "$HOME/.steam/root"
tpl="$steam/steamapps/common/Proton 10.0/files/share/default_pfx"
mkdir -p "$(dirname "$tpl")"
Xvfb :98 -screen 0 1024x768x24 >/dev/null 2>&1 & xvfb=$!
# mshtml= skips the Wine Gecko install prompt, which would hang headless.
# Wine names the profile after $USER; Proton's template is steamuser's.
DISPLAY=:98 USER=steamuser WINEPREFIX="$tpl" WINEDEBUG=-all WINEDLLOVERRIDES="mshtml=" wineboot --init
WINEPREFIX="$tpl" wineserver -w
kill $xvfb
test -f "$tpl/system.reg"
"""


def provision() -> None:
    QEMU_READY.unlink(missing_ok=True)
    if not BASE.exists():
        fetch()
    pubkey = ensure_key()
    overlay(PROVISIONED, BASE)
    subprocess.run(["qemu-img", "resize", "-q", str(PROVISIONED), "40G"], check=True)
    if not BUILD_CACHE.exists():
        subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", str(BUILD_CACHE), "40G"], check=True)
    seed = SeedServer(pubkey)
    try:
        # Two boots: The upgrade replaces the kernel, and the running one's
        # modules (ntsync among them) are gone until the guest boots the new one.
        with Guest(PROVISIONED, seed) as g:
            g.ssh("sudo cloud-init status --wait >/dev/null")
            log_info("enabling multilib, upgrading and installing packages")
            g.ssh("printf '\\n[multilib]\\nInclude = /etc/pacman.d/mirrorlist\\n' | sudo tee -a /etc/pacman.conf >/dev/null")
            g.ssh("sudo pacman -Syu --noconfirm --needed " + " ".join(PACKAGES))
            g.ssh("echo ntsync | sudo tee /etc/modules-load.d/ntsync.conf >/dev/null")
        with Guest(PROVISIONED, seed) as g:
            log_info(f"running kernel {g.ssh('uname -r', capture=True).stdout.strip()}")
            g.ssh("test -e /dev/ntsync || sudo modprobe ntsync")
            log_info(f"mounting the build-cache disk at {GUEST_BUILD_DIR}")
            g.ssh("sudo blkid -L quarkcache >/dev/null || sudo mkfs.ext4 -q -L quarkcache /dev/vdb")
            g.ssh(f"echo 'LABEL=quarkcache {GUEST_BUILD_DIR} ext4 defaults,nofail 0 2' | sudo tee -a /etc/fstab >/dev/null"
                  f" && sudo mkdir -p {GUEST_BUILD_DIR} && sudo mount {GUEST_BUILD_DIR}"
                  f" && sudo chown {GUEST_USER}: {GUEST_BUILD_DIR}")
            log_info("building the stand-in Steam root and its prefix template")
            g.ssh(STAND_IN_STEAM)
            g.ssh("sudo touch /etc/cloud/cloud-init.disabled")
            log_info("provisioned")
    finally:
        seed.close()
    QEMU_READY.touch()
    log_info(f"wrote {PROVISIONED} and {BUILD_CACHE}")


def repo_tarball(dest: Path) -> Path:
    """quark's and montauk's working trees as git sees them (tracked plus
    untracked, minus ignored), so uncommitted work is what the guest tests
    and traces with."""
    tar_path = dest / "quark.tar"
    with tarfile.open(tar_path, "w") as tar:
        for root, arc in ((REPO_ROOT, GUEST_REPO), (MONTAUK_SRC, GUEST_MONTAUK)):
            files = subprocess.run(["git", "ls-files", "-co", "--exclude-standard", "-z"],
                                   cwd=root, capture_output=True, check=True).stdout.split(b"\0")
            for rel in filter(None, (f.decode() for f in files)):
                path = root / rel
                if path.exists():
                    tar.add(path, arcname=f"{arc}/{rel}", recursive=False)
    return tar_path


def run_tests(selector: str | None) -> int:
    if not QEMU_READY.exists():
        die("no finished provision; run: tests/run.py qemu provision")
    out = SUITE_DIR / "qemu" / datetime.now().strftime("%Y%m%d-%H%M%S")
    for sub in ("quark", "suite"):
        (out / sub).mkdir(parents=True, exist_ok=True)
    overlay(RUN, PROVISIONED)
    with tempfile.TemporaryDirectory(prefix="quark-push-") as tmp:
        tar_path = repo_tarball(Path(tmp))
        with Guest(RUN) as g:
            # An image provisioned before the mount point was the guest user's
            # mounts the cache elsewhere; mount it where the install looks.
            g.ssh(f"mountpoint -q {GUEST_BUILD_DIR} || {{ sudo mkdir -p {GUEST_BUILD_DIR}"
                  f" && sudo mount -L quarkcache {GUEST_BUILD_DIR} && sudo chown {GUEST_USER}: {GUEST_BUILD_DIR}; }}")
            g.push(tar_path, "/tmp/quark.tar")
            g.ssh(f"rm -rf ~/{GUEST_REPO} ~/{GUEST_MONTAUK} && tar -xf /tmp/quark.tar -C ~ && rm /tmp/quark.tar")
            args = f" -k {selector}" if selector else ""
            # The guest's suite reports inside the host's run: A child, no INTRO.
            rc = g.ssh(f"cd ~/{GUEST_REPO} && QUARK_CHILD=1 python3 tests/qemu/suite.py{args}",
                       check=False).returncode
            g.pull(f"{LOG_DIR}/.", out / "quark")
            g.pull(f"{SUITE_DIR}/.", out / "suite")
    log_info(f"suite exited {rc}; guest logs and verdict in {out}")
    return rc


def shell() -> None:
    if not QEMU_READY.exists():
        die("no finished provision; run: tests/run.py qemu provision")
    overlay(RUN, PROVISIONED)
    with Guest(RUN) as g:
        subprocess.run(g.ssh_base())


def main(command: str, only: str | None = None) -> int:
    for tool in ("qemu-system-x86_64", "qemu-img", "ssh", "scp", "ssh-keygen", "git"):
        if not shutil.which(tool):
            die(f"{tool} not found")
    if command == "fetch":
        fetch()
    elif command == "provision":
        provision()
    elif command == "test":
        return run_tests(only)
    else:
        shell()
    return 0
