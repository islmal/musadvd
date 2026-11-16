# MusaDVD

DVD, Blu-ray and AVCHD authoring in Rust: a desktop app (Tauri; German, English, Russian) and a
CLI that turn video files into a menu-driven disc structure, an image, or a burned disc.

Not affiliated with VSO Software. ConvertX templates are not included; they are only read from a
user's own ConvertXToDVD installation.

## Features

- DVD-Video, Blu-ray and AVCHD output, native muxing and IFO/BDMV writers (no `dvdauthor`/`tsMuxeR` needed)
- Menus from ConvertX templates or custom layouts (built-in WYSIWYG menu editor)
- **Burn Studio**: read, build, verify and burn images (UDF 1.02 + ISO 9660), Discovery media tests, layer-break tools
- `ffmpeg`-based encoding with automatic GPU decoding (`d3d11va`/`cuda`/`qsv` on Windows, `vaapi`/`cuda`/`qsv` on Linux) and optional GPU encoding

## Installation

Prebuilt packages are published automatically on [GitHub Releases](https://github.com/islmal/musadvd/releases) (tagged releases and rolling `nightly` builds).

### Windows

- **Installer (recommended):** Download `MusaDVD_<version>_x64-setup.exe` and run it (includes `ffmpeg`/`ffprobe` and auto-updater).
- **Portable:** Download `MusaDVD-<tag>-windows-x64-portable.zip`, extract it, and run `musadvd-gui.exe` or `musadvd.exe`.

### Linux (Ubuntu / Debian / Linux Mint)

1. Download `MusaDVD_<version>_amd64.deb` from Releases and install it:
   ```bash
   sudo apt update
   sudo apt install ./MusaDVD_*_amd64.deb
   ```
2. Allow your user to access optical drives (`/dev/sr0`), then log out and back in:
   ```bash
   sudo usermod -aG cdrom "$USER"
   ```

### Linux (Any Distro — AppImage or Portable)

1. Install runtime dependencies:
   - **Debian/Ubuntu/Mint:** `sudo apt install ffmpeg xorriso dvd+rw-tools`
   - **Fedora:** `sudo dnf install ffmpeg xorriso dvd+rw-tools` *(with RPM Fusion enabled for `ffmpeg`)*
   - **Arch Linux:** `sudo pacman -S ffmpeg libisoburn dvd+rw-tools`
2. Add your user to the optical drive group (`cdrom` on Debian/Ubuntu/Fedora, `optical` on Arch) and re-login.
3. Download either:
   - **`MusaDVD_<version>_amd64.AppImage`:** `chmod +x MusaDVD_*.AppImage && ./MusaDVD_*.AppImage`
   - **`MusaDVD-<tag>-linux-x86_64-portable.tar.gz`:** extract and run `./musadvd-gui` or `./musadvd`.

### Arch Linux (AUR / PKGBUILD)

A ready-to-use PKGBUILD is included at [`packaging/aur/PKGBUILD`](packaging/aur/PKGBUILD):

```bash
cd packaging/aur
makepkg -si
```

## Build from Source

### Windows

Copy `ffmpeg.exe` and `ffprobe.exe` (gyan.dev full build) into `bin/`, then:

```powershell
npm install
npm run build
cargo build --release --bin musadvd
cargo build --release -p musadvd-gui --features tauri/custom-protocol
```

### Linux (Ubuntu 24.04 / Linux Mint 22)

```bash
sudo apt update
sudo apt install -y \
  build-essential curl wget file git patchelf \
  libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev \
  libayatana-appindicator3-dev libssl-dev libxdo-dev \
  ffmpeg xorriso dvd+rw-tools
sudo usermod -aG cdrom "$USER"

npm install
npm run build
cargo build --release --bin musadvd
cargo build --release -p musadvd-gui --features tauri/custom-protocol
```

## CLI

```bash
musadvd drives
musadvd --help
```

## Licence & credits

GPL-3.0-or-later, see [LICENSE](LICENSE). Derived and bundled components (`dvdauthor`, `tsMuxer`,
DejaVu, FFmpeg): [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
