# cdj3k-emu

A macOS desktop app that boots Pioneer DJ's **CDJ-3000** firmware inside QEMU on
Apple Silicon, surfacing the device's main LCD, jog LCD, jog wheel, faders,
buttons, and Pro DJ Link network in a native window.

<div style="display: flex; flex-wrap: wrap; align-items: center; gap: 10px;">
  <img src="docs/preview.png" alt="cdj3k-emu screenshot" width="500"/>
  <img src="docs/preview.gif" alt="Demo animation" style="display: block;"/>
</div>


## Why this exists

Built primarily for the technical challenge: figuring out what it takes to put
a piece of proprietary, tightly-coupled DJ hardware on a host hypervisor and
keep its real firmware happy. The interesting parts are the small surfaces
between a stock Linux kernel and the Pioneer userspace — DRM/KMS for the jog
LCD, the SPI sub-CPU protocol for the controls, the virtio-snd cadence shaping
that EP122's audio callback expects.

A secondary goal is having a reproducible bench for studying the **Pro DJ Link
(DJPL)** network protocol — beat broadcasts, master/slave hand-off, abs-pos
sync, the relationship between audible playback and what other players see on
the wire. Multiple emulator slots on the same host let you observe both ends of
a sync transaction with a packet capture between them, which is much harder to
do with real CDJs on a physical LAN when you're on vacation.

> [!TIP]
>
> ## What it is
>
> - A user-space wrapper around QEMU `aarch64-virt` running a vanilla Linux 6.6
>   kernel and a small set of out-of-tree modules + LD_PRELOAD shim that emulate
>   the Pioneer-specific hardware:
> - **virtio_snd** for audio (custom kernel module sized for the EP122 audio
>   callback cadence).
> - **subucom_virt** for the SPI sub-CPU protocol (buttons, jog encoder,
>   rotary, slider, capacitive sensors).
> - **ep122_shim** (LD_PRELOAD) that emulates the Rockchip-specific DRM/KMS
>   ABI EP122 expects (jog LCD as DSI-2, vsync_time, etc.) and routes jog-LCD
>   pixels through ivshmem so the host renders them as an egui texture.

> [!CAUTION]
>
> ## What it is **not**
>
> - **It does not ship Pioneer firmware.** You need to supply your own copy
>   of a CDJ-3000 update file (`.UPD`) and decryption key at first launch.
>   The in-app **Install Firmware** wizard decrypts it locally and provisions
>   a per-instance eMMC qcow2 disk image under `~/Library/Application Support/com.cdj3k.emu/`.
>   Nothing Pioneer-owned is in this repository or the distributed bundle.
> - Not endorsed by, affiliated with, or sponsored by AlphaTheta Corporation /
>   Pioneer DJ. CDJ, rekordbox, and Pro DJ Link are trademarks of their
>   respective owners.
> - Not a frame-accurate hardware simulator. Pixel output and SPI timings are
>   faithful enough to drive EP122 (the stock firmware), but the goal is a
>   daily-driver emulator, not a forensic recreation.
> - Not a turntable replacement. Jog wheel and rotary feel are reproduced via
>   pointer drag + scroll wheel or MIDI jog input. MIDI input covers selected
>   performance controls rather than the full control surface.
> - Not currently portable. Apple Silicon macOS only (HVF, vmnet.framework,
>   AppKit window/menu, CoreAudio). Linux/Windows are out of scope for v0.1.
> - **Not compatible with pre-3.00 firmware.** See _Firmware compatibility_
>   below — only CDJ-3000 firmware 3.00 and newer are accepted.

## Firmware compatibility

> [!IMPORTANT]  
> cdj3k-emu accepts **CDJ-3000 firmware version 3.00 or newer** only.

Pioneer shipped two different system-on-chip families across the CDJ-3000's
lifetime:

- **Renesas G2M** — used in firmware **1.x and 2.x**. Different kernel, very
  different driver stack, different DRM/KMS path for the jog LCD. cdj3k-emu
  does **not** support this hardware target.
- **Rockchip RK3399** — used in firmware **3.00 and newer**. This is the
  target cdj3k-emu emulates: vanilla Linux 6.6 aarch64 kernel, the
  `ep122_shim` LD_PRELOAD, the `subucom_virt` SPI emulation, the
  `virtio_snd` audio pipeline are all written against the RK3399 userspace
  that the 3.00+ firmware ships.

If your `.UPD` decrypts to a Renesas-G2M kernel + rootfs the firmware wizard
will reject it before provisioning the eMMC image. Use a 3.00+ update file.

## About the decryption key

Pioneer ships CDJ-3000 firmware updates as `.UPD` files whose payload is
LUKS-encrypted. The "decryption key" the firmware wizard asks for is the LUKS
keyfile that unwraps that payload so we can mount it and extract the kernel +
rootfs.

This repository does **not** ship that key, for the same reason it doesn't
ship the `.UPD` itself: it is Pioneer-controlled material and we have no right
to redistribute it. cdj3k-emu's posture is strict — nothing Pioneer-owned in
the repo, nothing Pioneer-owned in the bundle. Acquiring the keyfile is the
user's responsibility, by whatever means they are themselves entitled to.

## System requirements

- **macOS 13 (Ventura) or newer**, Apple Silicon (M1/M2/M3/M4).
  HVF needs the `com.apple.security.hypervisor` entitlement; `bundle.sh`
  signs the binary with it.
- **macOS 15 (Sequoia) is strongly recommended.** cdj3k-emu auto-detects
  the host version at every spawn:
  - 15+ → QEMU uses Apple's **in-kernel ARM vGIC** (`hv_gic_create`).
    Drops the per-IRQ vCPU-exit cost dramatically. EP122's chatty IRQ
    pattern is the dominant cost driver for HVF guests, so this is a
    big perf win on 15+.
  - 13 / 14 → QEMU falls back to **userspace GIC emulation**
    (`kernel-irqchip=off`). Functional but noticeably more CPU per
    instance; you'll feel it most when running multiple slots at once.
- Roughly 8 GB free RAM if you plan to run two instances at once
  (1.5 GB guest each + host overhead).
- A copy of a Pioneer CDJ-3000 firmware update file and its decryption key.

## Known limitations

- Audio is not a bit-perfect reproduction of the real hardware. The CDJ-3000
  ships dedicated audio silicon with its own clock domain and DSP path; we
  re-route the firmware's PCM through `virtio_snd` -> a QEMU bypass ring ->
  CoreAudio, which is a fundamentally different pipeline. Occasional pops
  and transient artifacts can occur, especially under host CPU pressure or
  when the macOS HAL device clock drifts against the guest. A large amount
  of work has gone into mitigations (lock-free SPSC ring, dedicated RT
  writer thread, soft-PLL clock-skew correction, pipeline-depth watchdog,
  RELEASE-flush on recovery) - see `docs/audio-stack.md` for the full
  pipeline. Steady-state output on a matched-rate device is clean, but the
  result is not forensically identical to a physical CDJ-3000.
- Audio latency depends on macOS CoreAudio queue depth - typically 30-80 ms
  end-to-end with the bundled bypass-ring patches. ALC ("Enable ALC") shifts
  the audible / Pro DJ Link broadcast alignment to compensate but adds a
  bit of jitter; default-on.
- If an output device stalls or is reconnected, use **Audio → Reset Audio**
  in that player's window. This closes and reopens its CoreAudio path without
  restarting the emulator or guest firmware.
- Use **Audio → Latency Profile…** to select a per-player Safe, Balanced, Low
  Latency, or Custom buffer profile. Balanced retains the proven 2,048-frame
  startup prefill and 1,024-frame guest transfer size. Profile changes restart
  only that player because the guest transfer size is negotiated when ALSA
  opens the stream.
- Jog wheel feel is "good enough for cueing" — no torque feedback.
  Brake stop-time map matches the device's `jog_adjust` rotary, so
  the _cadence_ of stops is faithful even if the touch isn't.
- Service mode (EP122TestMode) is reachable via the Emulation menu but some
  test routines that touch hardware-only registers (e.g. fan-RPM read) return
  fixed values.
- LINK MODE (rekordbox <-> Emulation) is half-working.
- PC Link and Audio HID functionality are not available.
  This requires a guest USB gadget presenting HID and MIDI, connected to a CoreMIDI endpoint.
  On macOS 15 and later, all virtual HID implementations (such as IOHIDUserDevice via a DriverKit HID extension) are restricted by Apple Entitlements.

## Controls

### Jog wheel

You can control the jog wheel a few different ways:

- **Click and drag the center** — for scratching effects.
- **Click and drag the outside ring** — for a subtle pitch bend.
- **Scroll the mouse wheel** anywhere on the jog — also bends pitch.
- **Hold Ctrl, click-and-drag, then release ("slingshot")** — gives the wheel a flick, which is shown as a line. The faster you pull, the more spin you get.

### Using multiple controls at once

On a real CDJ, you can hold a button or touch the screen while doing something else, like turning the jog wheel. With a mouse, there's only one pointer, so you can hold **Ctrl** to "latch" a button or touchscreen press:

- **Ctrl + click a button** — the button stays pressed until you let go of Ctrl.
- **Ctrl + click the LCD touchscreen** — touch stays down until you release Ctrl.

This lets you, for example, keep `Search Forward` held down while moving the jog wheel to search faster, just like on real hardware.

### Firmware keyboard input

When the firmware shows its on-screen keyboard, click the player window and
type on the Mac keyboard. Printable US-ASCII characters, Backspace, Delete,
Enter, Escape, Tab, the arrow keys, Home, and End are forwarded to the
firmware's focused text field. Clipboard paste is supported for US-ASCII text.
The guest sees a standard USB keyboard; the firmware's own keyboard focus
determines whether the input is accepted, so there is no screen-image matching
or firmware-version-specific coordinate map.

### Virtual mixer

Open **View → Virtual Mixer** from any player window for a shared four-channel,
DJM-A9-style mixer. Players 1-4 are routed to channel strips 1-4 respectively.
Each strip has trim, high/mid/low EQ, a low-pass/high-pass
filter, a channel fader, a pre-fader meter, and an A/THRU/B crossfader assignment.
THRU bypasses the crossfader. The master strip to the right of channel 4 has
stereo output meters, a global EQ/ISO curve switch, a master level knob (0 dB at
12 o'clock), and Echo, Reverb, Flanger, and Delay effects. CH SELECT targets
channels 1–4 or the master mix. FX FREQUENCY's LOW/MID/HI buttons select which
input bands feed the effect; LEVEL/DEPTH controls the wet amount. FX ON/OFF
applies the effect only while enabled; switching it off fades the wet signal
out briefly while the dry track continues. The A and B
headphone CUE buttons on each channel and Master toggle independent cue buses.
Choose **Audio → Cue A Output** and **Audio → Cue B Output** to route them to
host output devices; both are off until assigned. Channel cues tap post-EQ and
filter but pre-fader/crossfader, while Master cue taps the master mix before the
master level knob. QEMU publishes
each player's raw audio to a shared lock-free
ring. One singleton native mixer owns CoreAudio, applies all four channel
strips, and performs the final sum through the Master output, with separate
native streams for the enabled cue buses. Mixer
positions are persisted independently for each player.

Drag a knob vertically; double-click it to return to the center/default value.
All four channels' trim, EQ, filter, fader, and crossfader assignment controls,
plus the global EQ/ISO curve, appear on the Mixer tab
of the shared MIDI Learn window. Continuous controls learn CC messages;
switches learn Notes or CCs. They intentionally have
no factory MIDI CC numbers, since mixer controller layouts vary.
The master level and FX controls can also be learned there.

### MIDI controllers

The app automatically connects to available CoreMIDI inputs and checks for
hot-plugged controllers twice per second. Endpoints are tracked by CoreMIDI's
stable ID, so adding or removing one controller does not interrupt the others.
MIDI Learn is one shared window with **Slot 1–4** and **Mixer** tabs. Open
**Emulation → MIDI Learn…** from any player, select a tab, click **Learn** beside
a control, then operate the desired controller button, jog, or fader. Deck
assignments are stored separately for each slot; mixer assignments are shared.
A learned binding is matched by CoreMIDI port, MIDI channel, and Note or CC
number, so multiple player windows do not cross-trigger.

If a controller was reconnected under the same endpoint ID but its driver left
a stale connection, click **Rescan MIDI Devices**. Replacements are opened
before healthy old handles are dropped whenever the driver permits it, avoiding
an input blackout. The profile bar can save, load, overwrite,
and delete named assignment sets. Each slot and the mixer have separate
profiles. Rescan and Reset apply to MIDI inputs in all running player instances.

Fresh slots use the official CDJ-3000 messages below. Player 1 defaults to MIDI
channel 1, Player 2 to channel 2, and so on:

| Control | MIDI message |
| --- | --- |
| Play/Pause | Note 0 (`0x00`) |
| Cue | Note 1 (`0x01`) |
| Hot Cues A-H | Notes 27-34 (`0x1B`-`0x22`) |
| Pitch/tempo fader | CC 29 (`0x1D`), values 0-127 |
| Hot Cue Call/Delete | Note 26 (`0x1A`) |
| Loop In / Out | Notes 35 / 36 (`0x23` / `0x24`) |
| Reloop/Exit | Note 37 (`0x25`) |
| Beat Sync | Note 22 (`0x16`) |
| Master Tempo | Note 11 (`0x0B`) |
| Tempo Range / Reset | Notes 10 / 9 (`0x0A` / `0x09`) |
| Jog platter / outer ring | CC 16 / 48 (`0x10` / `0x30`), 64 = stop |
| Jog touch | Note 23 (`0x17`) |
| Mixer trim / high / mid / low / filter / fader | Unassigned; learn any CC |

The Slot tabs also expose the remaining physical switch-frame buttons,
including search and track navigation, beat jump, loop, memory/call/delete,
source and browse controls, quantize, slip, reverse, and utility buttons.
Where the [CDJ-3000 MIDI message list](https://downloads.support.alphatheta.com/software_info/dj-players/CDJ-3000/CDJ3000_MIDI_Message_List_E108.pdf)
publishes a Note number, it is the default; other controls start unassigned.

Button velocity zero and Note Off both release a button. Tempo value 64 maps
to the center detent. Button assignments can learn either Notes or CCs; the
pitch/tempo, jog-motion, and mixer assignments learn CCs. Jog platter and outer
ring use the CDJ-3000's speed-centred CC format by default. For controllers
that send relative jog increments (1 forward, 127 backward), select
**Relative (1 / 127)** beside the jog assignment. The adjacent **Steps/rev**
field sets how many relative encoder counts equal one full turn, separately
for the platter and outer ring (default 128); it is saved with each slot's
MIDI profile. The separate Jog Touch assignment
enables vinyl/scratch touch semantics while held; outer-ring motion provides
pitch bending without touch. Reloop and Exit share the same physical control,
so there is one MIDI assignment for both. The MIDI Learn window can also clear
an individual binding or restore that player's defaults. Devices that emit the
[CDJ-3000 MIDI assignments](https://www.pioneerdj.com/-/media/pioneerdj/software-info/player/cdj-3000/cdj3000_midi_message_list_e108.pdf)
work without remapping when their MIDI channel matches the player slot.

## EP122 mods

The LD_PRELOAD shim also carries the [cdj3k-mods](https://github.com/nsaintot/cdj3k-mods)
feature set (Gate Cue, MOD SETTINGS, Themes, STEMS, X-PAD), pulled in as the
git submodule at `guest/ep122_shim/cdj3k-mods` and linked into `ep122_shim.so`
by `guest/Makefile`. It's gated by **Emulation → EP122 Mods**, off by default.
Until it is checked the instance boots with `ep122_no_mods` on the kernel
cmdline; the guest turns that into `EP122_NO_MODS=1` in EP122's environment and
the mods stay out of the process entirely. Checking it restarts the instance
with the mods installed. The setting is persisted per instance. `cdj3k-emu
--no-mods` forces the gate off for one launch without touching the saved value.

STEMS needs a [stemd](https://github.com/nsaintot/stemd) server on the LAN; the
guest-side `stemd_client` sidecar (`stemd-client.service`) discovers it over
mDNS or takes an address from MOD SETTINGS.

## Building

```bash
# 0. The mods are a submodule.
git clone --recurse-submodules https://github.com/nsaintot/cdj3k-emu
#    (or, in an existing checkout:)  git submodule update --init

# 1. Build QEMU (clones upstream, applies our shm-display patch, ~10 min).
./qemu/build.sh

# 2. Build the aarch64 kernel, out-of-tree modules, shim + mods, and guest
#    tools (Docker). No Pioneer firmware is needed at compile time; the app's
#    first-run wizard extracts and patches the user-supplied firmware.
#    `make abi-check` gates the shim against the deck's glibc.
./build.sh

# 3. Assemble the .app bundle. The Homebrew dylibs QEMU links against are
#    copied into the bundle and rewritten to @loader_path, so the .app runs
#    on a Mac without Homebrew.
./bundle.sh                # ad-hoc signed (HVF works, FDA does not)
./bundle.sh --sign "Apple Development"   # real cert (enables Full Disk Access)
./bundle.sh --sign "Developer ID Application" --dmg   # distributable .dmg

# or, optionally, a Release build: Developer ID + notarized + stapled .app and .dmg.
#    One-time: store notary credentials (app-specific password from appleid.apple.com) under a keychain profile:

xcrun notarytool store-credentials cdj3k-emu-notarization --apple-id <APPLE_ID> --team-id <TEAM_ID>
./bundle.sh --sign "Developer ID Application" --dmg --notarize
```

`scripts/sync-mods.sh [branch]` moves the submodule to the tip of a cdj3k-mods
branch; commit the new pin afterwards.

## First-run privilege prompts

cdj3k-emu asks for the macOS admin password in three specific situations,
**only when you ask it to**:

| Action                                                          | Why it elevates                                                                                                                                                                                                        |
| --------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Selecting a physical Wi-Fi / Ethernet interface for Pro DJ Link | Starts the bundled `socket_vmnet` daemon as root — the only way to bridge an L2 network on macOS without writing a kernel extension. Re-used across instances; one prompt per host interface.                          |
| Selecting a TAP interface (OpenVPN etc.)                        | Creates a macOS `bridge` device + assigns a `tap` device to QEMU via Authorization Services. Torn down automatically when the app exits.                                                                               |
| Attaching a physical USB drive in pass-through mode             | `chmod 660` on `/dev/diskN` so QEMU can open it `O_RDWR`. The exact device path is validated against `/dev/disk[0-9]+(s[0-9]+)?` before elevation — see `crates/cdj3k-emu-runtime/src/usb.rs::is_valid_bsd_disk_path`. |

All three use the native macOS password dialog (TouchID / Apple Watch eligible)
via `AuthorizationServices`, not a CLI prompt. None of them grant ongoing
privileges — every elevation is scoped to one command.


## Documentation and reference

- [Audio stack](docs/audio-stack.md)
- [ALC](docs/alc.md)
- [Network stack](docs/network.md)
- [Storage](docs/storage.md)
- [Host/guest stream transports](docs/stream-transports.md)
- [subucom SPI protocol](docs/subucom.md)
- [G2M (Renesas) — unsupported target](docs/g2m-renesas.md)


## Repository layout

```
app/cdj3k-emu/       binary (eframe egui app + runtime worker)
crates/cdj3k-emu-*   Rust workspace: subucom, streams, platform, ui,
                     runtime, storage, firmware
guest/               C sources built for the guest:
  cfgd/                cdj3k-cfgd  - virtio-serial config daemon
  ep122_shim/          ep122_shim.so - LD_PRELOAD shim (emulation half)
    cdj3k-mods/        submodule: the EP122 mods + stemd_client sidecar
  subucom/             subucom_forwarder, subucom_live
  modules/             out-of-tree kernel modules (subucom_virt, virtio_snd, udev_usb1)
  kernel-patches/      vanilla 6.6 patches + the guest kernel .config
qemu/                upstream QEMU source + our overlay patches
docker/              Alpine + Ubuntu build pipeline for guest artefacts
initramfs-patch/     numbered rootfs patch scripts (concatenated by bundle.sh)
scripts/             bundle-dylibs.sh (self-contained .app), sync-mods.sh
```

## License

cdj3k-emu's original sources are dual-licensed at your option under either of:

- **Apache License, Version 2.0** ([`LICENSE-APACHE`](LICENSE-APACHE))
- **MIT License** ([`LICENSE-MIT`](LICENSE-MIT))

Linux kernel modules in `guest/modules/`, kernel patches in
`guest/kernel-patches/`, the QEMU embedding shim in `qemu/shim/`, and QEMU
patches in `qemu/patches/` are derivatives of GPL-2.0 upstreams and retain
**GPL-2.0-or-later**. See [`NOTICE`](NOTICE) for a full inventory of bundled
third-party components and the firmware-acquisition expectation.

## Contributing

Issues and pull requests welcome. By submitting a contribution you agree it
is dual-licensed Apache-2.0 / MIT per [`LICENSE-APACHE`](LICENSE-APACHE) and
[`LICENSE-MIT`](LICENSE-MIT), except for contributions inside the GPL-2.0
carve-outs listed in [`NOTICE`](NOTICE), which remain GPL-2.0-or-later.
