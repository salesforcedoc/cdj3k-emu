# Audio architecture

The emulator uses one native mixer for all running players. QEMU does not
open an audio device, select an output, perform mixer DSP, calculate meters,
resample to the hardware rate, or reset CoreAudio.

```text
CDJ firmware / guest ALSA
          |
          v
QEMU virtio-sound (guest-facing device)
          |
          | S32/F32 stereo conversion and transport at guest rate
          v
per-player shared-memory PCM ring
          |
          v
singleton native mixer
  - per-channel anti-aliased cubic resampling
  - trim and three-band EQ
  - pre-fader level meter
  - filter, channel fader and crossfader
          |
          v
one AUHAL/CoreAudio output device
```

## QEMU boundary

QEMU retains only work that belongs to emulation or guest transport:

- virtio-sound control and PCM queues;
- guest PCM format conversion to interleaved F32 stereo;
- the dedicated ring-writer thread, backpressure, deferred virtqueue
  completion, and startup prefill;
- publication of the negotiated source rate for its player ring.

The shared ring is mandatory. If it cannot be mapped, the QEMU audio voice
fails instead of opening a per-player CoreAudio stream. Only stereo F32 and
S32 guest output formats are accepted by this transport.

## Native mixer boundary

The native mixer is elected across player processes with an advisory lock.
The owner opens the application-wide selected CoreAudio output, while every
player maps the same mixer state and its own PCM ring.

Each channel has a streaming cubic-Hermite resampler. Downsampling applies a
second-order Butterworth low-pass before interpolation. A slow, bounded
ring-depth integrator compensates for long-term guest/device clock skew
without discontinuous frame drops. Resampling occurs before channel DSP, so
all filters and summing run at the actual output-device rate.

The level meter is post-trim/post-EQ and pre-filter/pre-fader, matching the
virtual DJM layout. Filter, channel fader, crossfader and any future master
gain do not affect the channel meter.

## Device changes and reset

Output-device selection and Reset Audio target only the native mixer. The
native AUHAL engine is stopped and reopened while QEMU and the guest keep
running. During the short reopen interval, producers remain attached to their
rings and apply normal backpressure; there is no QMP audio-reset command and
no QEMU hardware-output fallback.

## Latency profiles

Profiles provide three independent values:

- guest transfer frames, fixed when ALSA negotiates the virtio-sound stream;
- startup prefill frames in each source-rate ring;
- requested native CoreAudio buffer duration.

Changing the guest transfer size still requires restarting that player.
Changing or resetting the native output does not restart emulation.

## Debug telemetry

The debug panel reports:

- negotiated source rate and guest transfer/prefill sizes;
- source-rate ring depth for the current player;
- native output rate;
- CoreAudio buffer, safety-offset and device-latency frames read by the
  process that owns the device;
- whether the mandatory shared transport is active.

The old host-latency value derived inside QEMU is deliberately zeroed and is
not used by the UI. The displayed host and total values are reconstructed
from native HAL telemetry plus the guest-reported in-flight latency.

## Rebuilding QEMU changes

All QEMU changes are represented by the ordered files in `qemu/patches/`.
Patch `24-native-audio-boundary.patch` removes the old CoreAudio/DSP path and
establishes the transport-only boundary. Run `bash qemu/build.sh` before
bundling so the installed executable and embedded dylib include the patch.
