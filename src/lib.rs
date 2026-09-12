//! Versioned PortAudio/ALSA adapter with a narrow C-compatible descriptor ABI.
//!
//! The callback-side contract is canonical interleaved stereo `f32` PCM.
//! PortAudio/ALSA performs physical-device conversion below the `paFloat32`
//! callback, keeping S16/S24 details out of the core.

#![deny(unsafe_op_in_unsafe_fn)]

mod ffi;
mod pcm;

use std::ffi::{CStr, c_char, c_int, c_void};
use std::mem::size_of;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

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
const DEFAULT_DEVICE: i32 = -1;
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
}

impl SharedStats {
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
        // The callback API hands complete device buffers directly to the
        // native tick, so this adapter intentionally owns no PCM queue.
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
    }
}

/// PortAudio callback state. Only the callback mutates its workspaces and
/// meter accumulators; readers consume the published atomic snapshot.
#[repr(C)]
struct AudioStream {
    portaudio_stream: *mut PaStream,
    functions: &'static ffi::FunctionTable,
    config: ValidatedStreamConfig,
    canonical_input: Box<[f32]>,
    canonical_output: Box<[f32]>,
    input_meter: MeterAccumulator,
    output_meter: MeterAccumulator,
    stats: SharedStats,
}

impl AudioStream {
    fn new(functions: &'static ffi::FunctionTable, config: ValidatedStreamConfig) -> Self {
        // `maximum_frame_count` originates as a u32 and supported targets are
        // 64-bit, so two canonical channels cannot overflow this workspace.
        let sample_count = config.maximum_frame_count * pcm::CANONICAL_CHANNELS;
        // Rust aborts on allocation failure. There is no reliable recoverable
        // allocation path here, so do not expose an unreachable C error path.
        Self {
            portaudio_stream: ptr::null_mut(),
            functions,
            config,
            canonical_input: vec![0.0; sample_count].into_boxed_slice(),
            canonical_output: vec![0.0; sample_count].into_boxed_slice(),
            input_meter: MeterAccumulator::default(),
            output_meter: MeterAccumulator::default(),
            stats: SharedStats::default(),
        }
    }

    fn publish_meters(&self) {
        self.stats
            .input_peak_bits
            .store(self.input_meter.peak().to_bits(), Ordering::Release);
        self.stats
            .input_rms_bits
            .store(self.input_meter.rms().to_bits(), Ordering::Release);
        self.stats
            .output_peak_bits
            .store(self.output_meter.peak().to_bits(), Ordering::Release);
        self.stats
            .output_rms_bits
            .store(self.output_meter.rms().to_bits(), Ordering::Release);
        self.stats
            .input_clip_sample_count
            .store(self.input_meter.clip_sample_count(), Ordering::Release);
        self.stats
            .output_clip_sample_count
            .store(self.output_meter.clip_sample_count(), Ordering::Release);
    }

    unsafe fn process_callback(
        &mut self,
        input: *const f32,
        output: *mut f32,
        frame_count: usize,
        status_flags: PaStreamCallbackFlags,
    ) -> c_int {
        if output.is_null() {
            self.stats
                .native_tick_failure_count
                .fetch_add(1, Ordering::Relaxed);
            return ffi::PA_ABORT;
        }
        // PortAudio requires a complete physical output block even when a
        // later native-tick chunk fails. Pre-silencing makes unprocessed
        // chunks deterministic without allocating in the callback.
        let physical_output_sample_count = frame_count * self.config.output_channels;
        let physical_output =
            unsafe { std::slice::from_raw_parts_mut(output, physical_output_sample_count) };
        physical_output.fill(0.0);
        if status_flags & ffi::PA_INPUT_OVERFLOW != 0 {
            self.stats
                .input_overflow_count
                .fetch_add(1, Ordering::Relaxed);
        }
        if status_flags & ffi::PA_OUTPUT_UNDERFLOW != 0 {
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

        let mut frame_offset = 0;
        while frame_offset < frame_count {
            let remaining = frame_count - frame_offset;
            let chunk_frames = remaining.min(self.config.maximum_frame_count);
            let canonical_samples = chunk_frames * pcm::CANONICAL_CHANNELS;
            let device_input = if input.is_null() {
                None
            } else {
                let input_offset = frame_offset * self.config.input_channels;
                let input_samples = chunk_frames * self.config.input_channels;
                Some(unsafe { std::slice::from_raw_parts(input.add(input_offset), input_samples) })
            };
            let output_offset = frame_offset * self.config.output_channels;
            let output_samples = chunk_frames * self.config.output_channels;
            let device_output = unsafe {
                std::slice::from_raw_parts_mut(output.add(output_offset), output_samples)
            };
            let canonical_input = &mut self.canonical_input[..canonical_samples];
            let canonical_output = &mut self.canonical_output[..canonical_samples];

            if let Some(device_input) = device_input {
                self.input_meter.observe(device_input);
            }
            device_input_to_canonical(device_input, self.config.input_channels, canonical_input);
            canonical_output.fill(0.0);
            let tick_status = unsafe {
                (self.config.native_tick)(
                    self.config.native_tick_context,
                    canonical_input.as_ptr(),
                    canonical_output.as_mut_ptr(),
                    chunk_frames as u32,
                )
            };
            if tick_status != 0 {
                device_output.fill(0.0);
                self.stats
                    .native_tick_failure_count
                    .fetch_add(1, Ordering::Relaxed);
                self.publish_meters();
                return ffi::PA_ABORT;
            }
            canonical_output_to_device(
                canonical_output,
                self.config.output_channels,
                device_output,
            );
            self.output_meter.observe(device_output);
            frame_offset += chunk_frames;
        }

        self.publish_meters();
        ffi::PA_CONTINUE
    }
}

unsafe extern "C" fn portaudio_callback(
    input: *const c_void,
    output: *mut c_void,
    frame_count: std::ffi::c_ulong,
    _time_info: *const ffi::PaStreamCallbackTimeInfo,
    status_flags: PaStreamCallbackFlags,
    user_data: *mut c_void,
) -> c_int {
    if user_data.is_null() {
        return ffi::PA_ABORT;
    }

    let stream = unsafe { &mut *(user_data.cast::<AudioStream>()) };
    unsafe {
        stream.process_callback(
            input.cast::<f32>(),
            output.cast::<f32>(),
            frame_count as usize,
            status_flags,
        )
    }
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
    if let Err(error) = portaudio_acquire(functions) {
        return error;
    }
    let (_, input_parameters) = match resolve_device(
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
    let (_, output_parameters) = match resolve_device(
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
    let boxed_stream = Box::new(AudioStream::new(functions, config));
    let raw_stream = Box::into_raw(boxed_stream);
    let result = unsafe {
        (functions.portaudio.open_stream)(
            &mut (*raw_stream).portaudio_stream,
            &input_parameters,
            &output_parameters,
            f64::from(config.sample_rate_hz),
            config.maximum_frame_count as std::ffi::c_ulong,
            0,
            Some(portaudio_callback),
            raw_stream.cast::<c_void>(),
        )
    };
    if result != ffi::PA_NO_ERROR {
        unsafe {
            (*raw_stream).stats.record_portaudio_error(result);
            drop(Box::from_raw(raw_stream));
        }
        portaudio_release(functions);
        return AUDIO_PORTAUDIO_ERROR;
    }
    unsafe {
        *stream = raw_stream;
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
    if active < 0 {
        stream.stats.record_portaudio_error(active);
        return AUDIO_PORTAUDIO_ERROR;
    }
    if active == 1 {
        return AUDIO_OK;
    }
    let result = unsafe { (stream.functions.portaudio.start_stream)(stream.portaudio_stream) };
    if result != ffi::PA_NO_ERROR {
        stream.stats.record_portaudio_error(result);
        return AUDIO_PORTAUDIO_ERROR;
    }
    AUDIO_OK
}

extern "C" fn stream_stop(stream: *mut AudioStream) -> c_int {
    let Some(stream) = NonNull::new(stream) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    let stream = unsafe { stream.as_ref() };
    let active = unsafe { (stream.functions.portaudio.is_stream_active)(stream.portaudio_stream) };
    if active < 0 {
        stream.stats.record_portaudio_error(active);
        return AUDIO_PORTAUDIO_ERROR;
    }
    if active == 0 {
        return AUDIO_OK;
    }
    let result = unsafe { (stream.functions.portaudio.stop_stream)(stream.portaudio_stream) };
    if result != ffi::PA_NO_ERROR {
        stream.stats.record_portaudio_error(result);
        return AUDIO_PORTAUDIO_ERROR;
    }
    AUDIO_OK
}

extern "C" fn stream_get_stats(stream: *const AudioStream, stats: *mut StreamStats) -> c_int {
    let Some(stream) = NonNull::new(stream.cast_mut()) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    let Some(stats) = NonNull::new(stats) else {
        return AUDIO_INVALID_ARGUMENT;
    };
    let stats = unsafe { stats.as_ptr().as_mut() }.expect("non-null checked above");
    if stats.struct_size < size_of::<StreamStats>() as u32 {
        return AUDIO_INVALID_ARGUMENT;
    }
    let stream = unsafe { stream.as_ref() };
    stream.stats.snapshot(stats);
    AUDIO_OK
}

extern "C" fn stream_destroy(stream: *mut AudioStream) {
    let Some(stream) = NonNull::new(stream) else {
        return;
    };
    let stream = unsafe { Box::from_raw(stream.as_ptr()) };
    let functions = stream.functions;
    if !stream.portaudio_stream.is_null() {
        let active = unsafe { (functions.portaudio.is_stream_active)(stream.portaudio_stream) };
        if active > 0 {
            let result = unsafe { (functions.portaudio.stop_stream)(stream.portaudio_stream) };
            if result != ffi::PA_NO_ERROR {
                stream.stats.record_portaudio_error(result);
                let _ = unsafe { (functions.portaudio.abort_stream)(stream.portaudio_stream) };
            }
        }
        let result = unsafe { (functions.portaudio.close_stream)(stream.portaudio_stream) };
        if result != ffi::PA_NO_ERROR {
            stream.stats.record_portaudio_error(result);
        }
    }
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

    fn get_range(&self) -> Result<(i64, i64), c_int> {
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
        (functions.alsa.selem_id_set_index)(element_id, config.element_index);
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
    if !audio_mixer.volume_supported() {
        drop(audio_mixer);
        return AUDIO_UNSUPPORTED;
    }
    unsafe {
        *mixer = Box::into_raw(Box::new(audio_mixer));
    }
    AUDIO_OK
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
};

/// Return the immutable function table for ABI version one.
#[unsafe(no_mangle)]
pub extern "C" fn rptadv_portaudio_alsa_adapter_descriptor() -> *const AdapterDescriptor {
    &DESCRIPTOR
}

#[cfg(test)]
mod tests;
