# Releasing djforge

Every push to the `release` branch runs `.github/workflows/release.yml`. The workflow builds
signed installers for macOS, Windows, Linux, FreeBSD and the web, then creates or updates a
**draft** GitHub Release named `djforge v<version>`. Nobody sees a draft until a maintainer
publishes it.

This is djforge's implementation (inherited from SoundCraft) of the shared [release playbook](release-playbook.md).
User-facing names say **djforge**. Files, binaries and ids stay lowercase
(`djforge-<version>-<platform>-<arch>.<ext>`, `io.github.mloki23.djforge`).

## Cutting a release

1. **Bump the version** on `main`. The only place it lives is `[workspace.package] version` in
   the root `Cargo.toml`; the app reads it from `CARGO_PKG_VERSION` (*About djforge*,
   `djforge --version`, `djforge-cli --version`).

   ```sh
   cargo xtask version                 # prints the current version, e.g. 0.1.0
   cargo xtask version set 0.2.0       # or 0.2.0-rc.1; updates Cargo.toml and Cargo.lock
   ```

   Commit the change (`Cargo.toml` + `Cargo.lock`) through the normal review flow.
2. **Merge `main` into `release`** (or fast-forward it) and push. The workflow starts by itself.
3. **Wait for the draft.** After about 30 to 60 minutes (notarization and the FreeBSD VM are the
   slow parts), the Releases page has a draft `djforge v0.2.0`, tagged `v0.2.0` on the pushed
   commit, with every artifact and `SHA256SUMS.txt`. The notes are generated from the merged PRs.
4. **Check it.** Download an installer or two and look at the job summaries. Any `::warning::`
   there means a signing secret was missing and that artifact is unsigned.
5. **Publish** the draft in the GitHub UI. Publishing creates the `v0.2.0` tag. Versions with a
   pre-release suffix (`-rc.1`) are marked as pre-releases.

Pushing to `release` again before you publish rebuilds the same draft and replaces its assets.
After the draft is published, the workflow refuses to touch that version again, so bump it first.

**Test runs:** *Actions → Release → Run workflow* runs the whole pipeline by hand. The optional
`version` input (such as `0.2.0-rc.1`) overrides `Cargo.toml` for that run only. The jobs apply it
with `cargo xtask version set` before building, so the binaries report it too. Only the signing
jobs (macOS, Windows) and the draft-release job use the `release` environment, which only the
`release` branch can use. On any other branch the run is a dry run: Linux, Flatpak, FreeBSD and
web build and are checked, the signing jobs are refused, and no release is drafted.

## What gets built

| Platform | Artifacts | Built on |
|---|---|---|
| macOS 11+ (universal: Apple silicon + Intel) | `djforge-<v>-macos-universal.dmg`, `djforge-cli-<v>-macos-universal.zip` | `macos-15` |
| Windows 10+ x64 | `djforge-<v>-windows-x64.msi`, `djforge-<v>-windows-x64-portable.zip` | `windows-latest` |
| Windows 10+ x86 (32-bit) | `djforge-<v>-windows-x86.msi`, `djforge-<v>-windows-x86-portable.zip` | `windows-latest` |
| Windows 11 ARM64 | `djforge-<v>-windows-arm64.msi`, `djforge-<v>-windows-arm64-portable.zip` | `windows-latest` (cross-compiled; `windows-arm64.yml` installs and runs it on ARM64) |
| Linux x86_64 | `djforge-<v>-linux-x86_64.{AppImage,deb,rpm,tar.gz}` | `ubuntu-22.04` |
| Linux aarch64 | `djforge-<v>-linux-aarch64.{AppImage,deb,rpm,tar.gz}` | `ubuntu-22.04-arm` |
| Linux riscv64 | `djforge-<v>-linux-riscv64.tar.gz` (glibc 2.39+) | `ubuntu-24.04` (cross-compiled; CLI smoke-tested under QEMU) |
| AppImage updates | `djforge-<v>-linux-{x86_64,aarch64}.AppImage.zsync` | with the AppImage |
| Flatpak x86_64, aarch64 | `djforge-<v>-linux-{x86_64,aarch64}.flatpak` | `ubuntu-24.04`, `ubuntu-24.04-arm` (repackages the Linux tarball) |
| FreeBSD 14 x86_64 | `djforge-<v>-freebsd-x86_64.tar.gz` (a `/usr/local` tree) | FreeBSD 14.3 VM on `ubuntu-latest` |
| Web | `djforge-web-<v>.zip` (static site; see [`packaging/web/README.md`](../packaging/web/README.md)) | `ubuntu-latest` |

The from-source Flatpak manifest (`packaging/linux/flatpak/io.github.mloki23.djforge.yml`) is
kept ready for a Flathub submission; the release's `.flatpak` bundles come from
`io.github.mloki23.djforge.bundle.yml` (see Linux below).

### Audio libraries

djforge plays and records through [cpal](https://github.com/RustAudio/cpal): CoreAudio on
macOS, WASAPI on Windows, ALSA on Linux and FreeBSD (PulseAudio and PipeWire through their ALSA
plugins), and WebAudio in the browser.

- **Linux** builds need the ALSA headers (`libasound2-dev` and `pkg-config` on Ubuntu). The binary
  links `libasound.so.2`, so the `.deb` depends on `libasound2 | libasound2t64` and the `.rpm` on
  `alsa-lib`; the AppImage and tarball expect it on the host (every desktop distro has it).
- **FreeBSD** builds need `alsa-lib` (`pkg install alsa-lib`); at runtime `alsa-plugins-oss`
  routes ALSA to the OSS `sound(4)` devices. Without a device the player falls back to its
  silent clock, so the transport still runs.
- **Flatpak**: alsa-lib and its PulseAudio plugin come with the freedesktop runtime; the
  manifests grant `--socket=pulseaudio` (PipeWire's pulse server answers on it). No raw ALSA
  devices and no JACK: djforge doesn't use either.
- **Web**: browsers start audio only after a user gesture, so the first click in the page may be
  needed before sound plays. The web build has no recording.

### macOS

`packaging/macos/package.sh` builds `aarch64-apple-darwin` and `x86_64-apple-darwin` with
`MACOSX_DEPLOYMENT_TARGET=11.0`, joins them with `lipo`, and assembles `djforge.app`:

- `Info.plist` is generated from `Info.plist.in`. The bundle id is `io.github.mloki23.djforge`.
  It declares the `.djforge` session type (and `.scraft`) (`io.github.mloki23.djforge.session`, MIME
  `application/x-soundcraft-session`, role Editor, rank Owner) and lists WAV, AIFF/AIFC, FLAC,
  MP3 and MIDI as Viewer/Alternate, so djforge appears under *Open With* without taking over
  the default player. `NSMicrophoneUsageDescription` explains the recording prompt.
- **Signing** goes inside-out with the hardened runtime and a secure timestamp. The executable is
  signed first, then the bundle. There's no `--deep` on the final signature. `entitlements.plist`
  holds a single exception, `com.apple.security.device.audio-input`, without which the hardened
  runtime blocks recording.
- **Notarization:** the app is zipped and sent with `xcrun notarytool submit --wait`, then the
  ticket is stapled to the app. The app goes on a DMG (`hdiutil`, with an `Applications` link).
  The DMG is signed, notarized and stapled too.
  Its Finder window (background, icon size and positions) comes from
  [`packaging/macos/dmg/`](../packaging/macos/dmg/README.md), and its volume is named `djforge`
  without the version, which the window's background needs; the DMG file name keeps the version.
- **CLI:** the universal `djforge-cli` is signed with the hardened runtime, zipped and
  notarized.

Locally, without certificates, the script signs ad-hoc (`codesign -s -`) and skips notarization:

```sh
packaging/macos/package.sh                    # universal; needs both rustup targets
packaging/macos/package.sh --arch aarch64     # quicker, host-only
open dist/release/djforge-*-macos-*.dmg
```

### Windows

`packaging/windows/package.ps1 -Arch x64|x86|arm64` builds with `-C target-feature=+crt-static`
(in `CARGO_TARGET_<TRIPLE>_RUSTFLAGS`, so host build scripts aren't affected), so neither the MSI
nor the portable zip needs the Visual C++ redistributable.

- `apps/soundcraft/build.rs` embeds the icon (`assets/app-icon/djforge.ico`) and VERSIONINFO
  with `winresource`, only when targeting Windows. Release packaging sets
  `SOUNDCRAFT_REQUIRE_WINRES=1` so a missing resource compiler fails the build.
- The script checks both PE headers: the machine type matches `-Arch`, `djforge.exe` is a GUI
  app (no console window) and `djforge-cli.exe` a console app.
- `djforge.wxs` (WiX v5) is a per-machine install into Program Files with an advertised Start
  Menu shortcut. djforge becomes the default app for `.djforge` and `.scraft` and is listed under *Open with*
  for `.wav`, `.aif`, `.aiff`, `.flac`, `.mp3`, `.mid` and `.midi`. It also registers App Paths
  (Win+R `djforge`). The MSI version is the numeric `X.Y.Z`; same-version upgrades let release
  candidates replace each other. The upgrade code `EE53D3E5-D0AE-40F9-808D-9417FDEBF61A` is fixed
  forever.
- **Signing:** `packaging/windows/sign.ps1` signs both `.exe` files and then the `.msi` with
  `signtool` (SHA-256, RFC 3161 timestamp), using a `.pfx` (`WINDOWS_CERTIFICATE*`) if present,
  otherwise Azure Trusted Signing (all six `AZURE_*`), otherwise nothing (a warning).

Locally on Windows: `dotnet tool install -g wix --version 5.0.2`, then
`pwsh packaging/windows/package.ps1 -Arch x64`.

### Linux

`packaging/linux/package.sh` stages one FHS tree and makes every format from it: both binaries,
`io.github.mloki23.djforge.desktop` (with `MimeType=` for sessions, WAV, AIFF, FLAC, MP3 and
MIDI), hicolor icons from 16 px to 512 px plus a scalable SVG, AppStream metainfo, and a
shared-mime-info file declaring `application/x-soundcraft-session` (`*.djforge`, `*.scraft`).

- **AppImage** (maintained `AppImage/appimagetool`, static runtime: no libfuse2 needed). Each
  embeds the update information
  `gh-releases-zsync|mloki23|djforge|latest|djforge-*-linux-<arch>.AppImage.zsync`, and
  the `.zsync` next to it (written when `zsyncmake` from the `zsync` package is installed, as in
  CI) lets AppImageUpdate fetch only the changed blocks of the newest non-pre-release.
- **.deb** and **.rpm** by [nfpm](https://nfpm.goreleaser.com/) from one `nfpm.yaml`; the
  `postinst.sh` hook refreshes the desktop, MIME and icon caches.
- **.tar.gz** for people who manage their own `/opt` or `~/.local`.
- **Flatpak**: the `flatpak` job runs `packaging/linux/flatpak-bundle.sh` on the Linux job's
  tarball (no Rust build) with `flatpak/io.github.mloki23.djforge.bundle.yml`, then installs the
  bundle and runs `djforge-cli --version` in the sandbox. Install with
  `flatpak install --user djforge-<v>-linux-<arch>.flatpak`. Both manifests use the freedesktop
  25.08 runtime with Wayland/X11, `dri`, PulseAudio and `xdg-music`; other files go through the
  portals. For the VST3 and CLAP hosts they grant `~/.vst3` and `~/.clap` read-only and mount
  Flathub's `org.freedesktop.LinuxAudio.Plugins` extensions (pointed to by `VST3_PATH` and
  `CLAP_PATH`); system plugin folders aren't visible in the sandbox.
  `io.github.mloki23.djforge.yml` builds from source for Flathub and needs vendored crates
  (`cargo-sources.json` from `flatpak-cargo-generator.py`); the commands are in its header.

Binaries are built on Ubuntu 22.04 and need **glibc ≥ 2.35**. X11, Wayland, xkbcommon, Vulkan
and EGL are loaded at runtime, so the packages declare them as dependencies or recommendations.

Locally (on Linux): install nfpm, then `packaging/linux/package.sh` (or `--formats "deb tar"`).

### FreeBSD

GitHub has no FreeBSD runners, so the `freebsd` job builds inside a FreeBSD 14.3 VM
(`vmactions/freebsd-vm`, pinned by commit) and runs `packaging/freebsd/package.sh`, which stages
a `/usr/local`-style tree (binaries, desktop entry, MIME type, metainfo, icons, docs) as a
`.tar.gz`. Install with `tar -xzf djforge-*-freebsd-x86_64.tar.gz --strip-components 1 -C /usr/local`.
`.github/workflows/freebsd.yml` builds and tests the workspace there on every push to `main`.

### Web

`packaging/web/package.sh` runs `trunk build --release` in `apps/soundcraft-web` (output in
`dist/web`, `public_url = "./"`) and zips the site with sample `_headers` and `.htaccess` files and
the hosting guide. `apps/soundcraft-web` runs the same `SoundApp` as the desktop app on the
synthesised demo session, rendering with wgpu (WebGPU, or WebGL2 with `?webgl`) and playing
through WebAudio.

## Secrets

All secrets live in the repository's **`release` environment** (*Settings → Environments →
release*), restricted to the `release` branch. Only the jobs that sign (macOS, Windows) and the
draft-release job declare `environment: release`; the others get no secrets. Each secret is optional: if one is missing, that platform's artifacts
are unsigned and the run shows a `::warning::`. Scripts never echo secret values; certificates
are written with `umask 077` and deleted in `always()`/`finally` steps.

| Secret | Used for |
|---|---|
| `APPLE_CERTIFICATE` | base64 `.p12` with "Developer ID Application: <your name> (<team id>)" (djforge has no signing identity yet) |
| `APPLE_CERTIFICATE_PASSWORD` | password for that `.p12` |
| `KEYCHAIN_PASSWORD` | password for the temporary CI keychain (random if unset) |
| `APPLE_ID` | Apple ID used by `notarytool` |
| `APPLE_PASSWORD` | app-specific password for that Apple ID |
| `APPLE_TEAM_ID` | `DJ6XS33FX8` |
| `WINDOWS_CERTIFICATE` / `WINDOWS_CERTIFICATE_PASSWORD` | base64 `.pfx` (unused: Windows uses Azure) |
| `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET` | service principal for Azure Trusted Signing |
| `AZURE_SIGNING_ENDPOINT`, `AZURE_SIGNING_ACCOUNT`, `AZURE_CERT_PROFILE` | Trusted Signing endpoint, account and profile |

`GITHUB_TOKEN` creates the release. Only the final job gets `contents: write`. Provisioning the
environment, ruleset and secrets for a new repo: craftrules `release/signing-setup.md`.

## Icons

`assets/app-icon/icon.svg` is the canonical icon: djforge's four yellow (`#f8d200`) waveform bars on
a dark (`#111317`) rounded square; see `assets/app-icon/README.md`. `tools/icon/make_icon.py` (also
run by `packaging/icons.sh`) draws it and writes the SVG, the 1024 px PNG, the macOS 512 px window
icon, the `.icns`, the `.ico` and the hicolor PNGs. It needs Python with Pillow only. The outputs
are committed, so packaging never needs it. `apps/soundcraft/src/main.rs` sets the runtime
window/Dock/taskbar icon from them.

## Checks

`.github/workflows/packaging-lint.yml` runs in seconds on any change to `packaging/`, the
workflows or the icons: actionlint, shellcheck, a PowerShell parse, xmllint (WiX, plists,
AppStream, MIME), `desktop-file-validate`, `appstreamcli validate`, and a check that the two
Flatpak manifests agree on runtime, finish-args and plugin extensions.
