# rptadv-portaudio-alsa-adapter

`rptadv-portaudio-alsa-adapter` is the hardware-audio boundary for the
standalone `rpt_advanced` architecture.  It will provide a small versioned
shared object that owns PortAudio stream lifetime, ALSA mixer control, and raw
hardware-audio measurements for one radio port.

It does not contain radio DSP, signaling, GPIO, EEPROM, Hamlib control,
Asterisk integration, or audio processing.  Those responsibilities remain in
their respective adapters and in `librptadvradio`.

## PCM contract

All internal audio interfaces use interleaved normalized `f32` PCM with a
full-scale reference from `-1.0` through `+1.0`.  The adapter requests
`paFloat32` from PortAudio, so a PortAudio callback has the same representation
as the internal native-tick boundary.  PortAudio or its host API may still
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

This initial revision defines ABI major 1 through the documented public C
header and implements it as a Rust `cdylib`.  It is not released until its
tests, package validation, and project quality gate pass.

## Published artifacts

The first ABI-major release will publish only dynamically linked artifacts:

- `librptadv_portaudio_alsa_adapter.so.1`
- `librptadv-portaudio-alsa-adapter1`
- `librptadv-portaudio-alsa-adapter-dev`
- `rptadv_portaudio_alsa_adapter.pc`

The development package will contain the public C header, the unversioned
linker symlink, and pkg-config metadata.  It will not ship a static archive.

## ABI and device ownership

ABI major 1 is provided by `librptadv_portaudio_alsa_adapter.so.1`. Consumers
must require a compatible major version through the Debian runtime package or
the pkg-config metadata. An adapter change takes effect only through a
controlled stream and process restart; it is never hot-replaced while audio is
running.

The caller resolves a stable audio-device identity and gives the adapter the
resulting PortAudio device indexes. One selected composition owns a device at a
time. The adapter selects each device's default-low PortAudio latency and
provides direct callback operation with no adapter PCM queue.

## Development environment

The repository uses the project-owned, labeled container launcher.  It removes
only stale containers from this exact workspace and records the freshly pulled
image digest before a run:

```sh
tools/run-in-quality-container.sh IMAGE COMMAND [ARG...]
```

`containers/quality.Dockerfile` extends the maintained Debian 13 project
quality image with the Rust toolchain and PortAudio/ALSA development headers.
The workflow implementation belongs in the dedicated
`rptadv-portaudio-alsa-adapter-workflows` repository; this repository will
contain only thin workflow callers.

## License

GPL-2.0-only.  See [COPYING](COPYING).
