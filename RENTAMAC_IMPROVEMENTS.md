# RentaMac RustDesk — Improvements & Build Guide

A fork of [RustDesk](https://github.com/rustdesk/rustdesk) maintained for a Mac-rental
service. The goal is a self-hosted, low-latency remote-desktop experience with Apple
Silicon Mac hosts. Licensed **AGPL-3.0** (same as upstream) — this public repository is
also the source offer for the modified binaries end users connect to.

> This file is public. **Do not add hosts, IPs, passwords, tunnels, firewall config, or
> any other operational secrets here.** Keep it to code, rationale, and build steps.

## Branches

| Branch | Contents |
|---|---|
| `rentamac-host-fps-patch` | Host fps-ceiling fix only |
| `rentamac-jitter-buffer` | Experimental client-side jitter buffer (a.k.a. "pillar 2") |
| `rentamac-direct-udp` / `rentamac-direct-udp-148` | KCP-over-UDP for Direct IP + transport indicator (master- / 1.4.8-based) |
| **`rentamac-improvements`** | **Integration branch — merges all of the above. Build this.** |

All feature branches are based on the upstream **`1.4.8`** tag so the pinned CI toolchain
below applies cleanly.

## The improvements

### 1. Host fps-ceiling fix
Stock RustDesk's QoS controller caps a Mac *host* around ~20 fps during motion regardless
of client settings. Files: `src/server/video_qos.rs` (raise the fps tiers to 60, start
`INIT_FPS` at 60 instead of ramping from 15) and `libs/scrap/src/common/vpxcodec.rs`
(VP9 `VP8E_SET_CPUUSED` 7 → 8 for more software-VP9 throughput). Keeps a floor for genuinely
bad networks. Measured ~20 → ~42 fps at 1080p on an M-series host.

### 2. Client-side jitter buffer (experimental)
Smooths frame delivery on jittery links. Experimental; kept behind the integration branch
for A/B testing.

### 3. KCP-over-UDP transport for Direct IP connections
Stock RustDesk only uses **TCP** for direct `IP:port` connections; KCP (reliable-UDP, which
avoids TCP head-of-line blocking on lossy links) is wired only into the ID/rendezvous
hole-punch flow. This adds a direct-IP UDP path on **both** ends:

- **Host** — `src/rendezvous_mediator.rs`: `direct_server_udp()` binds UDP on the same port
  as the TCP direct server (`direct-access-port`, default 21118). A directly reachable host
  needs no hole punching, so the peer's KCP SYN arrives via `recv_from`; the socket is then
  connected to that peer and handed to `KcpStream::accept()`; the session goes to
  `create_tcp_connection(..., secure=false, ...)` — identical to the TCP direct server.
  Serves one session at a time (single-tenant host); concurrent peers fall back to the
  parallel TCP direct server.
- **Client** — `src/client.rs`: the `is_ip_str(peer)` branch tries `connect_direct_udp()`
  first, and falls back to the existing TCP path on any failure. Same return-tuple shape as
  TCP-direct, so the post-connect handshake is unchanged.

Gated on the `enable-udp-punch` option; **both ends must run the patched build and have
`enable-udp-punch = 'Y'`**. Either side unpatched → clean TCP fallback.

### 4. Transport indicator icon
`flutter/lib/common.dart`: `buildTransportIndicator(streamType, iconSize)` — a lightning-bolt
in the session tab (top-left), **green** for UDP/KCP, **red** for TCP, with an explanatory
tooltip. Wired into `remote_tab_page.dart` and `view_camera_tab_page.dart`; reads
`ConnectionTypeState.stream_type` (already plumbed from the Rust `set_connection_type`).
Tooltip strings fall back to English if a locale lacks them.

## Pinned toolchain (from upstream 1.4.8 CI)

- Rust **1.81.0**
- Flutter **3.22.3** (stable)
- `flutter_rust_bridge_codegen` **1.80.1** (`--features uuid`), `cargo-expand` 1.0.95
- vcpkg pinned at commit `120deac3062162151622ca4860575a33844ba10b`
- LLVM/Clang **18** (for bindgen's libclang)
- NASM **2.16.03** (from nasm.us — **not** brew nasm 3.x, which `aom` rejects)
- Flutter 3.22.3 needs `extended_text` pinned to `13.0.0` in `flutter/pubspec.yaml`

## Build — macOS (Apple Silicon)

Requires **full Xcode** (not just Command Line Tools): the `screencapturekit` feature pulls
`cidre`, whose build script runs `xcodebuild`, and `flutter build macos` uses Xcode too. If
the active developer dir points at CLT, point the build at Xcode **without** changing the
global default:

```sh
export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
```

Deps + build:

```sh
brew install llvm@18 pkg-config cmake ninja cocoapods
# NASM 2.16.03 into a dir on PATH ahead of brew's nasm (see below)
export LIBCLANG_PATH="$(brew --prefix llvm@18)/lib"
export VCPKG_ROOT="$PWD/vcpkg"   # cloned + bootstrapped at the pinned commit

git clone --branch rentamac-improvements <this-repo> rustdesk && cd rustdesk
git submodule update --init --recursive --depth 1
(cd flutter && sed -i '' 's/extended_text: 14.0.0/extended_text: 13.0.0/' pubspec.yaml && flutter pub get)
flutter_rust_bridge_codegen --rust-input ./src/flutter_ffi.rs \
  --dart-output ./flutter/lib/generated_bridge.dart \
  --c-output ./flutter/macos/Runner/bridge_generated.h
"$VCPKG_ROOT/vcpkg" install --x-install-root="$VCPKG_ROOT/installed"

# full app (dylib + Flutter UI incl. the icon):
cargo build --locked --features flutter,hwcodec,screencapturekit --release --lib
(cd flutter && flutter build macos --release)
# -> flutter/build/macos/Build/Products/Release/RustDesk.app
```

NASM pin:

```sh
curl -sL -o nasm.zip https://www.nasm.us/pub/nasm/releasebuilds/2.16.03/macosx/nasm-2.16.03-macosx.zip
unzip -oq nasm.zip && cp nasm-2.16.03/nasm <dir-on-PATH>/nasm
```

### Host-only shortcut (no Xcode available)
The host needs only the Rust core (the icon is client-side). Drop `screencapturekit` to avoid
the `cidre`/Xcode requirement (loses system-audio-over-ScreenCaptureKit only), build just the
dylib, and swap it into an existing app:

```sh
cargo build --features flutter,hwcodec --release --lib
# -> target/release/liblibrustdesk.dylib  (swap into RustDesk.app/Contents/Frameworks/)
```

After swapping a dylib you must ad-hoc re-sign the app; the re-sign **resets TCC**, so
Screen Recording / Accessibility / Input Monitoring must be re-granted on the host. Rust
builds are not byte-reproducible, so a hash mismatch vs a committed artifact does **not**
imply "stock" — check the code signature (`adhoc` = a patched deploy) instead.

## Build — Windows (client)

Toolchain: **Visual Studio 2022 Build Tools** (C++ workload + Windows SDK), Rust (MSVC
target), LLVM (libclang), CMake, vcpkg. There is **no cross-compile** — a Windows client can
only be built on Windows (`flutter build windows` needs Visual Studio; macOS cannot produce it).

```powershell
$env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin'
$env:VCPKG_ROOT = '<path>\vcpkg'                 # cloned + bootstrapped at pinned commit
$env:VCPKG_DEFAULT_TRIPLET = 'x64-windows-static'

git clone --branch rentamac-improvements <this-repo> rustdesk; cd rustdesk
git submodule update --init --recursive --depth 1
(cd flutter; flutter pub get)                    # after pinning extended_text to 13.0.0
& "$env:VCPKG_ROOT\vcpkg.exe" install --triplet x64-windows-static

# REQUIRED: generate the (gitignored) Rust bridge before building
cargo install cargo-expand --version 1.0.95 --locked
cargo install flutter_rust_bridge_codegen --version 1.80.1 --features uuid --locked
flutter_rust_bridge_codegen --rust-input ./src/flutter_ffi.rs `
  --dart-output ./flutter/lib/generated_bridge.dart `
  --c-output ./flutter/windows/runner/bridge_generated.h

# decode-only client (no hwcodec -> no ffmpeg SDK needed; software VP9):
cargo build --locked --features flutter --lib --release      # -> target\release\librustdesk.dll
(cd flutter; flutter build windows --release)
# -> flutter\build\windows\x64\runner\Release\rustdesk.exe
```

Windows does not use `screencapturekit`. `build.py --flutter --hwcodec` automates the same
sequence if you prefer.

**hwcodec needs an ffmpeg SDK on Windows.** The `hwcodec` feature compiles C++ that includes
`libavcodec/avcodec.h` etc.; without a prebuilt ffmpeg (RustDesk's CI downloads one and points
the `hwcodec` build script at it), `cargo build --features …,hwcodec` fails with
`fatal error C1083: Cannot open include file: 'libavcodec/avcodec.h'`. For a client that only
needs to **decode**, drop the feature — `cargo build --locked --features flutter --lib --release`
— and RustDesk uses software VP9/VP8/AV1 (the fps patch targets VP9 anyway). Add hwcodec back
only when you wire up the ffmpeg SDK for hardware decode.

## Testing the UDP path

1. Both host and client run a `rentamac-improvements` build.
2. Set `enable-udp-punch = 'Y'` in `RustDesk2.toml` on **both** (client also has the
   "Enable UDP hole punching" toggle in Network settings).
3. Connect by direct **`IP:port`** (host needs `direct-server = 'Y'`; default port 21118, TCP+UDP
   reachable).
4. The session tab's bolt icon should be **green**; confirm KCP is really in use on the host
   with `lsof -nP -iUDP -a -c RustDesk` showing an established peer pair. Red bolt = it fell
   back to TCP (one side unpatched, UDP blocked, or option off).

## Relevant config options (`RustDesk2.toml`)

- `direct-server` — enable Direct IP Access (TCP + the new UDP listener)
- `direct-access-port` — direct server port (default 21118)
- `enable-udp-punch` — gates the KCP/UDP paths (rendezvous punch **and** the new direct-IP UDP)

## Notes for future agents

- Keep feature branches on the `1.4.8` base so the pinned toolchain applies; rebase onto a
  newer tag deliberately and update the pins together.
- The direct-IP UDP server is single-session by design (single-tenant hosts). Multi-session
  would need packet demux on one socket (a `send_to`-based server mode in `kcp_stream.rs`).
- **Always run `flutter_rust_bridge_codegen` before building** (both platforms). The Rust side
  it emits — `src/bridge_generated.rs` / `bridge_generated.io.rs` — is **gitignored**, so
  `mod bridge_generated;` in `lib.rs` fails to compile without it (E0583, then cascading
  `EventToUI: IntoIntoDart` E0277s). Needs `cargo-expand` 1.0.95 + `flutter_rust_bridge_codegen`
  1.80.1 installed. Our changes don't alter `src/flutter_ffi.rs`, so the generated content is
  the stock 1.4.8 bridge — but you still must generate it locally each fresh checkout.
