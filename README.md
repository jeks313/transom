# Transom

**Transom** is a seamless remote windowing system: individual
macOS app windows streamed to a Windows PC as independent, native windows you can
move, resize, snap, and fullscreen. Think RDS RemoteApp with a Mac host — which
does not otherwise exist.

> **Early access.** Transom is a usable host/client product on the intended
> Mac + Windows setup. The host half works end to end on the
> target Mac: it probes HEVC 4:4:4 hardware encode, tiles windows non-overlapping,
> captures + hardware-encodes the virtual display, and serves window rects (and
> optionally video) to a client over TCP. It has **no auth and no encryption**
> (see Security below), and none of this is verifiable in CI — only on the Mac
> Studio (I-7). The Windows client now lives in this repo under
> [`client/`](client/) and is a working window manager against this host's wire
> protocol (its GPU/decode runtime needs a real Windows box; see
> [`client/README.md`](client/README.md)).

## This fork (jeks313/transom)

> [!WARNING]
> **Disclaimer.** Everything in this fork was written with Claude Code over a
> couple of days, strictly for personal use, to see what was possible. No code
> review has been done: no human has reviewed any of this code. Do not rely on
> it, and do not run it anywhere it matters.

A fork of [aydinmrnv/transom](https://github.com/aydinmrnv/transom) at v0.5.0,
tuned for low latency between an M5 Pro Mac (macOS 26.7) and a Windows 11 PC
over Tailscale. Everything below was tested through `transom-host serve` and the
Windows client; the Transom Host app was not re-tested. Where this section and
the rest of the README disagree, this section is current: the host makes its
own virtual displays (no BetterDisplay), encodes each window separately in HEVC
4:2:0 8-bit, and speaks protocol v5.

### Video pipeline and latency

| Change | Effect (measured) |
| --- | --- |
| **One encoder per window** (protocol v4). Each lane is still captured as one display, so menus and popovers stay in their window's picture, but only the windows ScreenCaptureKit reports dirty are cropped (a Metal blit) into encoders of their own size. | Safari window capture to present: 11 ms p50, 21 ms p95 (was 79 / 210 ms with a whole 5K lane) |
| **GPU decode on Windows.** Media Foundation decodes into the renderer's D3D11 device, and pictures are copied GPU to GPU into a real NV12 texture. | 5K decode 18.8 ms to 0.3 ms |
| **Frames are decoded as they arrive** on the video thread, and a decoded frame wakes the message loop to present it at once. | Queue 8 ms to 0, display 12 to 4 ms |
| **No periodic keyframes or endless idle refresh.** Keyframes are a 60 s backstop, the client asks for one when it needs it, and an idle lane is refreshed once. | Real frames no longer queue behind a forced 250 KB keyframe |
| **App Nap is kept off** while sharing. | Encode time had doubled (8 to 17 ms) a minute after launch |
| **At most two frames in flight** per window encoder; the newest waits and replaces older waiting ones. | A 7 MP window at 120 fps no longer falls 42 ms behind |
| **120 Hz virtual displays.** Displays refresh at max(60, `--fps`), and `--fps` defaults to 120. | A small window arrives at 107 fps instead of 57 |
| **Encoder kept through a resize** (protocol v5). A window that shrinks or grows within its encoder keeps it, drawn in the top left with the rest black, and each frame carries the window's size. A window that outgrows its encoder gets one 25% larger; after 1.5 s at one size an oversized encoder is replaced by an exact fit. | A drag costs a keyframe now and then instead of one per step |
| **The old picture stays up** while a restarted window stream waits for its first keyframe. | No placeholder hatch after a resize |
| **Mouse moves are throttled to 120 Hz** on the host. | A 1000 Hz mouse no longer stalls Chrome's event queue for seconds |
| **Red/blue swap fixed** in the NV12 to RGB shader. | Colours are right |

**Latency measurement.** Every frame carries capture, encode and send stamps
on the host clock (protocol v3), and ping/pong maps that clock onto the
client's. The client logs p50/p95 for each stage to
`%LOCALAPPDATA%\Transom\latency.log` and shows the total in the status line.
Keys and clicks are matched to the first frame of their window captured after
they could have reached the Mac, which gives an `input:` line with input to
capture and input to screen. Typing into a test window over Wi-Fi measured
**43 ms p50, 82 to 108 ms p95** from key to screen; the p95 is the Mac's Wi-Fi.

### Sharing several apps

- **The host makes its own virtual displays.** `serve <app>` without a display
  id starts `transom-display`, a child holding a 2x `CGVirtualDisplay` (Sill's
  `VirtualDisplay.swift`, vendored, Apache-2.0) to the right of the other
  displays, never main. It keeps the Mac out of display sleep and goes away when
  `serve` stops. `serve` warns when Stage Manager is on.
- **Lanes** (protocol v2). `serve --lane APP[,APP...][=WxH]` puts more apps on
  displays of their own, each with its own capture, in the same process and over
  the same two connections.
- **System prompts.** Touch ID, password, admin-approval (including BeyondTrust
  Privilege Management) and similar prompts appear on the Mac's own screen,
  where the client cannot reach them. `serve` moves them onto a prompt display
  and the client opens them at once (`--no-prompts` to skip).
- **Tiny helper windows are hidden.** Untitled windows under 100x60 pt that are
  not the app's own (Safari and Chrome keep 66x20 pt ones) are not shared.
- **ScreenCaptureKit start is bounded:** 5 s per attempt, up to 4 tries.

### Using the apps

- **Native menu bar.** The host reads each shared app's menu bar through
  Accessibility and the client shows it as a Win32 menu bar on that app's
  windows. A pick types the item's shortcut, or presses it through
  Accessibility when it has none. Alt and F10 do not open it, since Alt is
  Option on the Mac.
- **Clipboard.** Plain text syncs both ways over the control channel: the host
  checks the Mac pasteboard every 250 ms, and the client sends what is copied
  on Windows plus its clipboard on connect. Neither side sends back what it
  just wrote, text over 1 MiB is skipped, and line endings are converted.
  Everything copied on Windows reaches the Mac while connected.

### Network

- `serve --host` also accepts the CGNAT range `100.64.0.0/10`, so the host can
  listen on a Tailscale address and a client on the same tailnet connects
  directly. Tailscale encrypts the link; Transom itself still does not.

### Headless client

`transom-client connect <host> --video` prints per-stage latency. It also has:

| Option | Does |
| --- | --- |
| `--type-every <ms>` | types `--type` one key at a time and reports input to screen |
| `--window <id>` | picks the window `--type` types into |
| `--copy <text>` | puts text on the Mac's clipboard |

## Quick connect

Start sharing in Transom Host on the Mac, then open Transom on Windows and
select the Mac by name under **Macs**. No IP address is needed.
Select a preview card to open a Mac window on the PC. The Mac host supports
sharing several selected apps; local Windows title bars move and resize each view.
Successful connections are remembered, including custom ports; discovered Macs
are found again when their IP changes. On an existing host installation, enable
**Settings → Connection → Choose a LAN address automatically**. Both computers
must share a trusted local network. See [the Windows guide](client/README.md).

On Windows, **Ctrl+Alt+Shift+D** disconnects from the dashboard or a focused remote
window and leaves the Mac documents open. In Transom Host on macOS,
**Sharing → Stop Sharing** (**⌘⌥⇧D**) stops the host session; this command is also
available while the host's Settings window is focused.

## The problem

Parsec already streams a Mac desktop to Windows beautifully, but its windowed
mode scales the *entire desktop* into the window, so shrinking the window turns
text into an unreadable smear. That is a **resampling** problem, not a codec
problem. Transom fixes it by mirroring geometry — the client window resizes, the
Mac window is set to exactly that pixel size, the app relayouts natively, and we
blit 1:1.

## Architecture in three sentences

A large virtual display on the Mac (created externally with BetterDisplay) is
used as a compositing scratch space: every managed app window is tiled onto it
non-overlapping, so nothing is ever occluded, and one ScreenCaptureKit stream
with one hardware encoder captures the whole thing. The Windows client crops
per-window sub-rectangles out of that shared texture and draws each as its own
native window, while window rectangles travel on a side metadata channel. The
Windows client is the real window manager; the Mac host only draws.

**Read the canonical design doc: [`docs/architecture.md`](docs/architecture.md).**
It is the source of truth for both halves. The Windows client lives in this repo
under [`client/`](client/).

## Build

Requires macOS 14+ and a Swift 6 toolchain (Xcode 16 or a matching open-source
toolchain). The only dependency is
[swift-argument-parser](https://github.com/apple/swift-argument-parser).

```sh
swift build
swift run transom-host doctor
```

## `doctor`

`doctor` is the one real command. It checks Screen Recording and Accessibility
permissions, enumerates displays, and confirms ScreenCaptureKit can see them.

```sh
swift run transom-host doctor            # report and exit non-zero if not ready
swift run transom-host doctor --prompt   # also trigger the Accessibility prompt
```

**The permission gotcha it exists to explain:** macOS attributes privacy (TCC)
permissions to the app that *launched* the process, not to the `transom-host`
binary. Run from a terminal, the Screen Recording / Accessibility grants you need
are attributed to your **terminal app** (Terminal, iTerm2, Ghostty, VS Code…),
not to `transom-host`. `doctor` prints this in full — read it before you go
hunting for a "transom-host" checkbox that will never appear.

## ⚠️ Security: none

**There is no authentication and no encryption.** As of issue #7 the host also
**injects input**: anyone on the LAN who can reach the control port can move the
mouse and **type into your Mac** (`CGEventPost`), not just watch a live video feed
of it. The threat model is now "type into your machine," not merely "watch your
screen." That is acceptable for a **wired home LAN** and **completely unacceptable
anywhere else** — do not port-forward it, do not run it on a network you do not
trust, do not put it on the internet.

As a guardrail (not a security boundary), `serve` **refuses to bind to anything
but a private address** (`10/8`, `172.16/12`, `192.168/16`, `127/8`, `169.254/16`)
and defaults to `127.0.0.1`. Pass `--host <LAN-ip>` to expose it to a real client.
Auth and encryption are explicitly out of scope until designed properly.

> The product defaults are control `47100` and video `47101`. They avoid the
> macOS AirPlay Receiver collision on port `7000`. Custom ports are supported by
> passing the same `--control-port` / `--video-port` values to both sides.

## The apps

Two SwiftUI apps wrap the library in real bundles with **stable, distinct** bundle
ids, so each gets its own TCC (Screen Recording / Accessibility) identity — a CLI
run from a terminal has those grants attributed to the *terminal*, not the binary.

| App | Bundle id | What it is |
| --- | --- | --- |
| **Transom Host** | `one.nullstack.transom.host` | Control panel over `serve`: permissions, pick a display + apps, Start, and live status (client connected, fps/bitrate, encoder mode, tile layout with post-clamp deltas). Lives in the menu bar too: start/stop sharing there, and minimize or close the window into it. Settings cover launch at login, auto-start, theme, accent color, card size and the menu bar icon. |
| **Transom Probe** | `one.nullstack.transom.probe` | The M0 diagnostic probe (OQ-1/OQ-2/OQ-5). |

Build and package both signed bundles (hardened runtime), then run one:

```sh
scripts/make-app.sh            # builds build/Transom Host.app and build/Transom Probe.app
open "build/Transom Host.app"
```

`scripts/release.sh host` (or `probe`) cuts the matching GitHub prerelease. Neither
app is notarized.

## Use it today (trusted LAN)

1. Create the BetterDisplay virtual display and make it the Mac's main display.
2. Open the signed `Transom Host.app`, grant Screen Recording and Accessibility,
   select that display and the app to share, then set Settings → Connection →
   Address to the Mac's private LAN address. Press **Start**.
3. Download and run `TransomSetup-vX.Y.Z.exe` from the GitHub release. It
   installs the Windows client and creates a Start Menu shortcut. Open Transom,
   enter the Mac address, and press **Connect**. The default ports are `47100`
   (control) and `47101` (video); the host app and client already agree on them.
4. Move, resize, focus, type, scroll, and close the resulting native Windows
   proxy window. Stop the host when finished.

The transport is still unauthenticated and unencrypted. Keep it on a trusted
private network and do not port-forward it.

## Command surface

| Command | Status | Purpose |
| --- | --- | --- |
| `doctor` | **real** | permission / display / SCK health check |
| `displays` | **real** | machine-readable display list |
| `windows` | **real** | enumerate app windows + AX geometry |
| `place` | **real** | set one window's size/position via AX, report the readback delta |
| `tile` | **real** | pack windows non-overlapping (with gutters) on the virtual display |
| `capture` | **real** | run the shared ScreenCaptureKit stream, verify no scaling (I-1) |
| `encodeprobe` | **real** | probe HEVC 4:4:4 hardware encode (OQ-4) |
| `encode` | **real** | capture + HEVC 4:4:4 10-bit hardware encode; report fps/bitrate |
| `serve` | **real** | tile + watch an app (`--also <app>` adds more, as the host app does; hidden, minimized and full-screen windows are brought onto the display first unless `--no-restore-windows`), serve rects + video by default, drive the resize roundtrip (throttled AX writes, readback, I-4), and **inject client input** over TCP. Input reaches shared windows in the background by default (no raise, the Mac's cursor stays put); `--input-delivery foreground` restores raise-and-click |
| `mockresize` | **real** | mock client: drive a Live/End resize drag against `serve` and measure the ~10Hz throttle |
| `inject` | **real** | post a click/text/chord/scroll into a window locally; log the full coordinate chain. `--delivery background` (default) leaves the window where it is; `foreground` raises it |
| `mock-client` | **real** | stand-in client: connect to `serve` and post input events (issue #7 verification) |
| `probe` | **real** | architecture de-risking experiments |
| `menuwatch` | **real** | stream the focused app's windows/menus (answers OQ-1) |

## License

[AGPL-3.0](LICENSE).
