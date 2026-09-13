//! Versioned PortAudio/ALSA adapter with a narrow C-compatible descriptor ABI.
//!
//! The callback-side contract is canonical interleaved stereo `f32` PCM.
//! PortAudio/ALSA performs physical-device conversion below the `paFloat32`
//! callback, keeping S16/S24 details out of the core.

#![deny(unsafe_op_in_unsafe_fn)]

mod capture_ring;
mod ffi;
mod pcm;

use capture_ring::CaptureRing;
use std::cell::UnsafeCell;
use std::collections::HashSet;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fs;
use std::mem::{offset_of, size_of};
use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use ffi::{
    PaDeviceIndex, PaError, PaStream, PaStreamCallbackFlags, PaStreamParameters, SndMixer,
    SndMixerElem,
};
use pcm::{MeterAccumulator, canonical_output_to_device, device_input_to_canonical};

const ABI_VERSION: u32 = 1;
const AUDIO_OK: c_int = 0;
const AUDIO_INVALID_ARGUMENT: c_int = -1;
const AUDIO_NO_MEMORY: c_int = -2;
const AUDIO_PORTAUDIO_ERROR: c_int = -3;
const AUDIO_ALSA_ERROR: c_int = -4;
const AUDIO_UNSUPPORTED: c_int = -5;
const AUDIO_DEVICE_BUSY: c_int = -6;
const DEFAULT_DEVICE: i32 = -1;
const MIXER_NORMALIZED_MAXIMUM: u32 = 999;
const USB_INTERFACE_PATH_CAPACITY: usize = 256;
const USB_SERIAL_CAPACITY: usize = 256;
const CM119_MIXER_PATH_CAPACITY: usize = 2;
const CM119_MIXER_ELEMENT_NAME_CAPACITY: usize = 64;
const CM119_MIXER_PATH_VOLUME: u32 = 1 << 0;
const CM119_MIXER_PATH_SWITCH: u32 = 1 << 1;
const CM119_RX_BOOST_ELEMENT: &[u8] = b"Auto Gain Control";
const USB_SELECTION_EXACT: u32 = 0;
const USB_SELECTION_AUTOMATIC_LOWEST_ALSA_CARD: u32 = 1;
const CAPABILITY_NAME: &[u8] = b"rptadv.portaudio-alsa-audio\0";

type NativeTick = Option<unsafe extern "C" fn(*mut c_void, *const f32, *mut f32, u32) -> i32>;

#[repr(C)]
struct StreamConfig {
    struct_size: u32,
    abi_version: u32,
    native_sample_rate_hz: u32,
    maximum_frame_count: u32,
    input_device_index: i32,
    output_device_index: i32,
    input_device_channels: u32,
    output_device_channels: u32,
    native_tick: NativeTick,
    native_tick_context: *mut c_void,
}

#[repr(C)]
#[derive(Default)]
struct StreamStats {
    struct_size: u32,
    abi_version: u32,
    callback_count: u64,
    callback_frame_count: u64,
    oversized_callback_count: u64,
    native_tick_failure_count: u64,
    input_overflow_count: u64,
    output_underflow_count: u64,
    device_error_count: u64,
    input_queue_capacity_frames: u64,
    input_queue_occupancy_frames: u64,
    output_queue_capacity_frames: u64,
    output_queue_occupancy_frames: u64,
    output_queue_dropped_frame_count: u64,
    input_clip_sample_count: u64,
    output_clip_sample_count: u64,
    input_peak: f32,
    input_rms: f32,
    output_peak: f32,
    output_rms: f32,
    last_portaudio_error: i32,
    /// Initialize the original C ABI's trailing padding before a bounded byte copy.
    alignment_padding: u32,
    callback_last_duration_ns: u64,
    callback_max_duration_ns: u64,
    callback_last_start_delay_ns: u64,
    callback_max_start_delay_ns: u64,
    callback_late_start_count: u64,
    callback_late_start_tolerance_ns: u64,
    last_input_xrun_monotonic_ns: u64,
    last_output_xrun_monotonic_ns: u64,
    callback_clock_error_count: u64,
    capture_callback_count: u64,
    capture_ring_target_frames: u64,
    capture_ring_ratio_correction_ppm: i64,
    capture_ring_missing_frames: u64,
    capture_ring_dropped_frames: u64,
    capture_startup_wait_frames: u64,
}

/// Original ABI-1 snapshot size; trailing diagnostics never overwrite old callers.
const STREAM_STATS_V1_SIZE: usize = offset_of!(StreamStats, callback_last_duration_ns);
/// Timing-only ABI-1 callers predate the independent capture diagnostics.
const STREAM_STATS_TIMING_SIZE: usize = offset_of!(StreamStats, capture_callback_count);
/// Ignore ordinary sub-millisecond callback arrival jitter in the late-start count.
const CALLBACK_LATE_TOLERANCE_NS: u64 = 1_000_000;

#[repr(C)]
#[derive(Default)]
struct StreamTiming {
    struct_size: u32,
    abi_version: u32,
    input_latency_seconds: f64,
    output_latency_seconds: f64,
    sample_rate_hz: f64,
}

#[repr(C)]
struct MixerConfig {
    struct_size: u32,
    card: *const c_char,
    element: *const c_char,
    element_index: u32,
    channel: u32,
    direction: u32,
}

#[repr(C)]
struct UsbMixerConfig {
    struct_size: u32,
    usb_interface_path: *const c_char,
    element: *const c_char,
    element_index: u32,
    channel: u32,
    direction: u32,
}

#[repr(C)]
struct UsbDeviceIdentity {
    struct_size: u32,
    usb_interface_path: *const c_char,
    usb_serial: *const c_char,
    input_device_channels: u32,
    output_device_channels: u32,
}

#[repr(C)]
#[derive(Default)]
struct UsbDeviceSelection {
    struct_size: u32,
    abi_version: u32,
    alsa_card_index: u32,
    input_device_index: i32,
    output_device_index: i32,
}

#[repr(C)]
struct UsbDeviceSelector {
    struct_size: u32,
    selection_policy: u32,
    device_identifier: *const c_char,
    usb_serial: *const c_char,
    input_device_channels: u32,
    output_device_channels: u32,
}

#[repr(C)]
struct UsbDeviceMatch {
    struct_size: u32,
    abi_version: u32,
    usb_interface_path: [c_char; USB_INTERFACE_PATH_CAPACITY],
    usb_serial: [c_char; USB_SERIAL_CAPACITY],
    selection: UsbDeviceSelection,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Cm119MixerPath {
    element: [c_char; CM119_MIXER_ELEMENT_NAME_CAPACITY],
    element_index: u32,
    channel: u32,
    direction: u32,
    capabilities: u32,
}

impl Default for Cm119MixerPath {
    fn default() -> Self {
        Self {
            element: [0; CM119_MIXER_ELEMENT_NAME_CAPACITY],
            element_index: 0,
            channel: 0,
            direction: 0,
            capabilities: 0,
        }
    }
}

#[repr(C)]
#[derive(Default)]
struct Cm119MixerPaths {
    struct_size: u32,
    abi_version: u32,
    rx_capture_path_count: u32,
    tx_playback_path_count: u32,
    sidetone_path_count: u32,
    rx_compatibility_switch_path_count: u32,
    rx_capture_paths: [Cm119MixerPath; CM119_MIXER_PATH_CAPACITY],
    tx_playback_paths: [Cm119MixerPath; CM119_MIXER_PATH_CAPACITY],
    sidetone_paths: [Cm119MixerPath; CM119_MIXER_PATH_CAPACITY],
    rx_compatibility_switch_paths: [Cm119MixerPath; CM119_MIXER_PATH_CAPACITY],
}

impl Default for UsbDeviceMatch {
    fn default() -> Self {
        Self {
            struct_size: 0,
            abi_version: 0,
            usb_interface_path: [0; USB_INTERFACE_PATH_CAPACITY],
            usb_serial: [0; USB_SERIAL_CAPACITY],
            selection: UsbDeviceSelection::default(),
        }
    }
}

#[repr(C)]
pub struct AdapterDescriptor {
    struct_size: u32,
    abi_version: u32,
    capability_name: *const c_char,
    stream_create: extern "C" fn(*const StreamConfig, *mut *mut AudioStream) -> c_int,
    stream_start: extern "C" fn(*mut AudioStream) -> c_int,
    stream_stop: extern "C" fn(*mut AudioStream) -> c_int,
    stream_get_stats: extern "C" fn(*const AudioStream, *mut StreamStats) -> c_int,
    stream_destroy: extern "C" fn(*mut AudioStream),
    mixer_create: extern "C" fn(*const MixerConfig, *mut *mut AudioMixer) -> c_int,
    mixer_get_range_centibels: extern "C" fn(*const AudioMixer, *mut i64, *mut i64) -> c_int,
    mixer_get_centibels: extern "C" fn(*const AudioMixer, *mut i64) -> c_int,
    mixer_set_centibels: extern "C" fn(*mut AudioMixer, i64) -> c_int,
    mixer_destroy: extern "C" fn(*mut AudioMixer),
    mixer_create_for_usb_interface:
        extern "C" fn(*const UsbMixerConfig, *mut *mut AudioMixer) -> c_int,
    mixer_get_range_steps: extern "C" fn(*const AudioMixer, *mut i64, *mut i64) -> c_int,
    mixer_get_steps: extern "C" fn(*const AudioMixer, *mut i64) -> c_int,
    mixer_set_steps: extern "C" fn(*mut AudioMixer, i64) -> c_int,
    mixer_get_normalized: extern "C" fn(*const AudioMixer, *mut u32) -> c_int,
    mixer_set_normalized: extern "C" fn(*mut AudioMixer, u32) -> c_int,
    mixer_get_switch: extern "C" fn(*const AudioMixer, *mut u32) -> c_int,
    mixer_set_switch: extern "C" fn(*mut AudioMixer, u32) -> c_int,
    usb_device_resolve: extern "C" fn(*const UsbDeviceIdentity, *mut UsbDeviceSelection) -> c_int,
    usb_device_select: extern "C" fn(*const UsbDeviceSelector, *mut UsbDeviceMatch) -> c_int,
    stream_get_timing: extern "C" fn(*const AudioStream, *mut StreamTiming) -> c_int,
    cm119_mixer_paths_resolve: extern "C" fn(*const c_char, *mut Cm119MixerPaths) -> c_int,
}

// The descriptor is immutable function and data pointers that remain valid for
// the lifetime of this loaded shared object.
unsafe impl Sync for AdapterDescriptor {}

#[derive(Clone, Copy)]
struct ValidatedStreamConfig {
    sample_rate_hz: u32,
    maximum_frame_count: usize,
    input_device_index: i32,
    output_device_index: i32,
    input_channels: usize,
    output_channels: usize,
    native_tick: unsafe extern "C" fn(*mut c_void, *const f32, *mut f32, u32) -> i32,
    native_tick_context: *mut c_void,
}

impl ValidatedStreamConfig {
    unsafe fn from_ffi(config: *const StreamConfig) -> Result<Self, c_int> {
        if config.is_null() {
            return Err(AUDIO_INVALID_ARGUMENT);
        }

        let config = unsafe { &*config };
        if config.struct_size < size_of::<StreamConfig>() as u32
            || config.abi_version != ABI_VERSION
            || config.native_sample_rate_hz == 0
            || config.maximum_frame_count == 0
            || !matches!(config.input_device_channels, 1 | 2)
            || !matches!(config.output_device_channels, 1 | 2)
            || config.input_device_index < DEFAULT_DEVICE
            || config.output_device_index < DEFAULT_DEVICE
        {
            return Err(AUDIO_INVALID_ARGUMENT);
        }

        let native_tick = config.native_tick.ok_or(AUDIO_INVALID_ARGUMENT)?;
        Ok(Self {
            sample_rate_hz: config.native_sample_rate_hz,
            maximum_frame_count: config.maximum_frame_count as usize,
            input_device_index: config.input_device_index,
            output_device_index: config.output_device_index,
            input_channels: config.input_device_channels as usize,
            output_channels: config.output_device_channels as usize,
            native_tick,
            native_tick_context: config.native_tick_context,
        })
    }
}

#[derive(Default)]
struct SharedStats {
    capture_callback_count: AtomicU64,
    callback_count: AtomicU64,
    callback_frame_count: AtomicU64,
    oversized_callback_count: AtomicU64,
    native_tick_failure_count: AtomicU64,
    input_overflow_count: AtomicU64,
    output_underflow_count: AtomicU64,
    device_error_count: AtomicU64,
    input_clip_sample_count: AtomicU64,
    output_clip_sample_count: AtomicU64,
    input_peak_bits: AtomicU32,
    input_rms_bits: AtomicU32,
    output_peak_bits: AtomicU32,
    output_rms_bits: AtomicU32,
    last_portaudio_error: AtomicI32,
    callback_last_duration_ns: AtomicU64,
    callback_max_duration_ns: AtomicU64,
    callback_last_start_delay_ns: AtomicU64,
    callback_max_start_delay_ns: AtomicU64,
    callback_late_start_count: AtomicU64,
    last_input_xrun_monotonic_ns: AtomicU64,
    last_output_xrun_monotonic_ns: AtomicU64,
    callback_clock_error_count: AtomicU64,
    previous_callback_start_ns: AtomicU64,
    previous_callback_period_ns: AtomicU64,
}

impl SharedStats {
    fn record_capture_overflow_timestamp(&self, now: u64) {
        if now != 0 {
            self.last_input_xrun_monotonic_ns
                .store(now, Ordering::Relaxed);
        } else {
            self.callback_clock_error_count
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Measure arrival gaps against the previous block's audio duration, not a
    /// free-running ideal clock that would confuse hardware drift with lateness.
    fn callback_begin(&self, now: u64, period_ns: u64, flags: PaStreamCallbackFlags) {
        if now == 0 {
            self.callback_clock_error_count
                .fetch_add(1, Ordering::Relaxed);
            self.previous_callback_start_ns.store(0, Ordering::Relaxed);
            return;
        }
        let previous = self.previous_callback_start_ns.swap(now, Ordering::Relaxed);
        let period = self
            .previous_callback_period_ns
            .swap(period_ns, Ordering::Relaxed);
        let delay = if previous == 0 || period == 0 {
            0
        } else {
            now.saturating_sub(previous).saturating_sub(period)
        };
        self.callback_last_start_delay_ns
            .store(delay, Ordering::Relaxed);
        self.callback_max_start_delay_ns
            .fetch_max(delay, Ordering::Relaxed);
        if delay > CALLBACK_LATE_TOLERANCE_NS {
            self.callback_late_start_count
                .fetch_add(1, Ordering::Relaxed);
        }
        if flags & ffi::PA_INPUT_OVERFLOW != 0 {
            self.last_input_xrun_monotonic_ns
                .store(now, Ordering::Relaxed);
        }
        if flags & ffi::PA_OUTPUT_UNDERFLOW != 0 {
            self.last_output_xrun_monotonic_ns
                .store(now, Ordering::Relaxed);
        }
    }

    /// Publish wall-clock execution duration, including any preemption inside
    /// the callback. Never log, allocate, or wait for the statistics reader.
    fn callback_end(&self, start: u64, end: u64) {
        if end == 0 {
            self.callback_clock_error_count
                .fetch_add(1, Ordering::Relaxed);
        }
        if start == 0 || end == 0 {
            return;
        }
        let duration = end.saturating_sub(start);
        self.callback_last_duration_ns
            .store(duration, Ordering::Relaxed);
        self.callback_max_duration_ns
            .fetch_max(duration, Ordering::Relaxed);
    }

    fn record_portaudio_error(&self, error: PaError) {
        self.last_portaudio_error.store(error, Ordering::Release);
        self.device_error_count.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self, stats: &mut StreamStats) {
        stats.abi_version = ABI_VERSION;
        stats.callback_count = self.callback_count.load(Ordering::Acquire);
        stats.callback_frame_count = self.callback_frame_count.load(Ordering::Acquire);
        stats.oversized_callback_count = self.oversized_callback_count.load(Ordering::Acquire);
        stats.native_tick_failure_count = self.native_tick_failure_count.load(Ordering::Acquire);
        stats.input_overflow_count = self.input_overflow_count.load(Ordering::Acquire);
        stats.output_underflow_count = self.output_underflow_count.load(Ordering::Acquire);
        stats.device_error_count = self.device_error_count.load(Ordering::Acquire);
        // Capture queue diagnostics are filled from the shared ring separately.
        stats.input_queue_capacity_frames = 0;
        stats.input_queue_occupancy_frames = 0;
        stats.output_queue_capacity_frames = 0;
        stats.output_queue_occupancy_frames = 0;
        stats.output_queue_dropped_frame_count = 0;
        stats.input_clip_sample_count = self.input_clip_sample_count.load(Ordering::Acquire);
        stats.output_clip_sample_count = self.output_clip_sample_count.load(Ordering::Acquire);
        stats.input_peak = f32::from_bits(self.input_peak_bits.load(Ordering::Acquire));
        stats.input_rms = f32::from_bits(self.input_rms_bits.load(Ordering::Acquire));
        stats.output_peak = f32::from_bits(self.output_peak_bits.load(Ordering::Acquire));
        stats.output_rms = f32::from_bits(self.output_rms_bits.load(Ordering::Acquire));
        stats.last_portaudio_error = self.last_portaudio_error.load(Ordering::Acquire);
        stats.callback_last_duration_ns = self.callback_last_duration_ns.load(Ordering::Acquire);
        stats.callback_max_duration_ns = self.callback_max_duration_ns.load(Ordering::Acquire);
        stats.callback_last_start_delay_ns =
            self.callback_last_start_delay_ns.load(Ordering::Acquire);
        stats.callback_max_start_delay_ns =
            self.callback_max_start_delay_ns.load(Ordering::Acquire);
        stats.callback_late_start_count = self.callback_late_start_count.load(Ordering::Acquire);
        stats.callback_late_start_tolerance_ns = CALLBACK_LATE_TOLERANCE_NS;
        stats.last_input_xrun_monotonic_ns =
            self.last_input_xrun_monotonic_ns.load(Ordering::Acquire);
        stats.last_output_xrun_monotonic_ns =
            self.last_output_xrun_monotonic_ns.load(Ordering::Acquire);
        stats.callback_clock_error_count = self.callback_clock_error_count.load(Ordering::Acquire);
        stats.capture_callback_count = self.capture_callback_count.load(Ordering::Acquire);
    }
}

/// Capture and playback own disjoint callback contexts. UnsafeCell prevents
/// control-plane references to this container from aliasing callback mutation.
#[repr(C)]
struct AudioStream {
    portaudio_stream: *mut PaStream,
    capture_stream: *mut PaStream,
    started: [AtomicBool; 2],
    functions: &'static ffi::FunctionTable,
    config: ValidatedStreamConfig,
    capture: Box<UnsafeCell<CaptureState>>,
    playback: Box<UnsafeCell<PlaybackState>>,
    stats: Arc<SharedStats>,
    ring: Arc<CaptureRing>,
    /// Serialize diagnostic observers with quiescent ring reset, never callbacks.
    ring_control: Mutex<()>,
    device_lease: Option<DeviceLease>,
}

/// The capture callback is the sole producer and raw hardware meter owner.
struct CaptureState {
    maximum_frame_count: usize,
    silence: Box<[f32]>,
    input_meter: MeterAccumulator,
    stats: Arc<SharedStats>,
    ring: Arc<CaptureRing>,
}

/// Playback is the sole ring consumer and the sole caller of the native tick.
struct PlaybackState {
    config: ValidatedStreamConfig,
    mono_input: Box<[f32]>,
    canonical_input: Box<[f32]>,
    canonical_output: Box<[f32]>,
    output_meter: MeterAccumulator,
    stats: Arc<SharedStats>,
    ring: Arc<CaptureRing>,
}

impl AudioStream {
    fn new(
        functions: &'static ffi::FunctionTable,
        config: ValidatedStreamConfig,
    ) -> Result<Self, c_int> {
        let sample_count = config.maximum_frame_count * pcm::CANONICAL_CHANNELS;
        let stats = Arc::new(SharedStats::default());
        let ring = Arc::new(CaptureRing::new(config.maximum_frame_count)?);
        Ok(Self {
            portaudio_stream: ptr::null_mut(),
            capture_stream: ptr::null_mut(),
            started: [AtomicBool::new(false), AtomicBool::new(false)],
            functions,
            config,
            capture: Box::new(UnsafeCell::new(CaptureState {
                maximum_frame_count: config.maximum_frame_count,
                silence: vec![0.0; config.maximum_frame_count].into_boxed_slice(),
                input_meter: MeterAccumulator::default(),
                stats: Arc::clone(&stats),
                ring: Arc::clone(&ring),
            })),
            playback: Box::new(UnsafeCell::new(PlaybackState {
                config,
                mono_input: vec![0.0; config.maximum_frame_count].into_boxed_slice(),
                canonical_input: vec![0.0; sample_count].into_boxed_slice(),
                canonical_output: vec![0.0; sample_count].into_boxed_slice(),
                output_meter: MeterAccumulator::default(),
                stats: Arc::clone(&stats),
                ring: Arc::clone(&ring),
            })),
            stats,
            ring,
            ring_control: Mutex::new(()),
            device_lease: None,
        })
    }

    fn release_device_lease(&mut self) {
        drop(self.device_lease.take());
    }

    /// Attempt both endpoints even after one failure. A successful stop/abort
    /// joins the corresponding callback before its context can be reused.
    fn stop_endpoints(&self) -> c_int {
        let mut status = AUDIO_OK;
        for (index, handle) in [self.portaudio_stream, self.capture_stream]
            .into_iter()
            .enumerate()
        {
            if handle.is_null() {
                continue;
            }
            let active = unsafe { (self.functions.portaudio.is_stream_active)(handle) };
            if active == 0 && !self.started[index].load(Ordering::Relaxed) {
                continue;
            }
            let result = if active < 0 {
                active
            } else {
                unsafe { (self.functions.portaudio.stop_stream)(handle) }
            };
            if result != ffi::PA_NO_ERROR {
                self.stats.record_portaudio_error(result);
                let abort = unsafe { (self.functions.portaudio.abort_stream)(handle) };
                if abort != ffi::PA_NO_ERROR {
                    self.stats.record_portaudio_error(abort);
                } else {
                    self.started[index].store(false, Ordering::Relaxed);
                }
                status = AUDIO_PORTAUDIO_ERROR;
            } else {
                self.started[index].store(false, Ordering::Relaxed);
            }
        }
        status
    }
}

impl CaptureState {
    fn publish_meter(&self) {
        self.stats
            .input_peak_bits
            .store(self.input_meter.peak().to_bits(), Ordering::Release);
        self.stats
            .input_rms_bits
            .store(self.input_meter.rms().to_bits(), Ordering::Release);
        self.stats
            .input_clip_sample_count
            .store(self.input_meter.clip_sample_count(), Ordering::Release);
    }

    unsafe fn process_callback(
        &mut self,
        input: *const f32,
        frame_count: usize,
        flags: PaStreamCallbackFlags,
    ) -> c_int {
        self.stats
            .capture_callback_count
            .fetch_add(1, Ordering::Relaxed);
        if flags & ffi::PA_INPUT_OVERFLOW != 0 {
            self.stats
                .input_overflow_count
                .fetch_add(1, Ordering::Relaxed);
            self.stats
                .record_capture_overflow_timestamp(ffi::monotonic_ns());
        }
        let mut offset = 0;
        while offset < frame_count {
            let frames = (frame_count - offset).min(self.maximum_frame_count);
            let samples = if input.is_null() {
                &self.silence[..frames]
            } else {
                unsafe { std::slice::from_raw_parts(input.add(offset), frames) }
            };
            self.input_meter.observe(samples);
            // This context is the only producer for the lifetime of this ring.
            if unsafe { self.ring.push(samples) }.is_err() {
                self.stats
                    .device_error_count
                    .fetch_add(1, Ordering::Relaxed);
                self.publish_meter();
                return ffi::PA_ABORT;
            }
            offset += frames;
        }
        self.publish_meter();
        ffi::PA_CONTINUE
    }
}

impl PlaybackState {
    fn publish_meter(&self) {
        self.stats
            .output_peak_bits
            .store(self.output_meter.peak().to_bits(), Ordering::Release);
        self.stats
            .output_rms_bits
            .store(self.output_meter.rms().to_bits(), Ordering::Release);
        self.stats
            .output_clip_sample_count
            .store(self.output_meter.clip_sample_count(), Ordering::Release);
    }

    /// Split host blocks at the prepared maximum. Input production is supplied
    /// separately so the native tick remains one bounded render path.
    unsafe fn render_callback<F>(
        &mut self,
        output: *mut f32,
        frame_count: usize,
        flags: PaStreamCallbackFlags,
        mut fill_input: F,
    ) -> c_int
    where
        F: FnMut(&mut [f32], &mut [f32]) -> Result<(), c_int>,
    {
        if output.is_null() {
            self.stats
                .native_tick_failure_count
                .fetch_add(1, Ordering::Relaxed);
            return ffi::PA_ABORT;
        }
        let physical_output = unsafe {
            std::slice::from_raw_parts_mut(output, frame_count * self.config.output_channels)
        };
        physical_output.fill(0.0);
        if flags & ffi::PA_OUTPUT_UNDERFLOW != 0 {
            self.stats
                .output_underflow_count
                .fetch_add(1, Ordering::Relaxed);
        }
        self.stats.callback_count.fetch_add(1, Ordering::Relaxed);
        self.stats
            .callback_frame_count
            .fetch_add(frame_count as u64, Ordering::Relaxed);
        if frame_count > self.config.maximum_frame_count {
            self.stats
                .oversized_callback_count
                .fetch_add(1, Ordering::Relaxed);
        }
        let mut offset = 0;
        while offset < frame_count {
            let frames = (frame_count - offset).min(self.config.maximum_frame_count);
            let samples = frames * pcm::CANONICAL_CHANNELS;
            let canonical_input = &mut self.canonical_input[..samples];
            let canonical_output = &mut self.canonical_output[..samples];
            if fill_input(&mut self.mono_input[..frames], canonical_input).is_err() {
                self.stats
                    .device_error_count
                    .fetch_add(1, Ordering::Relaxed);
                self.publish_meter();
                return ffi::PA_ABORT;
            }
            canonical_output.fill(0.0);
            let result = unsafe {
                (self.config.native_tick)(
                    self.config.native_tick_context,
                    canonical_input.as_ptr(),
                    canonical_output.as_mut_ptr(),
                    frames as u32,
                )
            };
            if result != 0 {
                self.stats
                    .native_tick_failure_count
                    .fetch_add(1, Ordering::Relaxed);
                self.publish_meter();
                return ffi::PA_ABORT;
            }
            let device_output = &mut physical_output[offset * self.config.output_channels
                ..(offset + frames) * self.config.output_channels];
            canonical_output_to_device(
                canonical_output,
                self.config.output_channels,
                device_output,
            );
            self.output_meter.observe(device_output);
            offset += frames;
        }
        self.publish_meter();
        ffi::PA_CONTINUE
    }

    unsafe fn process_callback(
        &mut self,
        output: *mut f32,
        frame_count: usize,
        flags: PaStreamCallbackFlags,
    ) -> c_int {
        // Borrow the separately allocated ring, not this mutable callback state.
        // Its lifetime covers this call; playback is its only consumer.
        let ring = Arc::as_ptr(&self.ring);
        unsafe {
            self.render_callback(output, frame_count, flags, |mono, canonical| {
                (*ring).pull(mono)?;
                device_input_to_canonical(Some(mono), 1, canonical);
                Ok(())
            })
        }
    }
}

unsafe extern "C" fn capture_callback(
    input: *const c_void,
    _output: *mut c_void,
    frame_count: std::ffi::c_ulong,
    _time_info: *const ffi::PaStreamCallbackTimeInfo,
    status_flags: PaStreamCallbackFlags,
    user_data: *mut c_void,
) -> c_int {
    let Some(mut state) = NonNull::new(user_data.cast::<CaptureState>()) else {
        return ffi::PA_ABORT;
    };
    unsafe {
        state
            .as_mut()
            .process_callback(input.cast::<f32>(), frame_count as usize, status_flags)
    }
}

unsafe extern "C" fn portaudio_callback(
    _input: *const c_void,
    output: *mut c_void,
    frame_count: std::ffi::c_ulong,
    _time_info: *const ffi::PaStreamCallbackTimeInfo,
    status_flags: PaStreamCallbackFlags,
    user_data: *mut c_void,
) -> c_int {
    let Some(mut state) = NonNull::new(user_data.cast::<PlaybackState>()) else {
        return ffi::PA_ABORT;
    };
    let state = unsafe { state.as_mut() };
    let start = ffi::monotonic_ns();
    let period = frame_count * 1_000_000_000 / u64::from(state.config.sample_rate_hz);
    // Input xrun accounting belongs exclusively to the capture callback.
    let flags = status_flags & !ffi::PA_INPUT_OVERFLOW;
    state.stats.callback_begin(start, period, flags);
    let result =
        unsafe { state.process_callback(output.cast::<f32>(), frame_count as usize, flags) };
    state.stats.callback_end(start, ffi::monotonic_ns());
    result
}

#[derive(Default)]
struct PortAudioRuntime {
    references: usize,
    functions: Option<&'static ffi::FunctionTable>,
}

static PORTAUDIO_RUNTIME: OnceLock<Mutex<PortAudioRuntime>> = OnceLock::new();

fn portaudio_runtime() -> &'static Mutex<PortAudioRuntime> {
    PORTAUDIO_RUNTIME.get_or_init(|| Mutex::new(PortAudioRuntime::default()))
}

/// Process-wide ownership of physical PortAudio devices.
///
/// PortAudio device indexes are unique for the active runtime.  Reserving each
/// resolved input and output index independently prevents a second stream from
/// opening the same full-duplex USB interface through a different direction
/// pairing.  This mutex is control-plane-only and is never reached from the
/// PortAudio callback.
#[derive(Default)]
struct DeviceLeaseRegistry {
    claimed_devices: HashSet<PaDeviceIndex>,
}

static DEVICE_LEASE_REGISTRY: OnceLock<Mutex<DeviceLeaseRegistry>> = OnceLock::new();

fn device_lease_registry() -> &'static Mutex<DeviceLeaseRegistry> {
    DEVICE_LEASE_REGISTRY.get_or_init(|| Mutex::new(DeviceLeaseRegistry::default()))
}

/// One stream's exclusive reservation of its resolved physical devices.
struct DeviceLease {
    devices: [PaDeviceIndex; 2],
    count: usize,
}

impl DeviceLease {
    fn acquire(input_device: PaDeviceIndex, output_device: PaDeviceIndex) -> Result<Self, c_int> {
        let mut registry = device_lease_registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if registry.claimed_devices.contains(&input_device)
            || registry.claimed_devices.contains(&output_device)
        {
            return Err(AUDIO_DEVICE_BUSY);
        }

        registry.claimed_devices.insert(input_device);
        let count = if input_device == output_device {
            1
        } else {
            registry.claimed_devices.insert(output_device);
            2
        };
        Ok(Self {
            devices: [input_device, output_device],
            count,
        })
    }
}

impl Drop for DeviceLease {
    fn drop(&mut self) {
        let mut registry = device_lease_registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for device in &self.devices[..self.count] {
            registry.claimed_devices.remove(device);
        }
    }
}

fn portaudio_acquire(functions: &'static ffi::FunctionTable) -> Result<(), c_int> {
    let mut runtime = portaudio_runtime()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if runtime.references == 0 {
        let result = unsafe { (functions.portaudio.initialize)() };
        if result != ffi::PA_NO_ERROR {
            return Err(AUDIO_PORTAUDIO_ERROR);
        }
        runtime.functions = Some(functions);
    } else if !std::ptr::eq(
        runtime.functions.expect("active runtime has functions"),
        functions,
    ) {
        return Err(AUDIO_PORTAUDIO_ERROR);
    }
    runtime.references += 1;
    Ok(())
}

fn portaudio_release(functions: &'static ffi::FunctionTable) {
    let mut runtime = portaudio_runtime()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if runtime.references == 0 || !std::ptr::eq(runtime.functions.unwrap_or(functions), functions) {
        return;
    }
    runtime.references -= 1;
    if runtime.references == 0 {
        let _ = unsafe { (functions.portaudio.terminate)() };
        runtime.functions = None;
    }
}

fn resolve_device(
    functions: &'static ffi::FunctionTable,
    configured_index: i32,
    channels: usize,
    input: bool,
) -> Result<(PaDeviceIndex, PaStreamParameters), c_int> {
    let device = if configured_index == DEFAULT_DEVICE {
        if input {
            unsafe { (functions.portaudio.get_default_input_device)() }
        } else {
            unsafe { (functions.portaudio.get_default_output_device)() }
        }
    } else {
        configured_index
    };
    let device_count = unsafe { (functions.portaudio.get_device_count)() };
    if device_count < 0 {
        return Err(AUDIO_PORTAUDIO_ERROR);
    }
    if device == ffi::PA_NO_DEVICE || device < 0 || device >= device_count {
        return Err(AUDIO_UNSUPPORTED);
    }
    let info = unsafe { (functions.portaudio.get_device_info)(device) };
    let info = NonNull::new(info.cast_mut()).ok_or(AUDIO_PORTAUDIO_ERROR)?;
    let info = unsafe { info.as_ref() };
    let supported_channels = if input {
        info.max_input_channels
    } else {
        info.max_output_channels
    };
    if supported_channels < channels as c_int {
        return Err(AUDIO_UNSUPPORTED);
    }
    let latency = if input {
        info.default_low_input_latency
    } else {
        info.default_low_output_latency
    };
    Ok((
        device,
        PaStreamParameters {
            device,
            channel_count: channels as c_int,
            sample_format: ffi::PA_FLOAT_32,
            suggested_latency: latency,
            host_api_specific_stream_info: ptr::null_mut(),
        },
    ))
}

/// A native ALSA card and PCM device extracted from a PortAudio ALSA name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RawAlsaDevice {
    card_index: u32,
    pcm_device: u32,
}

/// Parse an unsigned decimal prefix without accepting empty or overflowing input.
fn parse_decimal_prefix(value: &str) -> Option<(u32, &str)> {
    let length = value
        .bytes()
        .position(|byte| !byte.is_ascii_digit())
        .unwrap_or(value.len());
    if length == 0 {
        return None;
    }
    Some((value[..length].parse().ok()?, &value[length..]))
}

/// Return a native `hw:<card>,<pcm>` token from a PortAudio ALSA device name.
///
/// The ALSA host API embeds raw names in human-readable device names. Plugin
/// aliases are deliberately excluded so the adapter never hides a format or
/// latency policy behind `plughw:` or another ALSA plugin.
fn raw_alsa_device_from_portaudio_name(name: &str) -> Option<RawAlsaDevice> {
    for (offset, _) in name.match_indices("hw:") {
        if offset != 0
            && !name.as_bytes()[..offset]
                .last()
                .is_some_and(|byte| *byte == b'(' || byte.is_ascii_whitespace())
        {
            continue;
        }
        let Some((card_index, remainder)) = parse_decimal_prefix(&name[offset + 3..]) else {
            continue;
        };
        let Some(remainder) = remainder.strip_prefix(',') else {
            continue;
        };
        let Some((pcm_device, remainder)) = parse_decimal_prefix(remainder) else {
            continue;
        };
        if remainder.is_empty()
            || remainder
                .as_bytes()
                .first()
                .is_some_and(|byte| *byte == b')' || byte.is_ascii_whitespace())
        {
            return Some(RawAlsaDevice {
                card_index,
                pcm_device,
            });
        }
    }
    None
}

/// Resolve one uniquely named PortAudio ALSA device for the requested direction.
fn resolve_portaudio_device_for_alsa_card(
    functions: &'static ffi::FunctionTable,
    card_index: u32,
    channels: usize,
    input: bool,
) -> Result<PaDeviceIndex, c_int> {
    resolve_portaudio_device_for_alsa_endpoint(functions, card_index, None, channels, input)
}

/// Resolve one uniquely named raw PortAudio ALSA endpoint for a requested direction.
fn resolve_portaudio_device_for_alsa_endpoint(
    functions: &'static ffi::FunctionTable,
    card_index: u32,
    pcm_device: Option<u32>,
    channels: usize,
    input: bool,
) -> Result<PaDeviceIndex, c_int> {
    let device_count = unsafe { (functions.portaudio.get_device_count)() };
    if device_count < 0 {
        return Err(AUDIO_PORTAUDIO_ERROR);
    }
    let mut result = None;
    for device in 0..device_count {
        let info = unsafe { (functions.portaudio.get_device_info)(device) };
        let info = NonNull::new(info.cast_mut()).ok_or(AUDIO_PORTAUDIO_ERROR)?;
        let info = unsafe { info.as_ref() };
        if info.name.is_null() {
            return Err(AUDIO_PORTAUDIO_ERROR);
        }
        let Ok(name) = unsafe { CStr::from_ptr(info.name) }.to_str() else {
            continue;
        };
        let supported_channels = if input {
            info.max_input_channels
        } else {
            info.max_output_channels
        };
        let Some(raw_device) = raw_alsa_device_from_portaudio_name(name) else {
            continue;
        };
        if supported_channels < channels as c_int
            || raw_device.card_index != card_index
            || pcm_device.is_some_and(|expected| raw_device.pcm_device != expected)
        {
            continue;
        }
        if result.replace(device).is_some() {
            return Err(AUDIO_UNSUPPORTED);
        }
    }
    result.ok_or(AUDIO_UNSUPPORTED)
}

fn stream_create_with_functions(
    functions: &'static ffi::FunctionTable,
    config: *const StreamConfig,
    stream: *mut *mut AudioStream,
) -> c_int {
    if stream.is_null() {
        return AUDIO_INVALID_ARGUMENT;
    }
    unsafe {
        *stream = ptr::null_mut();
    }
    let config = match unsafe { ValidatedStreamConfig::from_ffi(config) } {
        Ok(config) => config,
        Err(error) => return error,
    };
    // This node-only experiment uses the released mono clock-recovery ring.
    // Never reinterpret interleaved stereo as a mono sample stream.
    if config.input_channels != 1 || config.sample_rate_hz != 48_000 {
        return AUDIO_UNSUPPORTED;
    }
    if let Err(error) = portaudio_acquire(functions) {
        return error;
    }
    let (input_device, input_parameters) = match resolve_device(
        functions,
        config.input_device_index,
        config.input_channels,
        true,
    ) {
        Ok(parameters) => parameters,
        Err(error) => {
            portaudio_release(functions);
            return error;
        }
    };
    let (output_device, output_parameters) = match resolve_device(
        functions,
        config.output_device_index,
        config.output_channels,
        false,
    ) {
        Ok(parameters) => parameters,
        Err(error) => {
            portaudio_release(functions);
            return error;
        }
    };
    let device_lease = match DeviceLease::acquire(input_device, output_device) {
        Ok(device_lease) => device_lease,
        Err(error) => {
            portaudio_release(functions);
            return error;
        }
    };
    let mut boxed_stream = match AudioStream::new(functions, config) {
        Ok(stream) => Box::new(stream),
        Err(error) => {
            drop(device_lease);
            portaudio_release(functions);
            return error;
        }
    };
    boxed_stream.device_lease = Some(device_lease);
    let result = unsafe {
        (functions.portaudio.open_stream)(
            &mut boxed_stream.capture_stream,
            &input_parameters,
            ptr::null(),
            f64::from(config.sample_rate_hz),
            config.maximum_frame_count as std::ffi::c_ulong,
            0,
            Some(capture_callback),
            boxed_stream.capture.get().cast::<c_void>(),
        )
    };
    if result != ffi::PA_NO_ERROR {
        boxed_stream.stats.record_portaudio_error(result);
        drop(boxed_stream);
        portaudio_release(functions);
        return AUDIO_PORTAUDIO_ERROR;
    }
    let result = unsafe {
        (functions.portaudio.open_stream)(
            &mut boxed_stream.portaudio_stream,
            ptr::null(),
            &output_parameters,
            f64::from(config.sample_rate_hz),
            config.maximum_frame_count as std::ffi::c_ulong,
            0,
            Some(portaudio_callback),
            boxed_stream.playback.get().cast::<c_void>(),
        )
    };
    if result != ffi::PA_NO_ERROR {
        boxed_stream.stats.record_portaudio_error(result);
        let close = unsafe { (functions.portaudio.close_stream)(boxed_stream.capture_stream) };
        if close == ffi::PA_NO_ERROR {
            drop(boxed_stream);
            portaudio_release(functions);
        } else {
            // An unclosed native handle must retain its callback storage,
            // runtime reference and lease rather than risk use-after-free.
            boxed_stream.stats.record_portaudio_error(close);
            let _ = Box::into_raw(boxed_stream);
        }
        return AUDIO_PORTAUDIO_ERROR;
    }
    unsafe {
        *stream = Box::into_raw(boxed_stream);
    }
    AUDIO_OK
}

extern "C" fn stream_create(config: *const StreamConfig, stream: *mut *mut AudioStream) -> c_int {
    stream_create_with_functions(&ffi::PRODUCTION_FUNCTIONS, config, stream)
}

extern "C" fn stream_start(stream: *mut AudioStream) -> c_int {
    let Some(stream) = NonNull::new(stream) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    let stream = unsafe { stream.as_ref() };
    let active = unsafe { (stream.functions.portaudio.is_stream_active)(stream.portaudio_stream) };
    let capture_active =
        unsafe { (stream.functions.portaudio.is_stream_active)(stream.capture_stream) };
    if active < 0 || capture_active < 0 {
        stream
            .stats
            .record_portaudio_error(if active < 0 { active } else { capture_active });
        return AUDIO_PORTAUDIO_ERROR;
    }
    if active == 1 && capture_active == 1 {
        return AUDIO_OK;
    }
    if stream.stop_endpoints() != AUDIO_OK {
        return AUDIO_PORTAUDIO_ERROR;
    }
    // Both endpoints are quiescent; prevent a diagnostic observer from using
    // the old released handle while resetting the capture clock boundary.
    let Ok(_ring_guard) = stream.ring_control.lock() else {
        return AUDIO_PORTAUDIO_ERROR;
    };
    if unsafe { stream.ring.reset() }.is_err() {
        stream
            .stats
            .device_error_count
            .fetch_add(1, Ordering::Relaxed);
        return AUDIO_PORTAUDIO_ERROR;
    }
    // PortAudio 19.6 ALSA's RT helper requests FIFO priority 1, not the highest
    // priority. Its default callback thread inherits the starter's scheduling.
    // Elevate only this lifecycle call, then restore the caller on every path.
    let scheduling = &stream.functions.scheduling;
    let thread = unsafe { (scheduling.thread_self)() };
    let mut policy = 0;
    let mut saved = ffi::SchedParam::default();
    if unsafe { (scheduling.get)(thread, &mut policy, &mut saved) } != 0 {
        stream.stats.record_portaudio_error(ffi::PA_INTERNAL_ERROR);
        return AUDIO_PORTAUDIO_ERROR;
    }
    let priority = unsafe { (scheduling.priority_max)(ffi::SCHED_FIFO) };
    if priority < 1 {
        stream.stats.record_portaudio_error(ffi::PA_INTERNAL_ERROR);
        return AUDIO_PORTAUDIO_ERROR;
    }
    let highest = ffi::SchedParam {
        sched_priority: priority,
    };
    if unsafe { (scheduling.set)(thread, ffi::SCHED_FIFO, &highest) } != 0 {
        stream.stats.record_portaudio_error(ffi::PA_INTERNAL_ERROR);
        return AUDIO_PORTAUDIO_ERROR;
    }
    // A stopped interval is not a scheduling delay on the next callback.
    stream
        .stats
        .previous_callback_start_ns
        .store(0, Ordering::Relaxed);
    stream
        .stats
        .previous_callback_period_ns
        .store(0, Ordering::Relaxed);
    let mut result = unsafe { (stream.functions.portaudio.start_stream)(stream.capture_stream) };
    if result == ffi::PA_NO_ERROR {
        stream.started[1].store(true, Ordering::Relaxed);
        result = unsafe { (stream.functions.portaudio.start_stream)(stream.portaudio_stream) };
        if result == ffi::PA_NO_ERROR {
            stream.started[0].store(true, Ordering::Relaxed);
        }
    }
    let restore = unsafe { (scheduling.set)(thread, policy, &saved) };
    if result != ffi::PA_NO_ERROR || restore != 0 {
        for (index, handle) in [stream.portaudio_stream, stream.capture_stream]
            .into_iter()
            .enumerate()
        {
            if !stream.started[index].load(Ordering::Relaxed)
                && unsafe { (stream.functions.portaudio.is_stream_active)(handle) } == 0
            {
                continue;
            }
            let abort = unsafe { (stream.functions.portaudio.abort_stream)(handle) };
            if abort != ffi::PA_NO_ERROR {
                stream.stats.record_portaudio_error(abort);
            } else {
                stream.started[index].store(false, Ordering::Relaxed);
            }
        }
        stream.stats.record_portaudio_error(if restore != 0 {
            ffi::PA_INTERNAL_ERROR
        } else {
            result
        });
        return AUDIO_PORTAUDIO_ERROR;
    }
    AUDIO_OK
}

extern "C" fn stream_stop(stream: *mut AudioStream) -> c_int {
    let Some(stream) = NonNull::new(stream) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    let stream = unsafe { stream.as_ref() };
    stream.stop_endpoints()
}

extern "C" fn stream_get_stats(stream: *const AudioStream, stats: *mut StreamStats) -> c_int {
    let Some(stream) = NonNull::new(stream.cast_mut()) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    let Some(stats) = NonNull::new(stats) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    // Read only the header until the caller's allocation size is known. Old
    // ABI-1 consumers own a smaller object, so never form a full-size reference.
    let supplied_size = unsafe { ptr::addr_of!((*stats.as_ptr()).struct_size).read() };
    if supplied_size != STREAM_STATS_V1_SIZE as u32
        && supplied_size != STREAM_STATS_TIMING_SIZE as u32
        && supplied_size < size_of::<StreamStats>() as u32
    {
        return AUDIO_INVALID_ARGUMENT;
    }
    let stream = unsafe { stream.as_ref() };
    let mut snapshot = StreamStats {
        struct_size: supplied_size,
        ..StreamStats::default()
    };
    stream.stats.snapshot(&mut snapshot);
    let Ok(_ring_guard) = stream.ring_control.lock() else {
        return AUDIO_PORTAUDIO_ERROR;
    };
    let ring = match stream.ring.snapshot() {
        Ok(ring) => ring,
        Err(error) => return error,
    };
    snapshot.input_queue_capacity_frames = ring.capacity_frames;
    snapshot.input_queue_occupancy_frames = ring.occupancy_frames;
    snapshot.capture_ring_target_frames = ring.target_frames;
    snapshot.capture_ring_ratio_correction_ppm = i64::from(ring.ratio_correction_ppm);
    snapshot.capture_ring_missing_frames = ring.shortfall_frames;
    snapshot.capture_ring_dropped_frames = ring.dropped_frames;
    snapshot.capture_startup_wait_frames = ring.startup_silence_frames;
    snapshot.device_error_count += ring.adapter_error_count;
    unsafe {
        ptr::copy_nonoverlapping(
            (&snapshot as *const StreamStats).cast::<u8>(),
            stats.as_ptr().cast::<u8>(),
            (supplied_size as usize).min(size_of::<StreamStats>()),
        );
    }
    AUDIO_OK
}

/// Copy PortAudio's immutable open-stream timing into the public ABI shape.
extern "C" fn stream_get_timing(stream: *const AudioStream, timing: *mut StreamTiming) -> c_int {
    let Some(stream) = NonNull::new(stream.cast_mut()) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    let Some(timing) = NonNull::new(timing) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    let timing = unsafe { timing.as_ptr().as_mut() }.expect("non-null checked above");
    let timing_size = timing.struct_size;
    *timing = StreamTiming {
        struct_size: timing_size,
        ..StreamTiming::default()
    };
    if timing_size < size_of::<StreamTiming>() as u32 {
        return AUDIO_INVALID_ARGUMENT;
    }

    let stream = unsafe { stream.as_ref() };
    let info = unsafe { (stream.functions.portaudio.get_stream_info)(stream.portaudio_stream) };
    let capture_info =
        unsafe { (stream.functions.portaudio.get_stream_info)(stream.capture_stream) };
    let Some(info) = (unsafe { info.as_ref() }) else {
        stream.stats.record_portaudio_error(ffi::PA_INTERNAL_ERROR);
        return AUDIO_PORTAUDIO_ERROR;
    };
    let Some(capture_info) = (unsafe { capture_info.as_ref() }) else {
        stream.stats.record_portaudio_error(ffi::PA_INTERNAL_ERROR);
        return AUDIO_PORTAUDIO_ERROR;
    };
    // PortAudio 19.6 ALSA leaves this field zero despite returning valid timing.
    if info.struct_version < 0
        || capture_info.struct_version < 0
        || !capture_info.input_latency.is_finite()
        || !info.output_latency.is_finite()
        || !info.sample_rate.is_finite()
        || capture_info.input_latency < 0.0
        || !capture_info.sample_rate.is_finite()
        || capture_info.sample_rate <= 0.0
        || capture_info.sample_rate != info.sample_rate
        || info.output_latency < 0.0
    {
        stream.stats.record_portaudio_error(ffi::PA_INTERNAL_ERROR);
        return AUDIO_PORTAUDIO_ERROR;
    }

    timing.abi_version = ABI_VERSION;
    timing.input_latency_seconds = capture_info.input_latency;
    timing.output_latency_seconds = info.output_latency;
    timing.sample_rate_hz = info.sample_rate;
    AUDIO_OK
}

extern "C" fn stream_destroy(stream: *mut AudioStream) {
    let Some(stream) = NonNull::new(stream) else {
        return;
    };
    let mut stream = unsafe { Box::from_raw(stream.as_ptr()) };
    let functions = stream.functions;
    stream.stop_endpoints();
    let mut closed = true;
    for handle in [&mut stream.portaudio_stream, &mut stream.capture_stream] {
        if handle.is_null() {
            continue;
        }
        let result = unsafe { (functions.portaudio.close_stream)(*handle) };
        if result != ffi::PA_NO_ERROR {
            stream.stats.record_portaudio_error(result);
            closed = false;
        } else {
            *handle = ptr::null_mut();
        }
    }
    if !closed {
        // A failed native close cannot prove callback quiescence. Retain the
        // contexts, reservation and runtime rather than freeing live userdata.
        let _ = Box::into_raw(stream);
        return;
    }
    stream.release_device_lease();
    drop(stream);
    portaudio_release(functions);
}

#[derive(Clone, Copy)]
enum MixerDirection {
    Capture,
    Playback,
}

impl MixerDirection {
    fn from_ffi(value: u32) -> Result<Self, c_int> {
        match value {
            0 => Ok(Self::Capture),
            1 => Ok(Self::Playback),
            _ => Err(AUDIO_INVALID_ARGUMENT),
        }
    }

    fn as_ffi(self) -> u32 {
        match self {
            Self::Capture => 0,
            Self::Playback => 1,
        }
    }
}

/// Adapter-owned hardware mixer channel; ALSA channel constants stay private.
#[derive(Clone, Copy)]
enum MixerChannel {
    Left,
    Right,
}

impl MixerChannel {
    fn from_ffi(value: u32) -> Result<Self, c_int> {
        match value {
            0 => Ok(Self::Left),
            1 => Ok(Self::Right),
            _ => Err(AUDIO_INVALID_ARGUMENT),
        }
    }

    fn as_alsa_channel(self) -> c_int {
        match self {
            Self::Left => 0,
            Self::Right => 1,
        }
    }

    fn as_ffi(self) -> u32 {
        match self {
            Self::Left => 0,
            Self::Right => 1,
        }
    }
}

#[repr(C)]
struct AudioMixer {
    functions: &'static ffi::FunctionTable,
    mixer: *mut SndMixer,
    element: *mut SndMixerElem,
    channel: MixerChannel,
    direction: MixerDirection,
}

impl AudioMixer {
    /// Refresh this handle's cached controls before control-plane reads or writes.
    ///
    /// ALSA writes a complete stereo control, so a stale sibling value would
    /// overwrite changes made through another handle or external mixer client.
    fn refresh(&self) -> Result<(), c_int> {
        let result = unsafe { (self.functions.alsa.mixer_handle_events)(self.mixer) };
        if result < 0 {
            return Err(AUDIO_ALSA_ERROR);
        }
        Ok(())
    }

    fn volume_supported(&self) -> bool {
        unsafe {
            match self.direction {
                MixerDirection::Capture => {
                    (self.functions.alsa.selem_has_capture_volume)(self.element) != 0
                }
                MixerDirection::Playback => {
                    (self.functions.alsa.selem_has_playback_volume)(self.element) != 0
                }
            }
        }
    }

    /// Report whether this element exposes the selected capture/playback path switch.
    fn switch_supported(&self) -> bool {
        unsafe {
            match self.direction {
                MixerDirection::Capture => {
                    (self.functions.alsa.selem_has_capture_switch)(self.element) != 0
                }
                MixerDirection::Playback => {
                    (self.functions.alsa.selem_has_playback_switch)(self.element) != 0
                }
            }
        }
    }

    fn get_range(&self) -> Result<(i64, i64), c_int> {
        self.refresh()?;
        if !self.volume_supported() {
            return Err(AUDIO_UNSUPPORTED);
        }
        let mut minimum = 0;
        let mut maximum = 0;
        let result = unsafe {
            match self.direction {
                MixerDirection::Capture => (self.functions.alsa.selem_get_capture_db_range)(
                    self.element,
                    &mut minimum,
                    &mut maximum,
                ),
                MixerDirection::Playback => (self.functions.alsa.selem_get_playback_db_range)(
                    self.element,
                    &mut minimum,
                    &mut maximum,
                ),
            }
        };
        if result < 0 {
            return Err(AUDIO_ALSA_ERROR);
        }
        Ok((minimum, maximum))
    }

    fn get_centibels(&self) -> Result<i64, c_int> {
        self.refresh()?;
        if !self.volume_supported() {
            return Err(AUDIO_UNSUPPORTED);
        }
        let mut value = 0;
        let result = unsafe {
            match self.direction {
                MixerDirection::Capture => (self.functions.alsa.selem_get_capture_db)(
                    self.element,
                    self.channel.as_alsa_channel(),
                    &mut value,
                ),
                MixerDirection::Playback => (self.functions.alsa.selem_get_playback_db)(
                    self.element,
                    self.channel.as_alsa_channel(),
                    &mut value,
                ),
            }
        };
        if result < 0 {
            return Err(AUDIO_ALSA_ERROR);
        }
        Ok(value)
    }

    fn set_centibels(&mut self, value: i64) -> Result<(), c_int> {
        self.refresh()?;
        if !self.volume_supported() {
            return Err(AUDIO_UNSUPPORTED);
        }
        let result = unsafe {
            match self.direction {
                MixerDirection::Capture => (self.functions.alsa.selem_set_capture_db)(
                    self.element,
                    self.channel.as_alsa_channel(),
                    value,
                    0,
                ),
                MixerDirection::Playback => (self.functions.alsa.selem_set_playback_db)(
                    self.element,
                    self.channel.as_alsa_channel(),
                    value,
                    0,
                ),
            }
        };
        if result < 0 {
            return Err(AUDIO_ALSA_ERROR);
        }
        Ok(())
    }

    /// Return ALSA's native integer step range for compatibility bridges.
    fn get_step_range(&self) -> Result<(i64, i64), c_int> {
        self.refresh()?;
        if !self.volume_supported() {
            return Err(AUDIO_UNSUPPORTED);
        }
        let mut minimum = 0;
        let mut maximum = 0;
        unsafe {
            match self.direction {
                MixerDirection::Capture => (self.functions.alsa.selem_get_capture_volume_range)(
                    self.element,
                    &mut minimum,
                    &mut maximum,
                ),
                MixerDirection::Playback => (self.functions.alsa.selem_get_playback_volume_range)(
                    self.element,
                    &mut minimum,
                    &mut maximum,
                ),
            }
        }
        if minimum > maximum {
            return Err(AUDIO_ALSA_ERROR);
        }
        Ok((minimum, maximum))
    }

    /// Read one selected ALSA mixer channel in native integer steps.
    fn get_steps(&self) -> Result<i64, c_int> {
        self.refresh()?;
        if !self.volume_supported() {
            return Err(AUDIO_UNSUPPORTED);
        }
        let mut value = 0;
        let result = unsafe {
            match self.direction {
                MixerDirection::Capture => (self.functions.alsa.selem_get_capture_volume)(
                    self.element,
                    self.channel.as_alsa_channel(),
                    &mut value,
                ),
                MixerDirection::Playback => (self.functions.alsa.selem_get_playback_volume)(
                    self.element,
                    self.channel.as_alsa_channel(),
                    &mut value,
                ),
            }
        };
        if result < 0 {
            return Err(AUDIO_ALSA_ERROR);
        }
        Ok(value)
    }

    /// Set one selected ALSA mixer channel in native integer steps.
    fn set_steps(&mut self, value: i64) -> Result<(), c_int> {
        // The range read refreshes all cached channels before this full-control write.
        let (minimum, maximum) = self.get_step_range()?;
        if value < minimum || value > maximum {
            return Err(AUDIO_INVALID_ARGUMENT);
        }
        let result = unsafe {
            match self.direction {
                MixerDirection::Capture => (self.functions.alsa.selem_set_capture_volume)(
                    self.element,
                    self.channel.as_alsa_channel(),
                    value,
                ),
                MixerDirection::Playback => (self.functions.alsa.selem_set_playback_volume)(
                    self.element,
                    self.channel.as_alsa_channel(),
                    value,
                ),
            }
        };
        if result < 0 {
            return Err(AUDIO_ALSA_ERROR);
        }
        Ok(())
    }

    /// Read the selected capture/playback path switch.
    fn get_switch(&self) -> Result<bool, c_int> {
        self.refresh()?;
        if !self.switch_supported() {
            return Err(AUDIO_UNSUPPORTED);
        }
        let mut enabled = 0;
        let result = unsafe {
            match self.direction {
                MixerDirection::Capture => (self.functions.alsa.selem_get_capture_switch)(
                    self.element,
                    self.channel.as_alsa_channel(),
                    &mut enabled,
                ),
                MixerDirection::Playback => (self.functions.alsa.selem_get_playback_switch)(
                    self.element,
                    self.channel.as_alsa_channel(),
                    &mut enabled,
                ),
            }
        };
        if result < 0 {
            return Err(AUDIO_ALSA_ERROR);
        }
        Ok(enabled != 0)
    }

    /// Set the selected capture/playback path switch.
    fn set_switch(&mut self, enabled: bool) -> Result<(), c_int> {
        self.refresh()?;
        if !self.switch_supported() {
            return Err(AUDIO_UNSUPPORTED);
        }
        let result = unsafe {
            match self.direction {
                MixerDirection::Capture => (self.functions.alsa.selem_set_capture_switch)(
                    self.element,
                    self.channel.as_alsa_channel(),
                    i32::from(enabled),
                ),
                MixerDirection::Playback => (self.functions.alsa.selem_set_playback_switch)(
                    self.element,
                    self.channel.as_alsa_channel(),
                    i32::from(enabled),
                ),
            }
        };
        if result < 0 {
            return Err(AUDIO_ALSA_ERROR);
        }
        Ok(())
    }
}

impl Drop for AudioMixer {
    fn drop(&mut self) {
        if !self.mixer.is_null() {
            let _ = unsafe { (self.functions.alsa.mixer_close)(self.mixer) };
        }
    }
}

unsafe fn mixer_config_strings(config: &MixerConfig) -> Result<(&CStr, &CStr), c_int> {
    if config.card.is_null() || config.element.is_null() {
        return Err(AUDIO_INVALID_ARGUMENT);
    }
    let card = unsafe { CStr::from_ptr(config.card) };
    let element = unsafe { CStr::from_ptr(config.element) };
    if card.is_empty() || element.is_empty() {
        return Err(AUDIO_INVALID_ARGUMENT);
    }
    Ok((card, element))
}

unsafe fn usb_mixer_config_strings(config: &UsbMixerConfig) -> Result<(&str, &CStr), c_int> {
    if config.usb_interface_path.is_null() || config.element.is_null() {
        return Err(AUDIO_INVALID_ARGUMENT);
    }
    let usb_interface_path = unsafe { CStr::from_ptr(config.usb_interface_path) };
    let element = unsafe { CStr::from_ptr(config.element) };
    let Ok(path) = usb_interface_path.to_str() else {
        return Err(AUDIO_INVALID_ARGUMENT);
    };
    if !valid_usb_interface_path(path) || element.is_empty() {
        return Err(AUDIO_INVALID_ARGUMENT);
    }
    Ok((path, element))
}

fn valid_usb_interface_path(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\'])
        && !value.chars().any(char::is_control)
}

fn valid_usb_serial(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(char::is_control)
}

/// Resolve a stable USB topology and/or serial number to exactly one ALSA card.
///
/// ALSA card indices are assigned at discovery time. This resolver therefore
/// compares immutable sysfs ancestry and rejects missing or ambiguous matches
/// rather than selecting an arbitrary transient card number.
fn resolve_alsa_card_index_from_sysfs_identity(
    sound_class_root: &Path,
    usb_interface_path: Option<&str>,
    usb_serial: Option<&str>,
) -> Result<u32, c_int> {
    if usb_interface_path.is_none() && usb_serial.is_none() {
        return Err(AUDIO_INVALID_ARGUMENT);
    }
    let entries = fs::read_dir(sound_class_root).map_err(|_| AUDIO_UNSUPPORTED)?;
    let mut result = None;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Some(index) = name
            .strip_prefix("card")
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(device_path) = fs::canonicalize(entry.path().join("device")) else {
            continue;
        };
        if usb_interface_path.is_some_and(|expected_path| {
            !device_path
                .components()
                .any(|component| component.as_os_str().to_str() == Some(expected_path))
        }) {
            continue;
        }
        if usb_serial.is_some_and(|expected_serial| {
            usb_serial_from_device_path(&device_path).as_deref() != Some(expected_serial)
        }) {
            continue;
        }
        if result.replace(index).is_some() {
            return Err(AUDIO_UNSUPPORTED);
        }
    }
    result.ok_or(AUDIO_UNSUPPORTED)
}

/// Resolve a Linux USB-interface component to exactly one ALSA card number.
fn resolve_alsa_card_index_from_sysfs(
    sound_class_root: &Path,
    usb_interface_path: &str,
) -> Result<u32, c_int> {
    resolve_alsa_card_index_from_sysfs_identity(sound_class_root, Some(usb_interface_path), None)
}

struct ValidatedUsbDeviceIdentity<'a> {
    usb_interface_path: Option<&'a str>,
    usb_serial: Option<&'a str>,
    input_channels: usize,
    output_channels: usize,
}

unsafe fn optional_utf8_c_string<'a>(value: *const c_char) -> Result<Option<&'a str>, c_int> {
    if value.is_null() {
        return Ok(None);
    }
    let value = unsafe { CStr::from_ptr(value) }
        .to_str()
        .map_err(|_| AUDIO_INVALID_ARGUMENT)?;
    Ok(Some(value))
}

impl ValidatedUsbDeviceIdentity<'_> {
    unsafe fn from_ffi(identity: *const UsbDeviceIdentity) -> Result<Self, c_int> {
        if identity.is_null() {
            return Err(AUDIO_INVALID_ARGUMENT);
        }
        let identity = unsafe { &*identity };
        if identity.struct_size < size_of::<UsbDeviceIdentity>() as u32
            || !matches!(identity.input_device_channels, 1 | 2)
            || !matches!(identity.output_device_channels, 1 | 2)
        {
            return Err(AUDIO_INVALID_ARGUMENT);
        }
        let usb_interface_path = unsafe { optional_utf8_c_string(identity.usb_interface_path) }?;
        let usb_serial = unsafe { optional_utf8_c_string(identity.usb_serial) }?;
        if usb_interface_path.is_none() && usb_serial.is_none()
            || usb_interface_path.is_some_and(|value| !valid_usb_interface_path(value))
            || usb_serial.is_some_and(|value| !valid_usb_serial(value))
        {
            return Err(AUDIO_INVALID_ARGUMENT);
        }
        Ok(Self {
            usb_interface_path,
            usb_serial,
            input_channels: identity.input_device_channels as usize,
            output_channels: identity.output_device_channels as usize,
        })
    }
}

/// Resolve stable USB identity to exact raw ALSA PortAudio device indexes.
fn usb_device_resolve_with_functions_and_root(
    functions: &'static ffi::FunctionTable,
    sound_class_root: &Path,
    identity: *const UsbDeviceIdentity,
    selection: *mut UsbDeviceSelection,
) -> c_int {
    let Some(selection) = NonNull::new(selection) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    let selection = unsafe { selection.as_ptr().as_mut() }.expect("non-null checked above");
    let selection_size = selection.struct_size;
    *selection = UsbDeviceSelection {
        struct_size: selection_size,
        ..UsbDeviceSelection::default()
    };
    if selection_size < size_of::<UsbDeviceSelection>() as u32 {
        return AUDIO_INVALID_ARGUMENT;
    }
    let identity = match unsafe { ValidatedUsbDeviceIdentity::from_ffi(identity) } {
        Ok(identity) => identity,
        Err(error) => return error,
    };
    let card_index = match resolve_alsa_card_index_from_sysfs_identity(
        sound_class_root,
        identity.usb_interface_path,
        identity.usb_serial,
    ) {
        Ok(card_index) => card_index,
        Err(error) => return error,
    };
    if let Err(error) = portaudio_acquire(functions) {
        return error;
    }
    let resolved = (|| {
        let input_device_index = resolve_portaudio_device_for_alsa_card(
            functions,
            card_index,
            identity.input_channels,
            true,
        )?;
        let output_device_index = resolve_portaudio_device_for_alsa_card(
            functions,
            card_index,
            identity.output_channels,
            false,
        )?;
        Ok::<_, c_int>((input_device_index, output_device_index))
    })();
    portaudio_release(functions);
    let (input_device_index, output_device_index) = match resolved {
        Ok(resolved) => resolved,
        Err(error) => return error,
    };
    selection.abi_version = ABI_VERSION;
    selection.alsa_card_index = card_index;
    selection.input_device_index = input_device_index;
    selection.output_device_index = output_device_index;
    AUDIO_OK
}

extern "C" fn usb_device_resolve(
    identity: *const UsbDeviceIdentity,
    selection: *mut UsbDeviceSelection,
) -> c_int {
    usb_device_resolve_with_functions_and_root(
        &ffi::PRODUCTION_FUNCTIONS,
        Path::new("/sys/class/sound"),
        identity,
        selection,
    )
}

/// One USB-backed ALSA card whose stable identity can be returned to a caller.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedUsbCard {
    card_index: u32,
    usb_interface_path: String,
    usb_serial: Option<String>,
}

/// Return whether a sysfs path component is a USB interface topology component.
fn looks_like_usb_interface_component(value: &str) -> bool {
    let Some((topology, interface)) = value.rsplit_once(':') else {
        return false;
    };
    !topology.is_empty()
        && topology.contains('-')
        && topology
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'-'))
        && !interface.is_empty()
        && interface
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
}

/// Locate the nearest USB interface in a canonical card path.
fn usb_interface_from_device_path(device_path: &Path) -> Option<&Path> {
    device_path.ancestors().find(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(looks_like_usb_interface_component)
    })
}

/// Extract the nearest USB-interface component from a canonical card path.
fn usb_interface_path_from_device_path(device_path: &Path) -> Option<String> {
    usb_interface_from_device_path(device_path)?
        .file_name()?
        .to_str()
        .map(str::to_owned)
}

/// Read only the serial of the physical device owning the nearest USB interface.
///
/// A device without a serial must not inherit one from its upstream hub or host.
fn usb_serial_from_device_path(device_path: &Path) -> Option<String> {
    let usb_device = usb_interface_from_device_path(device_path)?.parent()?;
    let serial = fs::read_to_string(usb_device.join("serial")).ok()?;
    let serial = serial.trim_end_matches(['\n', '\r']);
    valid_usb_serial(serial).then(|| serial.to_owned())
}

/// Enumerate physical USB sound cards in stable numerical ALSA-card order.
fn enumerate_usb_cards_from_sysfs(sound_class_root: &Path) -> Result<Vec<ResolvedUsbCard>, c_int> {
    let entries = fs::read_dir(sound_class_root).map_err(|_| AUDIO_UNSUPPORTED)?;
    let mut cards = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Some(card_index) = name
            .strip_prefix("card")
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(device_path) = fs::canonicalize(entry.path().join("device")) else {
            continue;
        };
        let Some(usb_interface_path) = usb_interface_path_from_device_path(&device_path) else {
            continue;
        };
        cards.push(ResolvedUsbCard {
            card_index,
            usb_interface_path,
            usb_serial: usb_serial_from_device_path(&device_path),
        });
    }
    cards.sort_unstable_by_key(|card| card.card_index);
    Ok(cards)
}

/// Resolve a known ALSA card to its exported stable USB identity.
fn resolve_usb_card_from_alsa_card(
    sound_class_root: &Path,
    card_index: u32,
) -> Result<ResolvedUsbCard, c_int> {
    enumerate_usb_cards_from_sysfs(sound_class_root)?
        .into_iter()
        .find(|card| card.card_index == card_index)
        .ok_or(AUDIO_UNSUPPORTED)
}

/// A parsed native `hw:<card>` or `hw:<card>,<pcm>` selector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LegacyHwSelector {
    card_index: u32,
    pcm_device: Option<u32>,
}

/// Parse the exact native ALSA spelling accepted for compatibility selection.
fn parse_legacy_hw_selector(value: &str) -> Result<LegacyHwSelector, c_int> {
    let Some(value) = value.strip_prefix("hw:") else {
        return Err(AUDIO_INVALID_ARGUMENT);
    };
    let Some((card_index, remainder)) = parse_decimal_prefix(value) else {
        return Err(AUDIO_INVALID_ARGUMENT);
    };
    if remainder.is_empty() {
        return Ok(LegacyHwSelector {
            card_index,
            pcm_device: None,
        });
    }
    let Some(remainder) = remainder.strip_prefix(',') else {
        return Err(AUDIO_INVALID_ARGUMENT);
    };
    let Some((pcm_device, remainder)) = parse_decimal_prefix(remainder) else {
        return Err(AUDIO_INVALID_ARGUMENT);
    };
    if !remainder.is_empty() {
        return Err(AUDIO_INVALID_ARGUMENT);
    }
    Ok(LegacyHwSelector {
        card_index,
        pcm_device: Some(pcm_device),
    })
}

/// Identifier forms accepted by the legacy-compatible selection entry point.
enum UsbDeviceIdentifier<'a> {
    UsbTopology(&'a str),
    LegacyHw(LegacyHwSelector),
}

/// Validate one selection request before reading host inventory.
struct ValidatedUsbDeviceSelector<'a> {
    selection_policy: u32,
    identifier: Option<UsbDeviceIdentifier<'a>>,
    usb_serial: Option<&'a str>,
    input_channels: usize,
    output_channels: usize,
}

impl ValidatedUsbDeviceSelector<'_> {
    unsafe fn from_ffi(selector: *const UsbDeviceSelector) -> Result<Self, c_int> {
        if selector.is_null() {
            return Err(AUDIO_INVALID_ARGUMENT);
        }
        let selector = unsafe { &*selector };
        if selector.struct_size < size_of::<UsbDeviceSelector>() as u32
            || !matches!(
                selector.selection_policy,
                USB_SELECTION_EXACT | USB_SELECTION_AUTOMATIC_LOWEST_ALSA_CARD
            )
            || !matches!(selector.input_device_channels, 1 | 2)
            || !matches!(selector.output_device_channels, 1 | 2)
        {
            return Err(AUDIO_INVALID_ARGUMENT);
        }
        let identifier = unsafe { optional_utf8_c_string(selector.device_identifier) }?;
        let identifier = match identifier {
            Some(value) if value.starts_with("hw:") => Some(UsbDeviceIdentifier::LegacyHw(
                parse_legacy_hw_selector(value)?,
            )),
            Some(value) if valid_usb_interface_path(value) => {
                Some(UsbDeviceIdentifier::UsbTopology(value))
            }
            Some(_) => return Err(AUDIO_INVALID_ARGUMENT),
            None => None,
        };
        let usb_serial = unsafe { optional_utf8_c_string(selector.usb_serial) }?;
        if usb_serial.is_some_and(|value| !valid_usb_serial(value)) {
            return Err(AUDIO_INVALID_ARGUMENT);
        }
        match selector.selection_policy {
            USB_SELECTION_EXACT if identifier.is_none() && usb_serial.is_none() => {
                return Err(AUDIO_INVALID_ARGUMENT);
            }
            USB_SELECTION_AUTOMATIC_LOWEST_ALSA_CARD
                if identifier.is_some() || usb_serial.is_some() =>
            {
                return Err(AUDIO_INVALID_ARGUMENT);
            }
            _ => {}
        }
        Ok(Self {
            selection_policy: selector.selection_policy,
            identifier,
            usb_serial,
            input_channels: selector.input_device_channels as usize,
            output_channels: selector.output_device_channels as usize,
        })
    }
}

/// Resolve an exact selector to a card and optional requested PCM device.
fn resolve_exact_usb_card(
    sound_class_root: &Path,
    selector: &ValidatedUsbDeviceSelector<'_>,
) -> Result<(ResolvedUsbCard, Option<u32>), c_int> {
    let (card_index, pcm_device) = match selector.identifier.as_ref() {
        Some(UsbDeviceIdentifier::UsbTopology(topology)) => (
            resolve_alsa_card_index_from_sysfs_identity(
                sound_class_root,
                Some(topology),
                selector.usb_serial,
            )?,
            None,
        ),
        Some(UsbDeviceIdentifier::LegacyHw(selector)) => (selector.card_index, selector.pcm_device),
        None => (
            resolve_alsa_card_index_from_sysfs_identity(
                sound_class_root,
                None,
                selector.usb_serial,
            )?,
            None,
        ),
    };
    let card = resolve_usb_card_from_alsa_card(sound_class_root, card_index)?;
    if selector
        .usb_serial
        .is_some_and(|serial| card.usb_serial.as_deref() != Some(serial))
    {
        return Err(AUDIO_UNSUPPORTED);
    }
    Ok((card, pcm_device))
}

/// Resolve both PortAudio directions for one selected USB card.
fn resolve_selected_portaudio_devices(
    functions: &'static ffi::FunctionTable,
    card: &ResolvedUsbCard,
    pcm_device: Option<u32>,
    selector: &ValidatedUsbDeviceSelector<'_>,
) -> Result<(PaDeviceIndex, PaDeviceIndex), c_int> {
    let input_device_index = resolve_portaudio_device_for_alsa_endpoint(
        functions,
        card.card_index,
        pcm_device,
        selector.input_channels,
        true,
    )?;
    let output_device_index = resolve_portaudio_device_for_alsa_endpoint(
        functions,
        card.card_index,
        pcm_device,
        selector.output_channels,
        false,
    )?;
    Ok((input_device_index, output_device_index))
}

/// Copy one UTF-8 host identity into an ABI-owned NUL-terminated buffer.
fn copy_usb_identity_component(destination: &mut [c_char], value: &str) -> Result<(), c_int> {
    if value.len() >= destination.len() {
        return Err(AUDIO_UNSUPPORTED);
    }
    destination.fill(0);
    for (destination, source) in destination.iter_mut().zip(value.bytes()) {
        *destination = source as c_char;
    }
    Ok(())
}

/// Resolve a legacy-compatible selector to stable identity and stream indexes.
fn usb_device_select_with_functions_and_root(
    functions: &'static ffi::FunctionTable,
    sound_class_root: &Path,
    selector: *const UsbDeviceSelector,
    device_match: *mut UsbDeviceMatch,
) -> c_int {
    let Some(device_match) = NonNull::new(device_match) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    let device_match = unsafe { device_match.as_ptr().as_mut() }.expect("non-null checked above");
    let match_size = device_match.struct_size;
    *device_match = UsbDeviceMatch {
        struct_size: match_size,
        ..UsbDeviceMatch::default()
    };
    if match_size < size_of::<UsbDeviceMatch>() as u32 {
        return AUDIO_INVALID_ARGUMENT;
    }
    let selector = match unsafe { ValidatedUsbDeviceSelector::from_ffi(selector) } {
        Ok(selector) => selector,
        Err(error) => return error,
    };
    if let Err(error) = portaudio_acquire(functions) {
        return error;
    }
    let resolved = (|| {
        if selector.selection_policy == USB_SELECTION_AUTOMATIC_LOWEST_ALSA_CARD {
            for card in enumerate_usb_cards_from_sysfs(sound_class_root)? {
                match resolve_selected_portaudio_devices(functions, &card, None, &selector) {
                    Ok(indexes) => return Ok((card, indexes)),
                    Err(AUDIO_UNSUPPORTED) => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(AUDIO_UNSUPPORTED)
        } else {
            let (card, pcm_device) = resolve_exact_usb_card(sound_class_root, &selector)?;
            let indexes =
                resolve_selected_portaudio_devices(functions, &card, pcm_device, &selector)?;
            Ok((card, indexes))
        }
    })();
    portaudio_release(functions);
    let (card, (input_device_index, output_device_index)) = match resolved {
        Ok(resolved) => resolved,
        Err(error) => return error,
    };
    // Build the full identity privately before publishing it.  In particular,
    // a serial that does not fit the fixed ABI buffer must not leave a caller
    // with a usable-looking interface path paired with failed selection.
    let mut resolved_match = UsbDeviceMatch {
        struct_size: match_size,
        ..UsbDeviceMatch::default()
    };
    // A returned interface path is exactly one Linux sysfs component. Its
    // capacity is bounded by NAME_MAX (255), while the ABI reserves 256 bytes.
    copy_usb_identity_component(
        &mut resolved_match.usb_interface_path,
        &card.usb_interface_path,
    )
    .expect("USB interface path fits the ABI identity buffer");
    if let Some(serial) = card.usb_serial.as_deref() {
        if let Err(error) = copy_usb_identity_component(&mut resolved_match.usb_serial, serial) {
            return error;
        }
    }
    resolved_match.abi_version = ABI_VERSION;
    resolved_match.selection = UsbDeviceSelection {
        struct_size: size_of::<UsbDeviceSelection>() as u32,
        abi_version: ABI_VERSION,
        alsa_card_index: card.card_index,
        input_device_index,
        output_device_index,
    };
    *device_match = resolved_match;
    AUDIO_OK
}

extern "C" fn usb_device_select(
    selector: *const UsbDeviceSelector,
    device_match: *mut UsbDeviceMatch,
) -> c_int {
    usb_device_select_with_functions_and_root(
        &ffi::PRODUCTION_FUNCTIONS,
        Path::new("/sys/class/sound"),
        selector,
        device_match,
    )
}

fn mixer_create_from_values_with_functions(
    functions: &'static ffi::FunctionTable,
    card: &CStr,
    element_name: &CStr,
    element_index: u32,
    channel: MixerChannel,
    direction: MixerDirection,
    mixer: *mut *mut AudioMixer,
) -> c_int {
    let mut alsa_mixer = ptr::null_mut();
    if unsafe { (functions.alsa.mixer_open)(&mut alsa_mixer, 0) } < 0 {
        return AUDIO_ALSA_ERROR;
    }
    if unsafe { (functions.alsa.mixer_attach)(alsa_mixer, card.as_ptr()) } < 0
        || unsafe {
            (functions.alsa.mixer_selem_register)(alsa_mixer, ptr::null_mut(), ptr::null_mut())
        } < 0
        || unsafe { (functions.alsa.mixer_load)(alsa_mixer) } < 0
    {
        let _ = unsafe { (functions.alsa.mixer_close)(alsa_mixer) };
        return AUDIO_ALSA_ERROR;
    }
    let mut element_id = ptr::null_mut();
    if unsafe { (functions.alsa.selem_id_malloc)(&mut element_id) } < 0 {
        let _ = unsafe { (functions.alsa.mixer_close)(alsa_mixer) };
        return AUDIO_NO_MEMORY;
    }
    unsafe {
        (functions.alsa.selem_id_set_name)(element_id, element_name.as_ptr());
        (functions.alsa.selem_id_set_index)(element_id, element_index);
    }
    let element = unsafe { (functions.alsa.mixer_find_selem)(alsa_mixer, element_id) };
    unsafe {
        (functions.alsa.selem_id_free)(element_id);
    }
    if element.is_null() {
        let _ = unsafe { (functions.alsa.mixer_close)(alsa_mixer) };
        return AUDIO_UNSUPPORTED;
    }
    let audio_mixer = AudioMixer {
        functions,
        mixer: alsa_mixer,
        element,
        channel,
        direction,
    };
    if !audio_mixer.volume_supported() && !audio_mixer.switch_supported() {
        drop(audio_mixer);
        return AUDIO_UNSUPPORTED;
    }
    unsafe {
        *mixer = Box::into_raw(Box::new(audio_mixer));
    }
    AUDIO_OK
}

fn mixer_create_with_functions(
    functions: &'static ffi::FunctionTable,
    config: *const MixerConfig,
    mixer: *mut *mut AudioMixer,
) -> c_int {
    if config.is_null() || mixer.is_null() {
        return AUDIO_INVALID_ARGUMENT;
    }
    unsafe {
        *mixer = ptr::null_mut();
    }
    let config = unsafe { &*config };
    if config.struct_size < size_of::<MixerConfig>() as u32 {
        return AUDIO_INVALID_ARGUMENT;
    }
    let direction = match MixerDirection::from_ffi(config.direction) {
        Ok(direction) => direction,
        Err(error) => return error,
    };
    let channel = match MixerChannel::from_ffi(config.channel) {
        Ok(channel) => channel,
        Err(error) => return error,
    };
    let (card, element_name) = match unsafe { mixer_config_strings(config) } {
        Ok(strings) => strings,
        Err(error) => return error,
    };
    mixer_create_from_values_with_functions(
        functions,
        card,
        element_name,
        config.element_index,
        channel,
        direction,
        mixer,
    )
}

extern "C" fn mixer_create(config: *const MixerConfig, mixer: *mut *mut AudioMixer) -> c_int {
    mixer_create_with_functions(&ffi::PRODUCTION_FUNCTIONS, config, mixer)
}

extern "C" fn mixer_get_range_centibels(
    mixer: *const AudioMixer,
    minimum: *mut i64,
    maximum: *mut i64,
) -> c_int {
    let Some(mixer) = NonNull::new(mixer.cast_mut()) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    if minimum.is_null() || maximum.is_null() {
        return AUDIO_INVALID_ARGUMENT;
    }
    let mixer = unsafe { mixer.as_ref() };
    let (low, high) = match mixer.get_range() {
        Ok(range) => range,
        Err(error) => return error,
    };
    unsafe {
        *minimum = low;
        *maximum = high;
    }
    AUDIO_OK
}

extern "C" fn mixer_get_centibels(mixer: *const AudioMixer, value: *mut i64) -> c_int {
    let Some(mixer) = NonNull::new(mixer.cast_mut()) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    if value.is_null() {
        return AUDIO_INVALID_ARGUMENT;
    }
    let mixer = unsafe { mixer.as_ref() };
    let value_from_mixer = match mixer.get_centibels() {
        Ok(value_from_mixer) => value_from_mixer,
        Err(error) => return error,
    };
    unsafe {
        *value = value_from_mixer;
    }
    AUDIO_OK
}

extern "C" fn mixer_set_centibels(mixer: *mut AudioMixer, value: i64) -> c_int {
    let Some(mut mixer) = NonNull::new(mixer) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    match unsafe { mixer.as_mut() }.set_centibels(value) {
        Ok(()) => AUDIO_OK,
        Err(error) => error,
    }
}

extern "C" fn mixer_destroy(mixer: *mut AudioMixer) {
    if let Some(mixer) = NonNull::new(mixer) {
        unsafe {
            drop(Box::from_raw(mixer.as_ptr()));
        }
    }
}

fn mixer_create_for_usb_interface_with_functions_and_root(
    functions: &'static ffi::FunctionTable,
    sound_class_root: &Path,
    config: *const UsbMixerConfig,
    mixer: *mut *mut AudioMixer,
) -> c_int {
    if config.is_null() || mixer.is_null() {
        return AUDIO_INVALID_ARGUMENT;
    }
    unsafe {
        *mixer = ptr::null_mut();
    }
    let config = unsafe { &*config };
    if config.struct_size < size_of::<UsbMixerConfig>() as u32 {
        return AUDIO_INVALID_ARGUMENT;
    }
    let direction = match MixerDirection::from_ffi(config.direction) {
        Ok(direction) => direction,
        Err(error) => return error,
    };
    let channel = match MixerChannel::from_ffi(config.channel) {
        Ok(channel) => channel,
        Err(error) => return error,
    };
    let (usb_interface_path, element_name) = match unsafe { usb_mixer_config_strings(config) } {
        Ok(strings) => strings,
        Err(error) => return error,
    };
    let card_index = match resolve_alsa_card_index_from_sysfs(sound_class_root, usb_interface_path)
    {
        Ok(card_index) => card_index,
        Err(error) => return error,
    };
    let card_name = format!("hw:{card_index}");
    // SAFETY: `hw:` and a formatted unsigned decimal card number contain no
    // interior NUL byte. `from_vec_unchecked` appends the required terminator.
    let card = unsafe { CString::from_vec_unchecked(card_name.into_bytes()) };
    mixer_create_from_values_with_functions(
        functions,
        &card,
        element_name,
        config.element_index,
        channel,
        direction,
        mixer,
    )
}

extern "C" fn mixer_create_for_usb_interface(
    config: *const UsbMixerConfig,
    mixer: *mut *mut AudioMixer,
) -> c_int {
    mixer_create_for_usb_interface_with_functions_and_root(
        &ffi::PRODUCTION_FUNCTIONS,
        Path::new("/sys/class/sound"),
        config,
        mixer,
    )
}

/// Borrowed ALSA values that describe one copied public mixer path.
struct Cm119MixerPathSource<'a> {
    name: &'a CStr,
    element_index: u32,
    channel: MixerChannel,
    direction: MixerDirection,
    volume_supported: bool,
    switch_supported: bool,
}

/// Append one discovered simple-mixer path without publishing borrowed ALSA data.
fn append_cm119_mixer_path(
    paths: &mut [Cm119MixerPath],
    count: &mut u32,
    source: Cm119MixerPathSource<'_>,
) -> Result<(), c_int> {
    if !source.volume_supported && !source.switch_supported {
        return Ok(());
    }
    let index = usize::try_from(*count).map_err(|_| AUDIO_UNSUPPORTED)?;
    let path = paths.get_mut(index).ok_or(AUDIO_UNSUPPORTED)?;
    let bytes = source.name.to_bytes();
    if bytes.len() >= path.element.len() {
        return Err(AUDIO_UNSUPPORTED);
    }
    path.element.fill(0);
    for (destination, source) in path.element.iter_mut().zip(bytes) {
        *destination = *source as c_char;
    }
    path.element_index = source.element_index;
    path.channel = source.channel.as_ffi();
    path.direction = source.direction.as_ffi();
    path.capabilities = (u32::from(source.volume_supported) * CM119_MIXER_PATH_VOLUME)
        | (u32::from(source.switch_supported) * CM119_MIXER_PATH_SWITCH);
    *count += 1;
    Ok(())
}

/// Resolve CM119 semantic mixer paths from active ALSA simple-mixer elements.
///
/// This mirrors the legacy resource module's capability classification rather
/// than assuming a particular `Speaker` or `Headphone` spelling.  It keeps the
/// discovered values owned by the output ABI structure, so the temporary ALSA
/// mixer can close before the caller opens individual controls.
fn cm119_mixer_paths_resolve_with_functions_and_root(
    functions: &'static ffi::FunctionTable,
    sound_class_root: &Path,
    usb_interface_path: *const c_char,
    paths: *mut Cm119MixerPaths,
) -> c_int {
    let Some(paths) = NonNull::new(paths) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    let paths = unsafe { paths.as_ptr().as_mut() }.expect("non-null checked above");
    let paths_size = paths.struct_size;
    *paths = Cm119MixerPaths {
        struct_size: paths_size,
        ..Cm119MixerPaths::default()
    };
    if paths_size < size_of::<Cm119MixerPaths>() as u32 || usb_interface_path.is_null() {
        return AUDIO_INVALID_ARGUMENT;
    }
    let usb_interface_path = match unsafe { CStr::from_ptr(usb_interface_path) }.to_str() {
        Ok(usb_interface_path) if valid_usb_interface_path(usb_interface_path) => {
            usb_interface_path
        }
        _ => return AUDIO_INVALID_ARGUMENT,
    };
    let card_index = match resolve_alsa_card_index_from_sysfs(sound_class_root, usb_interface_path)
    {
        Ok(card_index) => card_index,
        Err(error) => return error,
    };
    let card = CString::new(format!("hw:{card_index}"))
        .expect("formatted ALSA card name cannot contain an interior NUL");
    let mut mixer = ptr::null_mut();
    if unsafe { (functions.alsa.mixer_open)(&mut mixer, 0) } < 0 {
        return AUDIO_ALSA_ERROR;
    }
    if unsafe { (functions.alsa.mixer_attach)(mixer, card.as_ptr()) } < 0
        || unsafe { (functions.alsa.mixer_selem_register)(mixer, ptr::null_mut(), ptr::null_mut()) }
            < 0
        || unsafe { (functions.alsa.mixer_load)(mixer) } < 0
    {
        let _ = unsafe { (functions.alsa.mixer_close)(mixer) };
        return AUDIO_ALSA_ERROR;
    }

    let result = (|| -> Result<Cm119MixerPaths, c_int> {
        let mut resolved = Cm119MixerPaths {
            struct_size: paths_size,
            abi_version: ABI_VERSION,
            ..Cm119MixerPaths::default()
        };
        let mut element = unsafe { (functions.alsa.mixer_first_elem)(mixer) };
        while !element.is_null() {
            let next = unsafe { (functions.alsa.mixer_elem_next)(element) };
            if unsafe { (functions.alsa.selem_is_active)(element) } == 0 {
                element = next;
                continue;
            }
            let name = unsafe { (functions.alsa.selem_get_name)(element) };
            if name.is_null() {
                return Err(AUDIO_ALSA_ERROR);
            }
            let name = unsafe { CStr::from_ptr(name) };
            let element_index = unsafe { (functions.alsa.selem_get_index)(element) };
            let capture_volume = unsafe { (functions.alsa.selem_has_capture_volume)(element) } != 0;
            let playback_volume =
                unsafe { (functions.alsa.selem_has_playback_volume)(element) } != 0;
            let capture_switch = unsafe { (functions.alsa.selem_has_capture_switch)(element) } != 0;
            let playback_switch =
                unsafe { (functions.alsa.selem_has_playback_switch)(element) } != 0;
            let compatibility_switch =
                name.to_bytes().eq_ignore_ascii_case(CM119_RX_BOOST_ELEMENT) && playback_switch;

            for channel in [MixerChannel::Left, MixerChannel::Right] {
                let alsa_channel = channel.as_alsa_channel();
                if compatibility_switch
                    && unsafe { (functions.alsa.selem_has_playback_channel)(element, alsa_channel) }
                        != 0
                {
                    append_cm119_mixer_path(
                        &mut resolved.rx_compatibility_switch_paths,
                        &mut resolved.rx_compatibility_switch_path_count,
                        Cm119MixerPathSource {
                            name,
                            element_index,
                            channel,
                            direction: MixerDirection::Playback,
                            volume_supported: false,
                            switch_supported: true,
                        },
                    )?;
                    continue;
                }
                if capture_volume
                    && unsafe { (functions.alsa.selem_has_capture_channel)(element, alsa_channel) }
                        != 0
                {
                    append_cm119_mixer_path(
                        &mut resolved.rx_capture_paths,
                        &mut resolved.rx_capture_path_count,
                        Cm119MixerPathSource {
                            name,
                            element_index,
                            channel,
                            direction: MixerDirection::Capture,
                            volume_supported: true,
                            switch_supported: capture_switch,
                        },
                    )?;
                }
                if playback_volume
                    && unsafe { (functions.alsa.selem_has_playback_channel)(element, alsa_channel) }
                        != 0
                {
                    if capture_volume {
                        append_cm119_mixer_path(
                            &mut resolved.sidetone_paths,
                            &mut resolved.sidetone_path_count,
                            Cm119MixerPathSource {
                                name,
                                element_index,
                                channel,
                                direction: MixerDirection::Playback,
                                volume_supported: true,
                                switch_supported: playback_switch,
                            },
                        )?;
                    } else if resolved.tx_playback_path_count < CM119_MIXER_PATH_CAPACITY as u32 {
                        // Legacy USBRadioPlus maps only the first two TX paths
                        // to MIXA and MIXB; leave additional paths unchanged.
                        append_cm119_mixer_path(
                            &mut resolved.tx_playback_paths,
                            &mut resolved.tx_playback_path_count,
                            Cm119MixerPathSource {
                                name,
                                element_index,
                                channel,
                                direction: MixerDirection::Playback,
                                volume_supported: true,
                                switch_supported: playback_switch,
                            },
                        )?;
                    }
                }
            }
            element = next;
        }
        if resolved.rx_capture_path_count == 0 || resolved.tx_playback_path_count == 0 {
            return Err(AUDIO_UNSUPPORTED);
        }
        Ok(resolved)
    })();
    let _ = unsafe { (functions.alsa.mixer_close)(mixer) };
    match result {
        Ok(resolved) => {
            *paths = resolved;
            AUDIO_OK
        }
        Err(error) => error,
    }
}

extern "C" fn cm119_mixer_paths_resolve(
    usb_interface_path: *const c_char,
    paths: *mut Cm119MixerPaths,
) -> c_int {
    cm119_mixer_paths_resolve_with_functions_and_root(
        &ffi::PRODUCTION_FUNCTIONS,
        Path::new("/sys/class/sound"),
        usb_interface_path,
        paths,
    )
}

extern "C" fn mixer_get_range_steps(
    mixer: *const AudioMixer,
    minimum: *mut i64,
    maximum: *mut i64,
) -> c_int {
    let Some(mixer) = NonNull::new(mixer.cast_mut()) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    if minimum.is_null() || maximum.is_null() {
        return AUDIO_INVALID_ARGUMENT;
    }
    let (low, high) = match unsafe { mixer.as_ref() }.get_step_range() {
        Ok(range) => range,
        Err(error) => return error,
    };
    unsafe {
        *minimum = low;
        *maximum = high;
    }
    AUDIO_OK
}

extern "C" fn mixer_get_steps(mixer: *const AudioMixer, value: *mut i64) -> c_int {
    let Some(mixer) = NonNull::new(mixer.cast_mut()) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    if value.is_null() {
        return AUDIO_INVALID_ARGUMENT;
    }
    let value_from_mixer = match unsafe { mixer.as_ref() }.get_steps() {
        Ok(value_from_mixer) => value_from_mixer,
        Err(error) => return error,
    };
    unsafe {
        *value = value_from_mixer;
    }
    AUDIO_OK
}

extern "C" fn mixer_set_steps(mixer: *mut AudioMixer, value: i64) -> c_int {
    let Some(mut mixer) = NonNull::new(mixer) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    match unsafe { mixer.as_mut() }.set_steps(value) {
        Ok(()) => AUDIO_OK,
        Err(error) => error,
    }
}

fn normalized_from_steps(value: i64, minimum: i64, maximum: i64) -> Result<u32, c_int> {
    if minimum >= maximum {
        return Err(AUDIO_UNSUPPORTED);
    }
    if value < minimum || value > maximum {
        return Err(AUDIO_ALSA_ERROR);
    }
    let span = i128::from(maximum) - i128::from(minimum);
    let offset = i128::from(value) - i128::from(minimum);
    let normalized = (offset * i128::from(MIXER_NORMALIZED_MAXIMUM) + (span / 2)) / span;
    // `value` is inside the inclusive native range, so this ratio is always
    // within the 0 through 999 public normalized range.
    Ok(normalized as u32)
}

fn steps_from_normalized(value: u32, minimum: i64, maximum: i64) -> Result<i64, c_int> {
    if value > MIXER_NORMALIZED_MAXIMUM {
        return Err(AUDIO_INVALID_ARGUMENT);
    }
    if minimum >= maximum {
        return Err(AUDIO_UNSUPPORTED);
    }
    let span = i128::from(maximum) - i128::from(minimum);
    let offset = (i128::from(value) * span + (i128::from(MIXER_NORMALIZED_MAXIMUM) / 2))
        / i128::from(MIXER_NORMALIZED_MAXIMUM);
    // `offset` is inside the native range, so adding it to `minimum` remains
    // representable as the same `i64` range reported by ALSA.
    Ok((i128::from(minimum) + offset) as i64)
}

extern "C" fn mixer_get_normalized(mixer: *const AudioMixer, value: *mut u32) -> c_int {
    let Some(mixer) = NonNull::new(mixer.cast_mut()) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    if value.is_null() {
        return AUDIO_INVALID_ARGUMENT;
    }
    let mixer = unsafe { mixer.as_ref() };
    let (minimum, maximum) = match mixer.get_step_range() {
        Ok(range) => range,
        Err(error) => return error,
    };
    let steps = match mixer.get_steps() {
        Ok(steps) => steps,
        Err(error) => return error,
    };
    let normalized = match normalized_from_steps(steps, minimum, maximum) {
        Ok(normalized) => normalized,
        Err(error) => return error,
    };
    unsafe {
        *value = normalized;
    }
    AUDIO_OK
}

extern "C" fn mixer_set_normalized(mixer: *mut AudioMixer, value: u32) -> c_int {
    let Some(mut mixer) = NonNull::new(mixer) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    let mixer = unsafe { mixer.as_mut() };
    let (minimum, maximum) = match mixer.get_step_range() {
        Ok(range) => range,
        Err(error) => return error,
    };
    let steps = match steps_from_normalized(value, minimum, maximum) {
        Ok(steps) => steps,
        Err(error) => return error,
    };
    match mixer.set_steps(steps) {
        Ok(()) => AUDIO_OK,
        Err(error) => error,
    }
}

extern "C" fn mixer_get_switch(mixer: *const AudioMixer, enabled: *mut u32) -> c_int {
    let Some(mixer) = NonNull::new(mixer.cast_mut()) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    if enabled.is_null() {
        return AUDIO_INVALID_ARGUMENT;
    }
    let enabled_value = match unsafe { mixer.as_ref() }.get_switch() {
        Ok(enabled_value) => enabled_value,
        Err(error) => return error,
    };
    unsafe {
        *enabled = u32::from(enabled_value);
    }
    AUDIO_OK
}

extern "C" fn mixer_set_switch(mixer: *mut AudioMixer, enabled: u32) -> c_int {
    if enabled > 1 {
        return AUDIO_INVALID_ARGUMENT;
    }
    let Some(mut mixer) = NonNull::new(mixer) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    match unsafe { mixer.as_mut() }.set_switch(enabled != 0) {
        Ok(()) => AUDIO_OK,
        Err(error) => error,
    }
}

static DESCRIPTOR: AdapterDescriptor = AdapterDescriptor {
    struct_size: size_of::<AdapterDescriptor>() as u32,
    abi_version: ABI_VERSION,
    capability_name: CAPABILITY_NAME.as_ptr().cast::<c_char>(),
    stream_create,
    stream_start,
    stream_stop,
    stream_get_stats,
    stream_destroy,
    mixer_create,
    mixer_get_range_centibels,
    mixer_get_centibels,
    mixer_set_centibels,
    mixer_destroy,
    mixer_create_for_usb_interface,
    mixer_get_range_steps,
    mixer_get_steps,
    mixer_set_steps,
    mixer_get_normalized,
    mixer_set_normalized,
    mixer_get_switch,
    mixer_set_switch,
    usb_device_resolve,
    usb_device_select,
    stream_get_timing,
    cm119_mixer_paths_resolve,
};

/// Return the immutable function table for ABI version one.
#[unsafe(no_mangle)]
pub extern "C" fn rptadv_portaudio_alsa_adapter_descriptor() -> *const AdapterDescriptor {
    &DESCRIPTOR
}

#[cfg(test)]
mod tests;
