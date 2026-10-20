# Maintainer: wllclngn <https://github.com/wllclngn>
#
# The package installs the launcher binary, the installer and the patches
# under /usr/lib/quark. The compatibility tool itself is built per user by
# `quark-install`: It deploys the system Wine tree, builds the patched
# modules from the matching Wine source, bakes the prefix template from the
# user's Proton 11.0 and fetches DXVK and VKD3D-Proton, all under
# ~/.local/share/Steam/compatibilitytools.d/quark. That is why the Wine
# build toolchain is a runtime dependency of this package.
pkgname=quark
pkgver=0.3.0
pkgrel=1
epoch=1
pkgdesc="Proton replacement launcher on the stock Wine wineserver, for Steam"
arch=('x86_64')
url="https://github.com/wllclngn/quark"
license=('GPL-2.0-only')
depends=(
    'wine'
    'steam'
    'python'
    'git'
    # quark-install builds the tier0/vstdlib stubs and the patched Wine
    # modules on the user's machine: The host side with gcc, the PE side
    # with clang and lld.
    'gcc'
    'make'
    'patch'
    'pkgconf'
    'clang'
    'lld'
    'llvm'
    'autoconf'
    'bison'
    'flex'
    'gnutls'
    'libxslt'
    'libgphoto2'
    'libpulse'
    'alsa-lib'
    'alsa-plugins'
    'openal'
    'libxcomposite'
    'libxinerama'
    'libxrandr'
    'libxcursor'
    'libxi'
    'libxxf86vm'
    'libxdamage'
    'libxfixes'
    'libxkbcommon'
    'wayland'
    'fontconfig'
    'freetype2'
    'sdl2-compat'
    'v4l-utils'
    'libusb'
    'gst-plugins-base'
    'gst-plugins-good'
    'vulkan-icd-loader'
    'lib32-vulkan-icd-loader'
)
makedepends=(
    'cargo'
)
optdepends=(
    'wine-mono: the runtime .NET and FNA games run on; quark-install warns without it'
    'lib32-vulkan-driver: 32-bit Vulkan for the GPU (lib32-nvidia-utils, lib32-vulkan-radeon, lib32-vulkan-intel)'
    'lib32-libpulse: 32-bit audio through PulseAudio or pipewire-pulse'
    'montauk: the launch observer (QUARK_TRACE=1) and the eBPF captures the test harness reads'
)
source=("git+https://github.com/wllclngn/quark.git#tag=v${pkgver}")
sha256sums=('SKIP')
# The release profile strips the binary itself (Cargo.toml), so a split
# debug package would hold nothing.
options=('!debug')

prepare() {
    cd "${srcdir}/${pkgname}"
    export RUSTUP_TOOLCHAIN=stable
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
    cd "${srcdir}/${pkgname}"
    export RUSTUP_TOOLCHAIN=stable
    export CARGO_TARGET_DIR=target
    cargo build --release --frozen -p quark
}

check() {
    cd "${srcdir}/${pkgname}"
    export RUSTUP_TOOLCHAIN=stable
    export CARGO_TARGET_DIR=target
    cargo test --release --frozen -p quark
}

package() {
    cd "${srcdir}/${pkgname}"
    local lib="${pkgdir}/usr/lib/quark"

    install -Dm755 target/release/quark "${lib}/quark"
    install -Dm755 install.py "${lib}/install.py"
    install -Dm644 quark_common.py "${lib}/quark_common.py"
    # The installer reads the version from here.
    install -Dm644 rust/Cargo.toml "${lib}/rust/Cargo.toml"
    install -d "${lib}/patches/wine" "${lib}/patches/lsteamclient"
    install -m644 patches/wine/*.patch "${lib}/patches/wine/"
    install -m644 patches/lsteamclient/*.patch "${lib}/patches/lsteamclient/"

    install -d "${pkgdir}/usr/bin"
    ln -s /usr/lib/quark/install.py "${pkgdir}/usr/bin/quark-install"

    install -Dm644 LICENSE "${pkgdir}/usr/share/licenses/${pkgname}/LICENSE"
    install -Dm644 README.md "${pkgdir}/usr/share/doc/${pkgname}/README.md"
}
