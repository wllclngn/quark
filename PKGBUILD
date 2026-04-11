# Maintainer: wllclngn <https://github.com/wllclngn>
pkgname=quark
pkgver=0.13.0
pkgrel=1
pkgdesc="Linux gaming stack: Proton launcher, wineserver replacement, display compositor"
arch=('x86_64')
url="https://github.com/wllclngn/personal"
license=('GPL-2.0-only')
depends=(
    'wine'
    'vulkan-driver'
    'python'
    'steam'
)
makedepends=(
    'rust'
    'cargo'
    'git'
    'clang'
    'lld'
    'autoconf'
)
optdepends=(
    'dxvk: DirectX 9/10/11 to Vulkan translation'
    'vkd3d: DirectX 12 to Vulkan translation'
    'lib32-vulkan-driver: 32-bit Vulkan support'
)
provides=('quark' 'triskelion' 'sybil')
source=("git+https://github.com/wllclngn/personal.git#tag=v${pkgver}")
sha256sums=('SKIP')

_srcdir="personal/PROGRAMMING/SYSTEM PROGRAMS/LINUX/quark"

prepare() {
    cd "${srcdir}/${_srcdir}"
    export RUSTUP_TOOLCHAIN=stable
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
    cd "${srcdir}/${_srcdir}"
    export RUSTUP_TOOLCHAIN=stable
    export CARGO_TARGET_DIR=target
    cargo build --release --frozen
}

package() {
    cd "${srcdir}/${_srcdir}"

    # Binaries
    install -Dm755 target/release/quark "${pkgdir}/usr/lib/quark/quark"
    install -Dm755 target/release/triskelion "${pkgdir}/usr/lib/quark/triskelion"
    install -Dm755 target/release/sybil "${pkgdir}/usr/lib/quark/sybil"

    # Steam expects "proton" entry point
    ln -s quark "${pkgdir}/usr/lib/quark/proton"

    # Install script (prefix setup, DXVK/VKD3D deployment)
    install -Dm755 install.py "${pkgdir}/usr/lib/quark/install.py"

    # Patches
    install -d "${pkgdir}/usr/lib/quark/patches"
    cp -a patches/* "${pkgdir}/usr/lib/quark/patches/" 2>/dev/null || true

    # Compute shader
    install -d "${pkgdir}/usr/lib/quark/shaders"
    install -Dm644 rust/src/sybil/shaders/downscale.comp \
        "${pkgdir}/usr/lib/quark/shaders/downscale.comp"

    # Steam compatibility tool VDFs
    install -d "${pkgdir}/usr/share/steam/compatibilitytools.d/quark"
    cat > "${pkgdir}/usr/share/steam/compatibilitytools.d/quark/compatibilitytool.vdf" <<EOF
"compatibilitytools"
{
  "compat_tools"
  {
    "quark"
    {
      "install_path" "/usr/lib/quark"
      "display_name" "quark ${pkgver}"
      "from_oslist"  "windows"
      "to_oslist"    "linux"
    }
  }
}
EOF
    cat > "${pkgdir}/usr/share/steam/compatibilitytools.d/quark/toolmanifest.vdf" <<EOF
"manifest"
{
  "commandline" "/proton %verb%"
  "version" "2"
  "use_sessions" "1"
}
EOF

    # License
    install -Dm644 LICENSE "${pkgdir}/usr/share/licenses/${pkgname}/LICENSE"
}
