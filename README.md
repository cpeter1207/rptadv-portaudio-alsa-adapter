# rptadv-portaudio-alsa-adapter

`rptadv-portaudio-alsa-adapter` is the hardware-audio boundary for the
standalone `rpt_advanced` architecture. It provides a small versioned
shared object that owns PortAudio stream lifetime, ALSA mixer control, and raw
hardware-audio measurements for one radio port.

It does not contain radio DSP, signaling, GPIO, EEPROM, Hamlib control,
Asterisk integration, or audio processing.  Those responsibilities remain in
their respective adapters and in `librptadvradio`.

## PCM contract

All internal audio interfaces use interleaved normalized `f32` PCM with a
full-scale reference from `-1.0` through `+1.0`.  The adapter requests
`paFloat32` from PortAudio, so a PortAudio callback has the same representation
as the internal worker boundary. PortAudio or its host API may still
perform device-format conversion when the hardware itself does not natively
accept 32-bit float; no application-side PCM conversion is needed at that
boundary. Channel normalization remains adapter work: mono input is duplicated
to canonical stereo and mono output receives the average of the canonical
left and right samples.

Only hardware and Asterisk compatibility boundaries quantize or expand
samples. PortAudio/ALSA performs the hardware conversion below this adapter's
callback. This preserves a 24-bit-capable internal path while retaining current
S16 Asterisk and CM119 boundaries.

## Status

Version `0.2.0-alpha.1` packages separate input-paced receive and DAC-paced
transmit workers as a Rust `cdylib` with ABI major 2. Release publication requires
the full pull-request quality gate, including native Debian 13 amd64 and arm64
package checks and 100% production line and branch coverage on amd64.

## Published artifacts

The ABI-major-2 release publishes dynamically linked artifacts:

- `librptadv_portaudio_alsa_adapter.so.2`
- `librptadv-portaudio-alsa-adapter2`
- `librptadv-portaudio-alsa-adapter-dev`
- `rptadv_portaudio_alsa_adapter.pc`

The development package contains the public C header, the unversioned
linker symlink, and pkg-config metadata. It ships no static archive.

## ABI and device ownership

ABI major 2 is provided by `librptadv_portaudio_alsa_adapter.so.2`. Consumers
must require a compatible major version through the Debian runtime package or
the pkg-config metadata. An adapter change takes effect only through a
controlled stream and process restart; it is never hot-replaced while audio is
running.

The adapter resolves a stable USB topology, serial number, or both to exactly
one ALSA card and then to exactly one raw PortAudio ALSA device per direction.
When both topology and serial are configured, they must name the same physical
device. Serial discovery reads only that device, never an upstream hub or host;
devices without a serial return an empty serial identity. It rejects missing,
ambiguous, plugin, and non-ALSA-name matches rather
than guessing from a transient card or PortAudio index. A caller resolves the
selection immediately before opening a stream and supplies those exact indexes.
The adapter then takes a process-wide control-plane lease on each resolved
PortAudio device index. A second stream that overlaps either physical device
fails with `RPTADV_AUDIO_DEVICE_BUSY`; the lease is released after stream close
or an open failure. The lease mutex is never touched by the audio callback.
The adapter selects each device's default-low PortAudio latency. Capture and
playback run independently: capture dispatches each input block directly to the
receive worker, and playback asks the transmit worker to fill each output block.
The owning radio core is responsible for any path that bridges the two device
clocks. The adapter contains no PCM ring or resampler.
Both callbacks must run at the highest `SCHED_FIFO` priority (99 on Linux).
Stream startup temporarily applies that policy to the calling thread so the
PortAudio ALSA callback inherits it, then restores the caller's original policy
and priority. PortAudio 19.6's ALSA realtime helper is not used because it selects
FIFO priority 1. The service needs permission for priority 99, for example
`LimitRTPRIO=99` in its systemd unit or equivalent `CAP_SYS_NICE` authorization.
Scheduling setup failure prevents stream start. Restoration failure aborts a
successfully started stream and reports an error; scheduling failures appear as
PortAudio internal errors in the existing statistics. No scheduling calls run
inside an audio callback.
After opening, `stream_get_timing` reports PortAudio's actual input/output
latency estimates and actual sample-rate estimate without exposing a PortAudio
type. It accepts PortAudio 19.6 ALSA's zero stream-info version while validating
finite, nonnegative latencies and a positive sample rate. Consumers use that
control-plane snapshot for playout-delay accounting;
it is not a sample-clock synchronization mechanism.

For a compatibility adapter that still carries a legacy `devstr`, the trailing
`usb_device_select` descriptor entry accepts a stable USB topology, exact USB
serial, native `hw:<card>` or `hw:<card>,<pcm>` identifier, or an explicit
automatic lowest-card policy. It returns both the stable USB identity and the
exact indexes required to open a stream. ALSA aliases remain rejected, and an
identifier plus serial must resolve to the same physical device.

For ALSA mixer control, callers may either supply an ALSA card name directly or
select one by its stable Linux USB interface component, such as `3-1:1.0`.
Control-plane reads and writes refresh ALSA's per-handle cache first, preserving
sibling channel settings changed through another handle. Refresh errors fail
the operation without using stale values; audio callbacks never access mixers.
The latter resolves through `/sys/class/sound` and rejects absent or ambiguous
matches instead of relying on an unstable ALSA card number. The descriptor also
exposes native ALSA mixer steps for narrow compatibility bridges and an exact
0–999 mapping (`0` is the native minimum; `999` is the native maximum). New
control code should prefer the centibel interface when the hardware reports a
dB range.

Each mixer handle also owns one explicit capture or playback path switch. Open
the desired ALSA element and channel, then set its switch; the adapter does not
guess a device-specific CM119 routing plan. This keeps a complete path plan
visible in configuration and works for mixer elements that have only a switch
or only a volume control. These functions remain part of the ABI-2 descriptor.

For a CM119 compatibility adapter, `cm119_mixer_paths_resolve` supplies that
plan without hard-coding whether a particular interface calls its output
control `Speaker` or `Headphone`. It classifies active ALSA simple-mixer paths
using the legacy USB-radio rule: capture-volume paths are RX, playback-volume
paths on a capture element are sidetone, other playback-volume paths are TX A
then TX B, and `Auto Gain Control` is an optional receive compatibility switch.
The returned TX paths are the first two legacy-controlled paths (TX A and TX
B); any additional playback paths remain unchanged as they do today. The
returned paths are copied into caller-owned storage and can be passed to
`mixer_create_for_usb_interface`; resolving does not change a mixer setting.
An unfamiliar layout, non-left/right required channel, or an incomplete RX/TX
plan is rejected rather than guessed.

## Current boundary and deliberate gaps

This adapter is release-ready for PortAudio stream ownership, normalized F32
PCM, raw audio metrics, stable USB-to-audio selection, and ALSA mixer
volume/switch control. It intentionally does not own GPIO, PTT, COR, CTCSS,
EEPROM, parallel-port I/O, radio DSP, tuning policy, or Hamlib. Those belong to
the GPIO, radio-control, and radio-core adapters under ADR 0028.

The stable-device resolver requires the Linux PortAudio ALSA host API to expose
a unique native `hw:<card>,<device>` name. A host that exposes only aliases or
ambiguous device names is rejected; it is not silently mapped to a different
audio device. It also does not monitor hotplug while a stream is open: a device
identity change requires the controlled stream handoff defined by the ADRs.

## Development environment

### Split callback model

Separate PortAudio streams service capture and playback at a fixed 48 kHz.
The capture callback normalizes mono or stereo physical input to canonical
interleaved stereo and invokes the receive worker. The playback callback invokes
the transmit worker, then maps canonical stereo to the physical output. A host
block larger than the corresponding predeclared maximum is split into bounded
worker calls. All workspaces are allocated before either stream starts.

The callback paths do not allocate, lock, block, log, or perform control-plane
I/O. Each worker must consume or produce the complete `frame_count` it receives.
The two callbacks are independent; neither waits for or transfers PCM to the
other inside this adapter. They may run concurrently, so the owning core must
use disjoint callback contexts or make shared state safe without blocking.

Callback duration, frame-count, and late-start diagnostics describe playback.
They report last/maximum execution duration and positive excess between
successive playback-callback starts over the previous block's audio duration.
The late-start counter counts excess greater than 1 ms; the first callback after
each start is excluded. These gaps include PortAudio/ALSA wakeup and dispatch
variation, not just operating-system scheduler delay. Duration includes any
preemption during the callback. Input/output xrun notification timestamps use
`CLOCK_MONOTONIC` nanoseconds since boot, with zero meaning none observed; they
date the notification, not the exact sample where hardware lost continuity.
Capture and playback callback counts, worker failures, physical input/output
meters, and xrun notifications are reported independently where applicable.
Reads and atomic publication do no allocation or logging in either callback.

The repository uses the project-owned, labeled container launcher.  It removes
only stale containers from this exact workspace and records the freshly pulled
image digest before a run:

```sh
tools/run-in-quality-container.sh IMAGE COMMAND [ARG...]
```

`containers/quality.Dockerfile` extends the maintained Debian 13 project
quality image with the Rust toolchain and PortAudio/ALSA development headers.
The workflow implementation belongs in the dedicated
`rptadv-portaudio-alsa-adapter-workflows` repository; this repository contains
only thin workflow callers.

## License

GPL-2.0-only.  See [COPYING](COPYING).
