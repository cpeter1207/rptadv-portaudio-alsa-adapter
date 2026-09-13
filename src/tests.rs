//! Rust-only unit tests for the private adapter implementation.
//!
//! Keeping test code in this file lets production coverage exclude it without
//! weakening coverage of the adapter's shipped source parts.

use super::pcm::{MeterAccumulator, canonical_output_to_device, device_input_to_canonical};
use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::CString;
use std::fs;
#[cfg(unix)]
use std::os::unix::{ffi::OsStringExt, fs::symlink};
use std::path::Path;

#[test]
fn mono_input_is_duplicated_without_format_conversion() {
    let device = [0.25, -0.5];
    let mut canonical = [0.0; 4];

    device_input_to_canonical(Some(&device), 1, &mut canonical);

    assert_eq!(canonical, [0.25, 0.25, -0.5, -0.5]);
}

#[test]
fn absent_input_is_silence() {
    let mut canonical = [1.0; 4];

    device_input_to_canonical(None, 1, &mut canonical);

    assert_eq!(canonical, [0.0; 4]);
}

#[test]
fn stereo_input_is_preserved() {
    let device = [0.25, -0.5, 0.75, -1.0];
    let mut canonical = [0.0; 4];

    device_input_to_canonical(Some(&device), 2, &mut canonical);

    assert_eq!(canonical, device);
}

#[test]
fn mono_output_averages_stereo_program() {
    let canonical = [0.5, -0.25, 1.0, 0.0];
    let mut device = [0.0; 2];

    canonical_output_to_device(&canonical, 1, &mut device);

    assert_eq!(device, [0.125, 0.5]);
}

#[test]
fn stereo_output_is_preserved() {
    let canonical = [0.25, -0.5, 0.75, -1.0];
    let mut device = [0.0; 4];

    canonical_output_to_device(&canonical, 2, &mut device);

    assert_eq!(device, canonical);
}

#[test]
fn meter_tracks_peak_rms_and_clipping() {
    let mut meter = MeterAccumulator::default();

    meter.observe(&[0.0, -1.0, 0.5, 1.25]);

    assert_eq!(meter.peak(), 1.25);
    assert_eq!(meter.clip_sample_count(), 2);
    assert!((meter.rms() - 0.838_525_5).abs() < 0.000_001);
}

unsafe extern "C" fn copy_input_to_output(
    _context: *mut c_void,
    input: *const f32,
    output: *mut f32,
    frame_count: u32,
) -> i32 {
    let sample_count = frame_count as usize * pcm::CANONICAL_CHANNELS;
    unsafe {
        ptr::copy_nonoverlapping(input, output, sample_count);
    }
    0
}

unsafe extern "C" fn fail_tick(
    _context: *mut c_void,
    _input: *const f32,
    _output: *mut f32,
    _frame_count: u32,
) -> i32 {
    1
}

unsafe extern "C" fn fail_on_second_tick(
    context: *mut c_void,
    input: *const f32,
    output: *mut f32,
    frame_count: u32,
) -> i32 {
    let invocation = unsafe { &mut *context.cast::<usize>() };
    *invocation += 1;
    if *invocation == 2 {
        return 1;
    }
    unsafe { copy_input_to_output(ptr::null_mut(), input, output, frame_count) }
}

fn prepared_test_stream(
    functions: &'static ffi::FunctionTable,
    config: ValidatedStreamConfig,
) -> AudioStream {
    AudioStream::new(functions, config).expect("prepare shared capture ring")
}

impl AudioStream {
    /// Exercise the actual bounded render/mapping path with an exact input
    /// provider, independently of the separately tested asynchronous SRC.
    unsafe fn process_callback(
        &mut self,
        input: *const f32,
        output: *mut f32,
        frames: usize,
        flags: PaStreamCallbackFlags,
    ) -> c_int {
        let capture = unsafe { &mut *self.capture.get() };
        if flags & ffi::PA_INPUT_OVERFLOW != 0 {
            self.stats
                .input_overflow_count
                .fetch_add(1, Ordering::Relaxed);
        }
        let channels = self.config.input_channels;
        let mut offset = 0;
        let result = unsafe {
            (*self.playback.get()).render_callback(output, frames, flags, |mono, canonical| {
                let samples = if input.is_null() {
                    None
                } else {
                    Some(std::slice::from_raw_parts(
                        input.add(offset * channels),
                        mono.len() * channels,
                    ))
                };
                if let Some(samples) = samples {
                    capture.input_meter.observe(samples);
                }
                device_input_to_canonical(samples, channels, canonical);
                offset += mono.len();
                Ok(())
            })
        };
        capture.publish_meter();
        result
    }
}

fn test_config(
    maximum_frame_count: usize,
    input_channels: usize,
    output_channels: usize,
    native_tick: unsafe extern "C" fn(*mut c_void, *const f32, *mut f32, u32) -> i32,
) -> ValidatedStreamConfig {
    ValidatedStreamConfig {
        sample_rate_hz: 48_000,
        maximum_frame_count,
        input_device_index: DEFAULT_DEVICE,
        output_device_index: DEFAULT_DEVICE,
        input_channels,
        output_channels,
        native_tick,
        native_tick_context: ptr::null_mut(),
    }
}

struct FakePortAudioState {
    endpoint_active: Vec<PaError>,
    open_results: VecDeque<PaError>,
    start_results: VecDeque<PaError>,
    capture_callback: Option<ffi::PaStreamCallback>,
    capture_context: *mut c_void,
    initialize_count: u32,
    terminate_count: u32,
    open_count: u32,
    close_count: u32,
    start_count: u32,
    stop_count: u32,
    abort_count: u32,
    active: PaError,
    device_count: PaError,
    default_input_device: PaDeviceIndex,
    default_output_device: PaDeviceIndex,
    device_info_available: bool,
    stream_info_available: bool,
    use_plugin_name_for_first_device: bool,
    use_null_name_for_first_device: bool,
    use_invalid_utf8_name_for_first_device: bool,
    initialize_result: PaError,
    open_result: PaError,
    start_result: PaError,
    stop_result: PaError,
    abort_result: PaError,
    close_result: PaError,
    input_format: ffi::PaSampleFormat,
    output_format: ffi::PaSampleFormat,
    input_latency: f64,
    output_latency: f64,
    sample_rate: f64,
    frames_per_buffer: std::ffi::c_ulong,
    callback: Option<ffi::PaStreamCallback>,
    callback_context: *mut c_void,
}

impl Default for FakePortAudioState {
    fn default() -> Self {
        Self {
            endpoint_active: Vec::new(),
            open_results: VecDeque::new(),
            start_results: VecDeque::new(),
            capture_callback: None,
            capture_context: ptr::null_mut(),
            initialize_count: 0,
            terminate_count: 0,
            open_count: 0,
            close_count: 0,
            start_count: 0,
            stop_count: 0,
            abort_count: 0,
            active: 0,
            device_count: 0,
            default_input_device: ffi::PA_NO_DEVICE,
            default_output_device: ffi::PA_NO_DEVICE,
            device_info_available: false,
            stream_info_available: false,
            use_plugin_name_for_first_device: false,
            use_null_name_for_first_device: false,
            use_invalid_utf8_name_for_first_device: false,
            initialize_result: 0,
            open_result: 0,
            start_result: 0,
            stop_result: 0,
            abort_result: 0,
            close_result: 0,
            input_format: 0,
            output_format: 0,
            input_latency: 0.0,
            output_latency: 0.0,
            sample_rate: 0.0,
            frames_per_buffer: 0,
            callback: None,
            callback_context: ptr::null_mut(),
        }
    }
}

impl FakePortAudioState {
    fn force_active(&mut self, active: PaError) {
        self.endpoint_active.fill(active);
        self.active = active;
    }

    fn set_active(&mut self, stream: *mut PaStream, active: PaError) {
        self.endpoint_active[stream as usize - 1] = active;
        self.active = i32::from(self.endpoint_active.iter().any(|value| *value == 1));
    }
    fn reset() -> Self {
        Self {
            device_count: 2,
            default_input_device: 0,
            default_output_device: 1,
            device_info_available: true,
            stream_info_available: true,
            ..Self::default()
        }
    }
}

/// Model caller scheduling independently of the fake PortAudio callback lifetime.
#[derive(Default)]
struct FakeSchedulingState {
    policy: c_int,
    priority: c_int,
    maximum: c_int,
    get_result: c_int,
    set_results: VecDeque<c_int>,
    set_requests: Vec<(c_int, c_int)>,
    start_schedule: Option<(c_int, c_int)>,
    events: Vec<&'static str>,
}

impl FakeSchedulingState {
    fn reset() -> Self {
        Self {
            policy: 2, // The real Asterisk starter used SCHED_RR priority 10.
            priority: 10,
            maximum: 99,
            ..Self::default()
        }
    }
}

#[derive(Default)]
struct FakeAlsaState {
    open_count: u32,
    attach_count: u32,
    register_count: u32,
    load_count: u32,
    handle_events_count: u32,
    handle_events_result: c_int,
    id_malloc_count: u32,
    id_free_count: u32,
    find_count: u32,
    close_count: u32,
    centibels: i64,
    steps: i64,
    capture_step_minimum: i64,
    capture_step_maximum: i64,
    playback_step_minimum: i64,
    playback_step_maximum: i64,
    attached_card: String,
    last_channel: c_int,
    open_result: c_int,
    attach_result: c_int,
    register_result: c_int,
    load_result: c_int,
    id_malloc_result: c_int,
    element_available: bool,
    capture_volume_available: bool,
    playback_volume_available: bool,
    capture_switch_available: bool,
    playback_switch_available: bool,
    switch_enabled: bool,
    capture_range_result: c_int,
    playback_range_result: c_int,
    capture_get_result: c_int,
    playback_get_result: c_int,
    capture_set_result: c_int,
    playback_set_result: c_int,
    capture_step_get_result: c_int,
    playback_step_get_result: c_int,
    capture_step_set_result: c_int,
    playback_step_set_result: c_int,
    capture_switch_get_result: c_int,
    playback_switch_get_result: c_int,
    capture_switch_set_result: c_int,
    playback_switch_set_result: c_int,
    inventory: Vec<FakeMixerElement>,
}

#[derive(Clone)]
struct FakeMixerElement {
    name: CString,
    index: u32,
    active: bool,
    capture_volume: bool,
    playback_volume: bool,
    capture_switch: bool,
    playback_switch: bool,
    capture_channels: [bool; 2],
    playback_channels: [bool; 2],
}

#[derive(Clone, Copy)]
struct FakeMixerCapabilities {
    capture_volume: bool,
    playback_volume: bool,
    capture_switch: bool,
    playback_switch: bool,
    capture_channels: [bool; 2],
    playback_channels: [bool; 2],
}

impl FakeMixerElement {
    fn new(name: &str, index: u32, capabilities: FakeMixerCapabilities) -> Self {
        Self {
            name: CString::new(name).expect("test mixer name has no NUL"),
            index,
            active: true,
            capture_volume: capabilities.capture_volume,
            playback_volume: capabilities.playback_volume,
            capture_switch: capabilities.capture_switch,
            playback_switch: capabilities.playback_switch,
            capture_channels: capabilities.capture_channels,
            playback_channels: capabilities.playback_channels,
        }
    }
}

fn fake_device_info() -> ffi::PaDeviceInfo {
    ffi::PaDeviceInfo {
        struct_version: 1,
        name: TEST_DEVICE_NAME_0.as_ptr().cast(),
        host_api: 0,
        max_input_channels: 2,
        max_output_channels: 2,
        default_low_input_latency: 0.004,
        default_low_output_latency: 0.006,
        default_high_input_latency: 0.02,
        default_high_output_latency: 0.02,
        default_sample_rate: 48_000.0,
    }
}

fn fake_stream_info() -> ffi::PaStreamInfo {
    ffi::PaStreamInfo {
        struct_version: 1,
        input_latency: 0.008,
        output_latency: 0.012,
        sample_rate: 47_999.5,
    }
}

thread_local! {
    static FAKE_PORTAUDIO: RefCell<FakePortAudioState> = RefCell::new(FakePortAudioState::reset());
    static FAKE_SCHEDULING: RefCell<FakeSchedulingState> = RefCell::new(FakeSchedulingState::reset());
    static FAKE_ALSA: RefCell<FakeAlsaState> = RefCell::new(FakeAlsaState {
        centibels: -1_200,
        steps: 18,
        capture_step_minimum: 0,
        capture_step_maximum: 31,
        playback_step_minimum: 0,
        playback_step_maximum: 31,
        element_available: true,
        capture_volume_available: true,
        playback_volume_available: true,
        capture_switch_available: true,
        playback_switch_available: true,
        switch_enabled: true,
        ..FakeAlsaState::default()
    });
    static FAKE_DEVICE_INFO: RefCell<ffi::PaDeviceInfo> = RefCell::new(fake_device_info());
    static FAKE_STREAM_INFO: RefCell<ffi::PaStreamInfo> = RefCell::new(fake_stream_info());
}

static TEST_FFI_SERIAL: Mutex<()> = Mutex::new(());
static TEST_DEVICE_NAME_0: [u8; 15] = *b"CM119 (hw:4,0)\0";
static TEST_DEVICE_NAME_1: [u8; 15] = *b"Other (hw:7,0)\0";
static TEST_DEVICE_NAME_PLUGIN: [u8; 19] = *b"CM119 (plughw:4,0)\0";
static TEST_DEVICE_NAME_INVALID_UTF8: [u8; 2] = [0xff, 0];

fn reset_fake_functions() {
    FAKE_PORTAUDIO.with(|state| *state.borrow_mut() = FakePortAudioState::reset());
    FAKE_SCHEDULING.with(|state| *state.borrow_mut() = FakeSchedulingState::reset());
    FAKE_ALSA.with(|state| {
        *state.borrow_mut() = FakeAlsaState {
            centibels: -1_200,
            steps: 18,
            capture_step_minimum: 0,
            capture_step_maximum: 31,
            playback_step_minimum: 0,
            playback_step_maximum: 31,
            element_available: true,
            capture_volume_available: true,
            playback_volume_available: true,
            capture_switch_available: true,
            playback_switch_available: true,
            switch_enabled: true,
            ..FakeAlsaState::default()
        };
    });
    FAKE_DEVICE_INFO.with(|info| *info.borrow_mut() = fake_device_info());
    FAKE_STREAM_INFO.with(|info| *info.borrow_mut() = fake_stream_info());
}

fn reset_fake_stream_info() {
    FAKE_STREAM_INFO.with(|info| *info.borrow_mut() = fake_stream_info());
}

fn lock_fake() -> std::sync::MutexGuard<'static, ()> {
    TEST_FFI_SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

unsafe extern "C" fn fake_pa_initialize() -> PaError {
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.initialize_count += 1;
        state.initialize_result
    })
}

unsafe extern "C" fn fake_pa_terminate() -> PaError {
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().terminate_count += 1);
    ffi::PA_NO_ERROR
}

unsafe extern "C" fn fake_pa_get_device_count() -> PaError {
    FAKE_PORTAUDIO.with(|state| state.borrow().device_count)
}

unsafe extern "C" fn fake_pa_get_default_input_device() -> PaDeviceIndex {
    FAKE_PORTAUDIO.with(|state| state.borrow().default_input_device)
}

unsafe extern "C" fn fake_pa_get_default_output_device() -> PaDeviceIndex {
    FAKE_PORTAUDIO.with(|state| state.borrow().default_output_device)
}

unsafe extern "C" fn fake_pa_get_device_info(device: PaDeviceIndex) -> *const ffi::PaDeviceInfo {
    let state = FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        (
            state.device_info_available,
            state.device_count,
            state.use_plugin_name_for_first_device,
            state.use_null_name_for_first_device,
            state.use_invalid_utf8_name_for_first_device,
        )
    });
    if !state.0 || device < 0 || device >= state.1 {
        return ptr::null();
    }
    FAKE_DEVICE_INFO.with(|info| {
        let mut info = info.borrow_mut();
        info.name = if device == 1 {
            TEST_DEVICE_NAME_1.as_ptr().cast()
        } else if state.3 {
            ptr::null()
        } else if state.4 {
            TEST_DEVICE_NAME_INVALID_UTF8.as_ptr().cast()
        } else if state.2 {
            TEST_DEVICE_NAME_PLUGIN.as_ptr().cast()
        } else {
            TEST_DEVICE_NAME_0.as_ptr().cast()
        };
        (&*info) as *const ffi::PaDeviceInfo
    })
}

unsafe extern "C" fn fake_pa_open_stream(
    stream: *mut *mut PaStream,
    input_parameters: *const PaStreamParameters,
    output_parameters: *const PaStreamParameters,
    sample_rate: f64,
    frames_per_buffer: std::ffi::c_ulong,
    _stream_flags: std::ffi::c_ulong,
    callback: Option<ffi::PaStreamCallback>,
    callback_context: *mut c_void,
) -> PaError {
    if stream.is_null() || input_parameters.is_null() == output_parameters.is_null() {
        return -1;
    }
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.open_count += 1;
        if let Some(input) = unsafe { input_parameters.as_ref() } {
            state.input_format = input.sample_format;
            state.input_latency = input.suggested_latency;
            state.capture_callback = callback;
            state.capture_context = callback_context;
        }
        if let Some(output) = unsafe { output_parameters.as_ref() } {
            state.output_format = output.sample_format;
            state.output_latency = output.suggested_latency;
            state.callback = callback;
            state.callback_context = callback_context;
        }
        state.sample_rate = sample_rate;
        state.frames_per_buffer = frames_per_buffer;
        let result = state.open_results.pop_front().unwrap_or(state.open_result);
        if result == ffi::PA_NO_ERROR {
            state.endpoint_active.push(0);
            unsafe {
                *stream = state.endpoint_active.len() as *mut PaStream;
            }
        }
        result
    })
}

unsafe extern "C" fn fake_thread_self() -> ffi::Pthread {
    FAKE_SCHEDULING.with(|state| state.borrow_mut().events.push("self"));
    42
}

unsafe extern "C" fn fake_get_scheduling(
    thread: ffi::Pthread,
    policy: *mut c_int,
    param: *mut ffi::SchedParam,
) -> c_int {
    assert_eq!(thread, 42);
    FAKE_SCHEDULING.with(|state| {
        let mut state = state.borrow_mut();
        state.events.push("get");
        unsafe {
            *policy = state.policy;
            (*param).sched_priority = state.priority;
        }
        state.get_result
    })
}

unsafe extern "C" fn fake_set_scheduling(
    thread: ffi::Pthread,
    policy: c_int,
    param: *const ffi::SchedParam,
) -> c_int {
    assert_eq!(thread, 42);
    let priority = unsafe { (*param).sched_priority };
    FAKE_SCHEDULING.with(|state| {
        let mut state = state.borrow_mut();
        state.events.push("set");
        state.set_requests.push((policy, priority));
        let result = state.set_results.pop_front().unwrap_or(0);
        if result == 0 {
            state.policy = policy;
            state.priority = priority;
        }
        result
    })
}

unsafe extern "C" fn fake_priority_max(policy: c_int) -> c_int {
    assert_eq!(policy, ffi::SCHED_FIFO);
    FAKE_SCHEDULING.with(|state| {
        let mut state = state.borrow_mut();
        state.events.push("max");
        state.maximum
    })
}

unsafe extern "C" fn fake_pa_start_stream(stream: *mut PaStream) -> PaError {
    FAKE_SCHEDULING.with(|state| {
        let mut state = state.borrow_mut();
        state.events.push("start");
        state.start_schedule = Some((state.policy, state.priority));
    });
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.start_count += 1;
        let result = state
            .start_results
            .pop_front()
            .unwrap_or(state.start_result);
        if result == ffi::PA_NO_ERROR {
            state.set_active(stream, 1);
        }
        result
    })
}

unsafe extern "C" fn fake_pa_stop_stream(stream: *mut PaStream) -> PaError {
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.stop_count += 1;
        if state.stop_result == ffi::PA_NO_ERROR {
            state.set_active(stream, 0);
        }
        state.stop_result
    })
}

unsafe extern "C" fn fake_pa_abort_stream(stream: *mut PaStream) -> PaError {
    FAKE_SCHEDULING.with(|state| state.borrow_mut().events.push("abort"));
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.abort_count += 1;
        if state.abort_result == ffi::PA_NO_ERROR {
            state.set_active(stream, 0);
        }
        state.abort_result
    })
}

unsafe extern "C" fn fake_pa_close_stream(stream: *mut PaStream) -> PaError {
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.close_count += 1;
        if state.close_result == 0 {
            state.set_active(stream, 0);
        }
        state.close_result
    })
}

unsafe extern "C" fn fake_pa_is_stream_active(stream: *mut PaStream) -> PaError {
    FAKE_PORTAUDIO.with(|state| state.borrow().endpoint_active[stream as usize - 1])
}

unsafe extern "C" fn fake_pa_get_stream_info(_stream: *mut PaStream) -> *const ffi::PaStreamInfo {
    let available = FAKE_PORTAUDIO.with(|state| state.borrow().stream_info_available);
    if !available {
        return ptr::null();
    }
    FAKE_STREAM_INFO.with(|info| (&*info.borrow()) as *const ffi::PaStreamInfo)
}

unsafe extern "C" fn fake_mixer_open(mixer: *mut *mut SndMixer, _mode: c_int) -> c_int {
    if mixer.is_null() {
        return -1;
    }
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.open_count += 1;
        if state.open_result == 0 {
            unsafe {
                *mixer = NonNull::<SndMixer>::dangling().as_ptr();
            }
        }
        state.open_result
    })
}

unsafe extern "C" fn fake_mixer_close(_mixer: *mut SndMixer) -> c_int {
    FAKE_ALSA.with(|state| state.borrow_mut().close_count += 1);
    0
}

unsafe extern "C" fn fake_mixer_attach(_mixer: *mut SndMixer, name: *const c_char) -> c_int {
    if name.is_null() {
        return -1;
    }
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.attach_count += 1;
        state.attached_card = unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned();
        state.attach_result
    })
}

unsafe extern "C" fn fake_mixer_selem_register(
    _mixer: *mut SndMixer,
    _options: *mut c_void,
    _class: *mut *mut c_void,
) -> c_int {
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.register_count += 1;
        state.register_result
    })
}

unsafe extern "C" fn fake_mixer_load(_mixer: *mut SndMixer) -> c_int {
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.load_count += 1;
        state.load_result
    })
}

unsafe extern "C" fn fake_mixer_handle_events(_mixer: *mut SndMixer) -> c_int {
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.handle_events_count += 1;
        state.handle_events_result
    })
}

unsafe extern "C" fn fake_selem_id_malloc(id: *mut *mut ffi::SndMixerSelemId) -> c_int {
    if id.is_null() {
        return -1;
    }
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.id_malloc_count += 1;
        if state.id_malloc_result == 0 {
            unsafe {
                *id = NonNull::<ffi::SndMixerSelemId>::dangling().as_ptr();
            }
        }
        state.id_malloc_result
    })
}

unsafe extern "C" fn fake_selem_id_free(_id: *mut ffi::SndMixerSelemId) {
    FAKE_ALSA.with(|state| state.borrow_mut().id_free_count += 1);
}

unsafe extern "C" fn fake_selem_id_set_name(_id: *mut ffi::SndMixerSelemId, _name: *const c_char) {}

unsafe extern "C" fn fake_selem_id_set_index(_id: *mut ffi::SndMixerSelemId, _index: u32) {}

unsafe extern "C" fn fake_mixer_find_selem(
    _mixer: *mut SndMixer,
    _id: *const ffi::SndMixerSelemId,
) -> *mut SndMixerElem {
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.find_count += 1;
        if state.element_available {
            NonNull::<SndMixerElem>::dangling().as_ptr()
        } else {
            ptr::null_mut()
        }
    })
}

fn fake_inventory_element(index: usize) -> *mut SndMixerElem {
    NonNull::<SndMixerElem>::dangling()
        .as_ptr()
        .wrapping_byte_add(index)
}

fn fake_inventory_index(element: *mut SndMixerElem) -> Option<usize> {
    let base = NonNull::<SndMixerElem>::dangling().as_ptr() as usize;
    (element as usize).checked_sub(base)
}

fn fake_inventory_contains(element: *mut SndMixerElem) -> bool {
    fake_inventory_index(element)
        .is_some_and(|index| FAKE_ALSA.with(|state| state.borrow().inventory.get(index).is_some()))
}

unsafe extern "C" fn fake_mixer_first_elem(_mixer: *mut SndMixer) -> *mut SndMixerElem {
    FAKE_ALSA.with(|state| {
        if state.borrow().inventory.is_empty() {
            ptr::null_mut()
        } else {
            fake_inventory_element(0)
        }
    })
}

unsafe extern "C" fn fake_mixer_elem_next(element: *mut SndMixerElem) -> *mut SndMixerElem {
    let Some(index) = fake_inventory_index(element) else {
        return ptr::null_mut();
    };
    FAKE_ALSA.with(|state| {
        if index + 1 < state.borrow().inventory.len() {
            fake_inventory_element(index + 1)
        } else {
            ptr::null_mut()
        }
    })
}

unsafe extern "C" fn fake_selem_is_active(element: *mut SndMixerElem) -> c_int {
    let Some(index) = fake_inventory_index(element) else {
        return 0;
    };
    FAKE_ALSA.with(|state| {
        i32::from(
            state
                .borrow()
                .inventory
                .get(index)
                .is_some_and(|entry| entry.active),
        )
    })
}

unsafe extern "C" fn fake_selem_get_name(element: *mut SndMixerElem) -> *const c_char {
    let Some(index) = fake_inventory_index(element) else {
        return ptr::null();
    };
    FAKE_ALSA.with(|state| {
        state
            .borrow()
            .inventory
            .get(index)
            .map_or(ptr::null(), |entry| entry.name.as_ptr())
    })
}

unsafe extern "C" fn fake_selem_get_index(element: *mut SndMixerElem) -> u32 {
    let Some(index) = fake_inventory_index(element) else {
        return 0;
    };
    FAKE_ALSA.with(|state| {
        state
            .borrow()
            .inventory
            .get(index)
            .map_or(0, |entry| entry.index)
    })
}

unsafe extern "C" fn fake_selem_has_capture_channel(
    element: *mut SndMixerElem,
    channel: c_int,
) -> c_int {
    let Some(index) = fake_inventory_index(element) else {
        return 0;
    };
    let Ok(channel) = usize::try_from(channel) else {
        return 0;
    };
    FAKE_ALSA.with(|state| {
        i32::from(
            state
                .borrow()
                .inventory
                .get(index)
                .and_then(|entry| entry.capture_channels.get(channel))
                .copied()
                .unwrap_or(false),
        )
    })
}

unsafe extern "C" fn fake_selem_has_playback_channel(
    element: *mut SndMixerElem,
    channel: c_int,
) -> c_int {
    let Some(index) = fake_inventory_index(element) else {
        return 0;
    };
    let Ok(channel) = usize::try_from(channel) else {
        return 0;
    };
    FAKE_ALSA.with(|state| {
        i32::from(
            state
                .borrow()
                .inventory
                .get(index)
                .and_then(|entry| entry.playback_channels.get(channel))
                .copied()
                .unwrap_or(false),
        )
    })
}

unsafe extern "C" fn fake_selem_has_capture_volume(element: *mut SndMixerElem) -> c_int {
    if let Some(index) = fake_inventory_index(element).filter(|_| fake_inventory_contains(element))
    {
        return FAKE_ALSA.with(|state| {
            i32::from(
                state
                    .borrow()
                    .inventory
                    .get(index)
                    .is_some_and(|entry| entry.capture_volume),
            )
        });
    }
    FAKE_ALSA.with(|state| i32::from(state.borrow().capture_volume_available))
}

unsafe extern "C" fn fake_selem_has_playback_volume(element: *mut SndMixerElem) -> c_int {
    if let Some(index) = fake_inventory_index(element).filter(|_| fake_inventory_contains(element))
    {
        return FAKE_ALSA.with(|state| {
            i32::from(
                state
                    .borrow()
                    .inventory
                    .get(index)
                    .is_some_and(|entry| entry.playback_volume),
            )
        });
    }
    FAKE_ALSA.with(|state| i32::from(state.borrow().playback_volume_available))
}

unsafe extern "C" fn fake_selem_has_capture_switch(element: *mut SndMixerElem) -> c_int {
    if let Some(index) = fake_inventory_index(element).filter(|_| fake_inventory_contains(element))
    {
        return FAKE_ALSA.with(|state| {
            i32::from(
                state
                    .borrow()
                    .inventory
                    .get(index)
                    .is_some_and(|entry| entry.capture_switch),
            )
        });
    }
    FAKE_ALSA.with(|state| i32::from(state.borrow().capture_switch_available))
}

unsafe extern "C" fn fake_selem_has_playback_switch(element: *mut SndMixerElem) -> c_int {
    if let Some(index) = fake_inventory_index(element).filter(|_| fake_inventory_contains(element))
    {
        return FAKE_ALSA.with(|state| {
            i32::from(
                state
                    .borrow()
                    .inventory
                    .get(index)
                    .is_some_and(|entry| entry.playback_switch),
            )
        });
    }
    FAKE_ALSA.with(|state| i32::from(state.borrow().playback_switch_available))
}

unsafe extern "C" fn fake_selem_get_capture_range(
    _element: *mut SndMixerElem,
    minimum: *mut std::ffi::c_long,
    maximum: *mut std::ffi::c_long,
) -> c_int {
    if minimum.is_null() || maximum.is_null() {
        return -1;
    }
    FAKE_ALSA.with(|state| {
        let result = state.borrow().capture_range_result;
        if result == 0 {
            unsafe {
                *minimum = -5_000;
                *maximum = 5_000;
            }
        }
        result
    })
}

unsafe extern "C" fn fake_selem_get_playback_range(
    _element: *mut SndMixerElem,
    minimum: *mut std::ffi::c_long,
    maximum: *mut std::ffi::c_long,
) -> c_int {
    if minimum.is_null() || maximum.is_null() {
        return -1;
    }
    FAKE_ALSA.with(|state| {
        let result = state.borrow().playback_range_result;
        if result == 0 {
            unsafe {
                *minimum = -5_000;
                *maximum = 5_000;
            }
        }
        result
    })
}

unsafe extern "C" fn fake_selem_get_capture_step_range(
    _element: *mut SndMixerElem,
    minimum: *mut std::ffi::c_long,
    maximum: *mut std::ffi::c_long,
) {
    if minimum.is_null() || maximum.is_null() {
        return;
    }
    FAKE_ALSA.with(|state| {
        let state = state.borrow();
        unsafe {
            *minimum = state.capture_step_minimum;
            *maximum = state.capture_step_maximum;
        }
    });
}

unsafe extern "C" fn fake_selem_get_playback_step_range(
    _element: *mut SndMixerElem,
    minimum: *mut std::ffi::c_long,
    maximum: *mut std::ffi::c_long,
) {
    if minimum.is_null() || maximum.is_null() {
        return;
    }
    FAKE_ALSA.with(|state| {
        let state = state.borrow();
        unsafe {
            *minimum = state.playback_step_minimum;
            *maximum = state.playback_step_maximum;
        }
    });
}

unsafe extern "C" fn fake_selem_get_capture_db(
    _element: *mut SndMixerElem,
    channel: c_int,
    value: *mut std::ffi::c_long,
) -> c_int {
    if value.is_null() {
        return -1;
    }
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.last_channel = channel;
        if state.capture_get_result == 0 {
            unsafe {
                *value = state.centibels;
            }
        }
        state.capture_get_result
    })
}

unsafe extern "C" fn fake_selem_get_playback_db(
    _element: *mut SndMixerElem,
    channel: c_int,
    value: *mut std::ffi::c_long,
) -> c_int {
    if value.is_null() {
        return -1;
    }
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.last_channel = channel;
        if state.playback_get_result == 0 {
            unsafe {
                *value = state.centibels;
            }
        }
        state.playback_get_result
    })
}

unsafe extern "C" fn fake_selem_set_capture_db(
    _element: *mut SndMixerElem,
    channel: c_int,
    value: std::ffi::c_long,
    _direction: c_int,
) -> c_int {
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.last_channel = channel;
        if state.capture_set_result == 0 {
            state.centibels = value;
        }
        state.capture_set_result
    })
}

unsafe extern "C" fn fake_selem_set_playback_db(
    _element: *mut SndMixerElem,
    channel: c_int,
    value: std::ffi::c_long,
    _direction: c_int,
) -> c_int {
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.last_channel = channel;
        if state.playback_set_result == 0 {
            state.centibels = value;
        }
        state.playback_set_result
    })
}

unsafe extern "C" fn fake_selem_get_capture_steps(
    _element: *mut SndMixerElem,
    channel: c_int,
    value: *mut std::ffi::c_long,
) -> c_int {
    if value.is_null() {
        return -1;
    }
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.last_channel = channel;
        if state.capture_step_get_result == 0 {
            unsafe {
                *value = state.steps;
            }
        }
        state.capture_step_get_result
    })
}

unsafe extern "C" fn fake_selem_get_playback_steps(
    _element: *mut SndMixerElem,
    channel: c_int,
    value: *mut std::ffi::c_long,
) -> c_int {
    if value.is_null() {
        return -1;
    }
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.last_channel = channel;
        if state.playback_step_get_result == 0 {
            unsafe {
                *value = state.steps;
            }
        }
        state.playback_step_get_result
    })
}

unsafe extern "C" fn fake_selem_set_capture_steps(
    _element: *mut SndMixerElem,
    channel: c_int,
    value: std::ffi::c_long,
) -> c_int {
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.last_channel = channel;
        if state.capture_step_set_result == 0 {
            state.steps = value;
        }
        state.capture_step_set_result
    })
}

unsafe extern "C" fn fake_selem_set_playback_steps(
    _element: *mut SndMixerElem,
    channel: c_int,
    value: std::ffi::c_long,
) -> c_int {
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.last_channel = channel;
        if state.playback_step_set_result == 0 {
            state.steps = value;
        }
        state.playback_step_set_result
    })
}

unsafe extern "C" fn fake_selem_get_capture_switch(
    _element: *mut SndMixerElem,
    channel: c_int,
    value: *mut c_int,
) -> c_int {
    if value.is_null() {
        return -1;
    }
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.last_channel = channel;
        if state.capture_switch_get_result == 0 {
            unsafe {
                *value = i32::from(state.switch_enabled);
            }
        }
        state.capture_switch_get_result
    })
}

unsafe extern "C" fn fake_selem_get_playback_switch(
    _element: *mut SndMixerElem,
    channel: c_int,
    value: *mut c_int,
) -> c_int {
    if value.is_null() {
        return -1;
    }
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.last_channel = channel;
        if state.playback_switch_get_result == 0 {
            unsafe {
                *value = i32::from(state.switch_enabled);
            }
        }
        state.playback_switch_get_result
    })
}

unsafe extern "C" fn fake_selem_set_capture_switch(
    _element: *mut SndMixerElem,
    channel: c_int,
    value: c_int,
) -> c_int {
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.last_channel = channel;
        if state.capture_switch_set_result == 0 {
            state.switch_enabled = value != 0;
        }
        state.capture_switch_set_result
    })
}

unsafe extern "C" fn fake_selem_set_playback_switch(
    _element: *mut SndMixerElem,
    channel: c_int,
    value: c_int,
) -> c_int {
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.last_channel = channel;
        if state.playback_switch_set_result == 0 {
            state.switch_enabled = value != 0;
        }
        state.playback_switch_set_result
    })
}

static TEST_FUNCTIONS: ffi::FunctionTable = ffi::FunctionTable {
    scheduling: ffi::SchedulingFunctions {
        thread_self: fake_thread_self,
        get: fake_get_scheduling,
        set: fake_set_scheduling,
        priority_max: fake_priority_max,
    },
    portaudio: ffi::PortAudioFunctions {
        initialize: fake_pa_initialize,
        terminate: fake_pa_terminate,
        get_device_count: fake_pa_get_device_count,
        get_default_input_device: fake_pa_get_default_input_device,
        get_default_output_device: fake_pa_get_default_output_device,
        get_device_info: fake_pa_get_device_info,
        open_stream: fake_pa_open_stream,
        start_stream: fake_pa_start_stream,
        stop_stream: fake_pa_stop_stream,
        abort_stream: fake_pa_abort_stream,
        close_stream: fake_pa_close_stream,
        is_stream_active: fake_pa_is_stream_active,
        get_stream_info: fake_pa_get_stream_info,
    },
    alsa: ffi::AlsaFunctions {
        mixer_open: fake_mixer_open,
        mixer_close: fake_mixer_close,
        mixer_attach: fake_mixer_attach,
        mixer_selem_register: fake_mixer_selem_register,
        mixer_load: fake_mixer_load,
        mixer_handle_events: fake_mixer_handle_events,
        selem_id_malloc: fake_selem_id_malloc,
        selem_id_free: fake_selem_id_free,
        selem_id_set_name: fake_selem_id_set_name,
        selem_id_set_index: fake_selem_id_set_index,
        mixer_find_selem: fake_mixer_find_selem,
        mixer_first_elem: fake_mixer_first_elem,
        mixer_elem_next: fake_mixer_elem_next,
        selem_is_active: fake_selem_is_active,
        selem_get_name: fake_selem_get_name,
        selem_get_index: fake_selem_get_index,
        selem_has_capture_volume: fake_selem_has_capture_volume,
        selem_has_playback_volume: fake_selem_has_playback_volume,
        selem_has_capture_switch: fake_selem_has_capture_switch,
        selem_has_playback_switch: fake_selem_has_playback_switch,
        selem_has_capture_channel: fake_selem_has_capture_channel,
        selem_has_playback_channel: fake_selem_has_playback_channel,
        selem_get_capture_db_range: fake_selem_get_capture_range,
        selem_get_playback_db_range: fake_selem_get_playback_range,
        selem_get_capture_volume_range: fake_selem_get_capture_step_range,
        selem_get_playback_volume_range: fake_selem_get_playback_step_range,
        selem_get_capture_db: fake_selem_get_capture_db,
        selem_get_playback_db: fake_selem_get_playback_db,
        selem_set_capture_db: fake_selem_set_capture_db,
        selem_set_playback_db: fake_selem_set_playback_db,
        selem_get_capture_volume: fake_selem_get_capture_steps,
        selem_get_playback_volume: fake_selem_get_playback_steps,
        selem_set_capture_volume: fake_selem_set_capture_steps,
        selem_set_playback_volume: fake_selem_set_playback_steps,
        selem_get_capture_switch: fake_selem_get_capture_switch,
        selem_get_playback_switch: fake_selem_get_playback_switch,
        selem_set_capture_switch: fake_selem_set_capture_switch,
        selem_set_playback_switch: fake_selem_set_playback_switch,
    },
};

fn fake_invoke_callback(input: &[f32], output: &mut [f32]) -> c_int {
    let (capture, capture_context) = FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        (state.capture_callback.unwrap(), state.capture_context)
    });
    assert_eq!(
        unsafe {
            capture(
                input.as_ptr().cast(),
                ptr::null_mut(),
                input.len() as _,
                ptr::null(),
                0,
                capture_context,
            )
        },
        ffi::PA_CONTINUE
    );
    let (callback, context) = FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        (
            state.callback.expect("fake stream has a callback"),
            state.callback_context,
        )
    });
    unsafe {
        callback(
            input.as_ptr().cast(),
            output.as_mut_ptr().cast(),
            input.len() as std::ffi::c_ulong,
            ptr::null(),
            0,
            context,
        )
    }
}

fn ffi_stream_config() -> StreamConfig {
    StreamConfig {
        struct_size: size_of::<StreamConfig>() as u32,
        abi_version: ABI_VERSION,
        native_sample_rate_hz: 48_000,
        maximum_frame_count: 2,
        input_device_index: DEFAULT_DEVICE,
        output_device_index: DEFAULT_DEVICE,
        input_device_channels: 1,
        output_device_channels: 1,
        native_tick: Some(copy_input_to_output),
        native_tick_context: ptr::null_mut(),
    }
}

fn ffi_stream_timing() -> StreamTiming {
    StreamTiming {
        struct_size: size_of::<StreamTiming>() as u32,
        ..StreamTiming::default()
    }
}

fn ffi_mixer_config(direction: u32, channel: u32) -> MixerConfig {
    MixerConfig {
        struct_size: size_of::<MixerConfig>() as u32,
        card: c"default".as_ptr(),
        element: c"Capture".as_ptr(),
        element_index: 0,
        channel,
        direction,
    }
}

fn ffi_usb_mixer_config(direction: u32, channel: u32) -> UsbMixerConfig {
    UsbMixerConfig {
        struct_size: size_of::<UsbMixerConfig>() as u32,
        usb_interface_path: c"3-1:1.0".as_ptr(),
        element: c"Capture".as_ptr(),
        element_index: 0,
        channel,
        direction,
    }
}

fn ffi_usb_device_identity(
    usb_interface_path: *const c_char,
    usb_serial: *const c_char,
) -> UsbDeviceIdentity {
    UsbDeviceIdentity {
        struct_size: size_of::<UsbDeviceIdentity>() as u32,
        usb_interface_path,
        usb_serial,
        input_device_channels: 1,
        output_device_channels: 1,
    }
}

fn ffi_usb_device_selector(
    selection_policy: u32,
    device_identifier: *const c_char,
    usb_serial: *const c_char,
) -> UsbDeviceSelector {
    UsbDeviceSelector {
        struct_size: size_of::<UsbDeviceSelector>() as u32,
        selection_policy,
        device_identifier,
        usb_serial,
        input_device_channels: 1,
        output_device_channels: 1,
    }
}

fn ffi_usb_device_match() -> UsbDeviceMatch {
    UsbDeviceMatch {
        struct_size: size_of::<UsbDeviceMatch>() as u32,
        ..UsbDeviceMatch::default()
    }
}

fn ffi_cm119_mixer_paths() -> Cm119MixerPaths {
    Cm119MixerPaths {
        struct_size: size_of::<Cm119MixerPaths>() as u32,
        ..Cm119MixerPaths::default()
    }
}

fn cm119_path_element(path: &Cm119MixerPath) -> &str {
    unsafe { CStr::from_ptr(path.element.as_ptr()) }
        .to_str()
        .expect("test mixer element is UTF-8")
}

fn identity_component(value: &[c_char]) -> &str {
    unsafe { CStr::from_ptr(value.as_ptr()) }
        .to_str()
        .expect("test identity is UTF-8")
}

#[cfg(unix)]
fn create_test_sysfs_root(label: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "rptadv-portaudio-alsa-adapter-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time after Unix epoch")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("create test sysfs root");
    root
}

#[cfg(unix)]
fn create_test_sysfs_card(root: &Path, card_index: u32, usb_interface_path: &str) {
    create_test_sysfs_card_with_serial(root, card_index, usb_interface_path, None);
}

#[cfg(unix)]
fn create_test_sysfs_card_with_serial(
    root: &Path,
    card_index: u32,
    usb_interface_path: &str,
    serial: Option<&str>,
) {
    let topology = usb_interface_path
        .split_once(':')
        .map_or(usb_interface_path, |(topology, _)| topology);
    let bus = topology.split('-').next().expect("USB topology has a bus");
    let usb_device = root
        .join("devices")
        .join(format!("usb{bus}"))
        .join(topology);
    fs::create_dir_all(&usb_device).expect("create test USB device");
    if let Some(serial) = serial {
        fs::write(usb_device.join("serial"), format!("{serial}\n")).expect("write test USB serial");
    }
    let target = usb_device
        .join(usb_interface_path)
        .join("sound")
        .join(format!("card{card_index}"));
    fs::create_dir_all(&target).expect("create test sound-device target");
    let card = root.join(format!("card{card_index}"));
    fs::create_dir_all(&card).expect("create test sound-card entry");
    symlink(&target, card.join("device")).expect("link test sound device");
}

fn fake_stream(config: &StreamConfig) -> *mut AudioStream {
    let mut stream = ptr::null_mut();
    assert_eq!(
        stream_create_with_functions(&TEST_FUNCTIONS, config, &mut stream),
        AUDIO_OK
    );
    assert!(!stream.is_null());
    stream
}

fn fake_mixer(config: &MixerConfig) -> *mut AudioMixer {
    let mut mixer = ptr::null_mut();
    assert_eq!(
        mixer_create_with_functions(&TEST_FUNCTIONS, config, &mut mixer),
        AUDIO_OK
    );
    assert!(!mixer.is_null());
    mixer
}

#[test]
fn validate_stream_config_rejects_missing_tick() {
    let config = StreamConfig {
        struct_size: size_of::<StreamConfig>() as u32,
        abi_version: ABI_VERSION,
        native_sample_rate_hz: 48_000,
        maximum_frame_count: 960,
        input_device_index: DEFAULT_DEVICE,
        output_device_index: DEFAULT_DEVICE,
        input_device_channels: 1,
        output_device_channels: 1,
        native_tick: None,
        native_tick_context: ptr::null_mut(),
    };

    assert!(matches!(
        unsafe { ValidatedStreamConfig::from_ffi(&config) },
        Err(AUDIO_INVALID_ARGUMENT)
    ));
}

#[test]
fn validate_stream_config_rejects_every_incompatible_abi_field() {
    let mut config = ffi_stream_config();
    assert!(matches!(
        unsafe { ValidatedStreamConfig::from_ffi(ptr::null()) },
        Err(AUDIO_INVALID_ARGUMENT)
    ));

    let mut reject = |mutate: fn(&mut StreamConfig)| {
        config = ffi_stream_config();
        mutate(&mut config);
        assert!(matches!(
            unsafe { ValidatedStreamConfig::from_ffi(&config) },
            Err(AUDIO_INVALID_ARGUMENT)
        ));
    };
    reject(|value| value.struct_size = 0);
    reject(|value| value.abi_version = ABI_VERSION + 1);
    reject(|value| value.native_sample_rate_hz = 0);
    reject(|value| value.maximum_frame_count = 0);
    reject(|value| value.input_device_channels = 0);
    reject(|value| value.input_device_channels = 3);
    reject(|value| value.output_device_channels = 0);
    reject(|value| value.output_device_channels = 3);
    reject(|value| value.input_device_index = DEFAULT_DEVICE - 1);
    reject(|value| value.output_device_index = DEFAULT_DEVICE - 1);
}

#[test]
fn validate_stream_config_preserves_valid_configuration() {
    let mut config = ffi_stream_config();
    config.native_sample_rate_hz = 96_000;
    config.maximum_frame_count = 1_024;
    config.input_device_index = 3;
    config.output_device_index = 4;
    config.input_device_channels = 2;
    config.output_device_channels = 2;
    let validated = unsafe { ValidatedStreamConfig::from_ffi(&config) }.unwrap();

    assert_eq!(validated.sample_rate_hz, 96_000);
    assert_eq!(validated.maximum_frame_count, 1_024);
    assert_eq!(validated.input_device_index, 3);
    assert_eq!(validated.output_device_index, 4);
    assert_eq!(validated.input_channels, 2);
    assert_eq!(validated.output_channels, 2);
}

#[test]
fn descriptor_has_the_expected_contract() {
    let descriptor = rptadv_portaudio_alsa_adapter_descriptor();
    assert!(!descriptor.is_null());
    let descriptor = unsafe { &*descriptor };
    assert_eq!(descriptor.abi_version, ABI_VERSION);
    assert_eq!(
        descriptor.struct_size as usize,
        size_of::<AdapterDescriptor>()
    );
    assert_eq!(
        unsafe { CStr::from_ptr(descriptor.capability_name) }.to_bytes(),
        b"rptadv.portaudio-alsa-audio"
    );
    assert!(descriptor.mixer_create_for_usb_interface as usize != 0);
    assert!(descriptor.mixer_get_range_steps as usize != 0);
    assert!(descriptor.mixer_get_steps as usize != 0);
    assert!(descriptor.mixer_set_steps as usize != 0);
    assert!(descriptor.mixer_get_normalized as usize != 0);
    assert!(descriptor.mixer_set_normalized as usize != 0);
    assert!(descriptor.mixer_get_switch as usize != 0);
    assert!(descriptor.mixer_set_switch as usize != 0);
    assert!(descriptor.usb_device_resolve as usize != 0);
    assert!(descriptor.usb_device_select as usize != 0);
    assert!(descriptor.cm119_mixer_paths_resolve as usize != 0);
}

#[test]
fn function_table_exercises_f32_stream_lifecycle_without_hardware() {
    let _serial = lock_fake();
    reset_fake_functions();
    let config = ffi_stream_config();
    let mut stream = ptr::null_mut();

    assert_eq!(
        stream_create_with_functions(&TEST_FUNCTIONS, &config, &mut stream),
        AUDIO_OK
    );
    assert!(!stream.is_null());
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.initialize_count, 1);
        assert_eq!(state.open_count, 2);
        assert_ne!(state.capture_context, state.callback_context);
        assert_eq!(state.input_format, ffi::PA_FLOAT_32);
        assert_eq!(state.output_format, ffi::PA_FLOAT_32);
        assert_eq!(state.input_latency, 0.004);
        assert_eq!(state.output_latency, 0.006);
        assert_eq!(state.sample_rate, 48_000.0);
        assert_eq!(state.frames_per_buffer, 2);
    });

    assert_eq!(stream_start(stream), AUDIO_OK);
    let input = [0.123_456_7, -0.625];
    let mut output = [0.0; 2];
    assert_eq!(fake_invoke_callback(&input, &mut output), ffi::PA_CONTINUE);
    // Playback starts on time; initial capture priming is explicit silence.
    assert_eq!(output, [0.0; 2]);
    let mut stats = StreamStats {
        struct_size: size_of::<StreamStats>() as u32,
        ..StreamStats::default()
    };
    assert_eq!(stream_get_stats(stream, &mut stats), AUDIO_OK);
    assert_eq!(stats.capture_callback_count, 1);
    assert_eq!(stats.capture_startup_wait_frames, 2);
    assert_eq!(stats.capture_ring_missing_frames, 0);
    assert_eq!(stream_stop(stream), AUDIO_OK);
    stream_destroy(stream);

    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.start_count, 2);
        assert_eq!(state.stop_count, 2);
        assert_eq!(state.close_count, 2);
        assert_eq!(state.terminate_count, 1);
    });
}

#[test]
fn function_table_exercises_alsa_mixer_without_hardware() {
    let _serial = lock_fake();
    reset_fake_functions();
    let config = ffi_mixer_config(0, 0);
    let mixer = fake_mixer(&config);
    let mut minimum = 0;
    let mut maximum = 0;
    assert_eq!(
        mixer_get_range_centibels(mixer, &mut minimum, &mut maximum),
        AUDIO_OK
    );
    assert_eq!((minimum, maximum), (-5_000, 5_000));
    let mut value = 0;
    assert_eq!(mixer_get_centibels(mixer, &mut value), AUDIO_OK);
    assert_eq!(value, -1_200);
    assert_eq!(mixer_set_centibels(mixer, -2_500), AUDIO_OK);
    assert_eq!(mixer_get_centibels(mixer, &mut value), AUDIO_OK);
    assert_eq!(value, -2_500);
    assert_eq!(
        mixer_get_range_steps(mixer, &mut minimum, &mut maximum),
        AUDIO_OK
    );
    assert_eq!((minimum, maximum), (0, 31));
    assert_eq!(mixer_get_steps(mixer, &mut value), AUDIO_OK);
    assert_eq!(value, 18);
    assert_eq!(mixer_set_steps(mixer, 27), AUDIO_OK);
    assert_eq!(mixer_get_steps(mixer, &mut value), AUDIO_OK);
    assert_eq!(value, 27);
    let mut normalized = 0;
    assert_eq!(mixer_get_normalized(mixer, &mut normalized), AUDIO_OK);
    assert_eq!(normalized, 870);
    assert_eq!(mixer_set_normalized(mixer, 500), AUDIO_OK);
    assert_eq!(mixer_get_steps(mixer, &mut value), AUDIO_OK);
    assert_eq!(value, 16);
    let mut enabled = 0;
    assert_eq!(mixer_get_switch(mixer, &mut enabled), AUDIO_OK);
    assert_eq!(enabled, 1);
    assert_eq!(mixer_set_switch(mixer, 0), AUDIO_OK);
    assert_eq!(mixer_get_switch(mixer, &mut enabled), AUDIO_OK);
    assert_eq!(enabled, 0);
    mixer_destroy(mixer);
    FAKE_ALSA.with(|state| assert_eq!(state.borrow().close_count, 1));
}

#[test]
fn normalized_mixer_mapping_rounds_between_exact_native_endpoints() {
    assert_eq!(normalized_from_steps(0, 0, 31), Ok(0));
    assert_eq!(normalized_from_steps(31, 0, 31), Ok(999));
    assert_eq!(normalized_from_steps(16, 0, 31), Ok(516));
    assert_eq!(steps_from_normalized(0, 0, 31), Ok(0));
    assert_eq!(steps_from_normalized(999, 0, 31), Ok(31));
    assert_eq!(steps_from_normalized(500, 0, 31), Ok(16));
    assert_eq!(normalized_from_steps(-1, 0, 31), Err(AUDIO_ALSA_ERROR));
    assert_eq!(normalized_from_steps(32, 0, 31), Err(AUDIO_ALSA_ERROR));
    assert_eq!(normalized_from_steps(0, 4, 4), Err(AUDIO_UNSUPPORTED));
    assert_eq!(
        steps_from_normalized(1_000, 0, 31),
        Err(AUDIO_INVALID_ARGUMENT)
    );
    assert_eq!(steps_from_normalized(0, 4, 4), Err(AUDIO_UNSUPPORTED));
}

#[cfg(unix)]
#[test]
fn usb_interface_mixer_resolution_uses_the_exact_sysfs_card() {
    let _serial = lock_fake();
    reset_fake_functions();
    let root = create_test_sysfs_root("resolve");
    create_test_sysfs_card(&root, 4, "3-1:1.0");
    create_test_sysfs_card(&root, 7, "7-2:1.0");
    let config = ffi_usb_mixer_config(0, 0);
    let mut mixer = ptr::null_mut();

    assert_eq!(
        mixer_create_for_usb_interface_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &config,
            &mut mixer,
        ),
        AUDIO_OK
    );
    assert!(!mixer.is_null());
    FAKE_ALSA.with(|state| assert_eq!(state.borrow().attached_card, "hw:4"));
    mixer_destroy(mixer);
    fs::remove_dir_all(root).expect("remove test sysfs root");
}

#[cfg(unix)]
#[test]
fn usb_interface_mixer_resolution_rejects_missing_or_ambiguous_cards() {
    let _serial = lock_fake();
    reset_fake_functions();
    let missing_root = create_test_sysfs_root("missing");
    assert_eq!(
        resolve_alsa_card_index_from_sysfs(&missing_root, "3-1:1.0"),
        Err(AUDIO_UNSUPPORTED)
    );
    fs::remove_dir_all(&missing_root).expect("remove missing sysfs root");
    assert_eq!(
        resolve_alsa_card_index_from_sysfs(&missing_root, "3-1:1.0"),
        Err(AUDIO_UNSUPPORTED)
    );

    let ambiguous_root = create_test_sysfs_root("ambiguous");
    create_test_sysfs_card(&ambiguous_root, 1, "3-1:1.0");
    create_test_sysfs_card(&ambiguous_root, 2, "3-1:1.0");
    assert_eq!(
        resolve_alsa_card_index_from_sysfs(&ambiguous_root, "3-1:1.0"),
        Err(AUDIO_UNSUPPORTED)
    );
    fs::remove_dir_all(ambiguous_root).expect("remove ambiguous sysfs root");
}

#[cfg(unix)]
#[test]
fn usb_device_identity_resolves_one_matching_card_and_raw_portaudio_device() {
    let _serial = lock_fake();
    reset_fake_functions();
    let root = create_test_sysfs_root("device-identity");
    create_test_sysfs_card_with_serial(&root, 4, "3-1:1.0", Some("CM119-A"));
    create_test_sysfs_card_with_serial(&root, 7, "7-2:1.0", Some("OTHER"));
    let identity = ffi_usb_device_identity(c"3-1:1.0".as_ptr(), c"CM119-A".as_ptr());
    let mut selection = UsbDeviceSelection {
        struct_size: size_of::<UsbDeviceSelection>() as u32,
        ..UsbDeviceSelection::default()
    };

    assert_eq!(
        usb_device_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &identity,
            &mut selection,
        ),
        AUDIO_OK
    );
    assert_eq!(selection.abi_version, ABI_VERSION);
    assert_eq!(selection.alsa_card_index, 4);
    assert_eq!(selection.input_device_index, 0);
    assert_eq!(selection.output_device_index, 0);
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.initialize_count, 1);
        assert_eq!(state.terminate_count, 1);
    });

    let serial_only = ffi_usb_device_identity(ptr::null(), c"CM119-A".as_ptr());
    assert_eq!(
        usb_device_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &serial_only,
            &mut selection,
        ),
        AUDIO_OK
    );
    assert_eq!(selection.alsa_card_index, 4);

    let topology_only = ffi_usb_device_identity(c"3-1".as_ptr(), ptr::null());
    assert_eq!(
        usb_device_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &topology_only,
            &mut selection,
        ),
        AUDIO_OK
    );
    assert_eq!(selection.alsa_card_index, 4);

    let mismatch = ffi_usb_device_identity(c"3-1:1.0".as_ptr(), c"OTHER".as_ptr());
    assert_eq!(
        usb_device_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &mismatch,
            &mut selection,
        ),
        AUDIO_UNSUPPORTED
    );
    fs::remove_dir_all(root).expect("remove device identity sysfs root");
}

#[test]
fn raw_alsa_name_parser_accepts_native_names_and_rejects_aliases() {
    assert_eq!(
        raw_alsa_device_from_portaudio_name("CM119 (hw:4,0)"),
        Some(RawAlsaDevice {
            card_index: 4,
            pcm_device: 0,
        })
    );
    assert_eq!(
        raw_alsa_device_from_portaudio_name("hw:7,12"),
        Some(RawAlsaDevice {
            card_index: 7,
            pcm_device: 12,
        })
    );
    assert_eq!(
        raw_alsa_device_from_portaudio_name("CM119 (plughw:4,0)"),
        None
    );
    assert_eq!(raw_alsa_device_from_portaudio_name("xhw:4,0"), None);
    assert_eq!(raw_alsa_device_from_portaudio_name("hw:4,x"), None);
    assert_eq!(
        raw_alsa_device_from_portaudio_name("hw:x,0 CM119 (hw:4,0)"),
        Some(RawAlsaDevice {
            card_index: 4,
            pcm_device: 0,
        })
    );
    assert_eq!(raw_alsa_device_from_portaudio_name("hw:4,0x"), None);
    assert_eq!(raw_alsa_device_from_portaudio_name("hw:4"), None);
}

#[test]
fn legacy_hw_selector_parser_accepts_exact_native_identifiers() {
    assert_eq!(
        parse_legacy_hw_selector("hw:4"),
        Ok(LegacyHwSelector {
            card_index: 4,
            pcm_device: None,
        })
    );
    assert_eq!(
        parse_legacy_hw_selector("hw:4,12"),
        Ok(LegacyHwSelector {
            card_index: 4,
            pcm_device: Some(12),
        })
    );
    assert_eq!(parse_legacy_hw_selector("hw:"), Err(AUDIO_INVALID_ARGUMENT));
    assert_eq!(
        parse_legacy_hw_selector("hw:4,"),
        Err(AUDIO_INVALID_ARGUMENT)
    );
    assert_eq!(
        parse_legacy_hw_selector("hw:4x"),
        Err(AUDIO_INVALID_ARGUMENT)
    );
    assert_eq!(
        parse_legacy_hw_selector("hw:4,12extra"),
        Err(AUDIO_INVALID_ARGUMENT)
    );
    assert_eq!(
        parse_legacy_hw_selector("plughw:4,0"),
        Err(AUDIO_INVALID_ARGUMENT)
    );
    assert_eq!(
        parse_legacy_hw_selector("hw:4294967296"),
        Err(AUDIO_INVALID_ARGUMENT)
    );
    assert_eq!(parse_decimal_prefix(""), None);
    assert_eq!(parse_decimal_prefix("12x"), Some((12, "x")));
}

#[test]
fn usb_identity_component_and_output_copy_validate_boundaries() {
    assert!(looks_like_usb_interface_component("3-1:1.0"));
    assert!(looks_like_usb_interface_component("1-1.2:2.0"));
    assert!(!looks_like_usb_interface_component("3-1"));
    assert!(!looks_like_usb_interface_component(":1.0"));
    assert!(!looks_like_usb_interface_component("3-1:"));
    assert!(!looks_like_usb_interface_component("0000:00:14.0"));
    assert!(!looks_like_usb_interface_component("3-x:1.0"));
    assert!(!looks_like_usb_interface_component("3-1:x"));
    assert_eq!(
        usb_interface_path_from_device_path(Path::new("/sys/devices/no-usb-interface")),
        None
    );

    let mut copied = [0; 4];
    assert_eq!(copy_usb_identity_component(&mut copied, "abc"), Ok(()));
    assert_eq!(identity_component(&copied), "abc");
    assert_eq!(
        copy_usb_identity_component(&mut copied, "abcd"),
        Err(AUDIO_UNSUPPORTED)
    );
}

#[cfg(unix)]
#[test]
fn usb_device_selector_maps_legacy_identifiers_and_returns_stable_identity() {
    let _serial = lock_fake();
    reset_fake_functions();
    let root = create_test_sysfs_root("device-selector");
    create_test_sysfs_card_with_serial(&root, 4, "3-1:1.0", Some("CM119-A"));
    create_test_sysfs_card_with_serial(&root, 7, "7-2:1.0", Some("OTHER"));
    let selector = ffi_usb_device_selector(USB_SELECTION_EXACT, c"hw:7,0".as_ptr(), ptr::null());
    let mut device_match = ffi_usb_device_match();

    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &selector,
            &mut device_match,
        ),
        AUDIO_OK
    );
    assert_eq!(device_match.abi_version, ABI_VERSION);
    assert_eq!(
        identity_component(&device_match.usb_interface_path),
        "7-2:1.0"
    );
    assert_eq!(identity_component(&device_match.usb_serial), "OTHER");
    assert_eq!(device_match.selection.abi_version, ABI_VERSION);
    assert_eq!(
        device_match.selection.struct_size as usize,
        size_of::<UsbDeviceSelection>()
    );
    assert_eq!(device_match.selection.alsa_card_index, 7);
    assert_eq!(device_match.selection.input_device_index, 1);
    assert_eq!(device_match.selection.output_device_index, 1);

    let topology_and_serial =
        ffi_usb_device_selector(USB_SELECTION_EXACT, c"3-1".as_ptr(), c"CM119-A".as_ptr());
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &topology_and_serial,
            &mut device_match,
        ),
        AUDIO_OK
    );
    assert_eq!(device_match.selection.alsa_card_index, 4);
    assert_eq!(
        identity_component(&device_match.usb_interface_path),
        "3-1:1.0"
    );
    assert_eq!(identity_component(&device_match.usb_serial), "CM119-A");

    fs::remove_dir_all(root).expect("remove device selector sysfs root");
}

#[cfg(unix)]
#[test]
fn usb_device_selector_uses_lowest_usable_card_and_rejects_conflicts() {
    let _serial = lock_fake();
    reset_fake_functions();
    let root = create_test_sysfs_root("device-selector-automatic");
    create_test_sysfs_card_with_serial(&root, 4, "3-1:1.0", Some("CM119-A"));
    create_test_sysfs_card_with_serial(&root, 7, "7-2:1.0", Some("OTHER"));
    let automatic = ffi_usb_device_selector(
        USB_SELECTION_AUTOMATIC_LOWEST_ALSA_CARD,
        ptr::null(),
        ptr::null(),
    );
    let mut device_match = ffi_usb_device_match();

    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &automatic,
            &mut device_match,
        ),
        AUDIO_OK
    );
    assert_eq!(device_match.selection.alsa_card_index, 4);

    let mismatched =
        ffi_usb_device_selector(USB_SELECTION_EXACT, c"hw:4".as_ptr(), c"OTHER".as_ptr());
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &mismatched,
            &mut device_match,
        ),
        AUDIO_UNSUPPORTED
    );
    assert_eq!(device_match.abi_version, 0);

    let unavailable_pcm =
        ffi_usb_device_selector(USB_SELECTION_EXACT, c"hw:7,1".as_ptr(), ptr::null());
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &unavailable_pcm,
            &mut device_match,
        ),
        AUDIO_UNSUPPORTED
    );
    fs::remove_dir_all(root).expect("remove automatic selector sysfs root");
}

#[cfg(unix)]
#[test]
fn usb_card_inventory_filters_non_usb_entries_and_returns_stable_order() {
    let root = create_test_sysfs_root("usb-card-inventory");
    create_test_sysfs_card_with_serial(&root, 7, "7-2:1.0", Some("OTHER"));
    create_test_sysfs_card(&root, 4, "3-1:1.0");
    create_test_sysfs_card_with_serial(&root, 6, "6-1:1.0", Some(""));
    fs::create_dir(root.join("card8")).expect("create unusable sound-card entry");
    fs::create_dir(root.join(std::ffi::OsString::from_vec(vec![
        b'c', b'a', b'r', b'd', 0xff,
    ])))
    .expect("create non-UTF-8 sound-card entry");
    let non_usb_target = root
        .join("devices")
        .join("non-usb")
        .join("sound")
        .join("card9");
    fs::create_dir_all(&non_usb_target).expect("create non-USB card target");
    let non_usb_card = root.join("card9");
    fs::create_dir_all(&non_usb_card).expect("create non-USB sound-card entry");
    symlink(&non_usb_target, non_usb_card.join("device")).expect("link non-USB card target");

    let cards = enumerate_usb_cards_from_sysfs(&root).expect("enumerate USB sound cards");
    assert_eq!(cards.len(), 3);
    assert_eq!(cards[0].card_index, 4);
    assert_eq!(cards[0].usb_interface_path, "3-1:1.0");
    assert_eq!(cards[0].usb_serial, None);
    assert_eq!(cards[1].card_index, 6);
    assert_eq!(cards[1].usb_serial, None);
    assert_eq!(cards[2].card_index, 7);
    assert_eq!(cards[2].usb_serial.as_deref(), Some("OTHER"));
    assert_eq!(
        resolve_usb_card_from_alsa_card(&root, 8),
        Err(AUDIO_UNSUPPORTED)
    );
    fs::remove_dir_all(&root).expect("remove USB-card inventory root");
    assert_eq!(
        enumerate_usb_cards_from_sysfs(&root),
        Err(AUDIO_UNSUPPORTED)
    );
    assert_eq!(
        resolve_usb_card_from_alsa_card(&root, 4),
        Err(AUDIO_UNSUPPORTED)
    );
}

#[cfg(unix)]
#[test]
fn usb_device_selector_supports_serial_only_and_empty_returned_serial() {
    let _serial = lock_fake();
    reset_fake_functions();
    let root = create_test_sysfs_root("device-selector-serial");
    create_test_sysfs_card_with_serial(&root, 4, "3-1:1.0", Some("CM119-A"));
    let serial_only =
        ffi_usb_device_selector(USB_SELECTION_EXACT, ptr::null(), c"CM119-A".as_ptr());
    let mut device_match = ffi_usb_device_match();

    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &serial_only,
            &mut device_match,
        ),
        AUDIO_OK
    );
    assert_eq!(device_match.selection.alsa_card_index, 4);

    fs::remove_dir_all(&root).expect("replace selector serial root");
    let root = create_test_sysfs_root("device-selector-no-serial");
    create_test_sysfs_card(&root, 4, "3-1:1.0");
    let hw_only = ffi_usb_device_selector(USB_SELECTION_EXACT, c"hw:4".as_ptr(), ptr::null());
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &hw_only,
            &mut device_match,
        ),
        AUDIO_OK
    );
    assert_eq!(identity_component(&device_match.usb_serial), "");
    fs::remove_dir_all(root).expect("remove selector no-serial root");
}

#[cfg(unix)]
#[test]
fn usb_device_identity_never_inherits_an_upstream_hub_serial() {
    let _serial = lock_fake();
    reset_fake_functions();
    let root = create_test_sysfs_root("device-identity-hub-serial");
    create_test_sysfs_card(&root, 4, "3-1:1.0");
    let hub = root.join("devices").join("usb3");
    fs::write(hub.join("serial"), "xhci-hcd.1\n").expect("write upstream hub serial");
    let device_path =
        fs::canonicalize(root.join("card4").join("device")).expect("resolve test sound device");
    assert_eq!(usb_serial_from_device_path(&device_path), None);
    assert_eq!(usb_serial_from_device_path(&hub), None);
    assert_eq!(
        resolve_alsa_card_index_from_sysfs_identity(&root, None, Some("xhci-hcd.1")),
        Err(AUDIO_UNSUPPORTED)
    );
    let automatic = ffi_usb_device_selector(
        USB_SELECTION_AUTOMATIC_LOWEST_ALSA_CARD,
        ptr::null(),
        ptr::null(),
    );
    let mut device_match = ffi_usb_device_match();
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &automatic,
            &mut device_match,
        ),
        AUDIO_OK
    );
    assert_eq!(
        identity_component(&device_match.usb_interface_path),
        "3-1:1.0"
    );
    assert_eq!(identity_component(&device_match.usb_serial), "");

    let device_serial = hub.join("3-1").join("serial");
    for invalid_serial in ["\n", "invalid\nserial\n"] {
        fs::write(&device_serial, invalid_serial).expect("write invalid device serial");
        assert_eq!(usb_serial_from_device_path(&device_path), None);
    }
    fs::write(&device_serial, "CM119-A\r\n").expect("write actual device serial");
    assert_eq!(
        usb_serial_from_device_path(&device_path).as_deref(),
        Some("CM119-A")
    );
    assert_eq!(
        resolve_alsa_card_index_from_sysfs_identity(&root, None, Some("CM119-A")),
        Ok(4)
    );
    assert_eq!(
        resolve_alsa_card_index_from_sysfs_identity(&root, None, Some("xhci-hcd.1")),
        Err(AUDIO_UNSUPPORTED)
    );
    fs::remove_dir_all(root).expect("remove upstream hub serial sysfs root");
}

#[cfg(unix)]
#[test]
fn usb_device_selector_automatic_skips_unusable_cards_and_propagates_device_errors() {
    let _serial = lock_fake();
    reset_fake_functions();
    let root = create_test_sysfs_root("device-selector-skip");
    create_test_sysfs_card(&root, 3, "2-1:1.0");
    create_test_sysfs_card(&root, 4, "3-1:1.0");
    let automatic = ffi_usb_device_selector(
        USB_SELECTION_AUTOMATIC_LOWEST_ALSA_CARD,
        ptr::null(),
        ptr::null(),
    );
    let mut device_match = ffi_usb_device_match();

    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &automatic,
            &mut device_match,
        ),
        AUDIO_OK
    );
    assert_eq!(device_match.selection.alsa_card_index, 4);

    fs::remove_dir_all(&root).expect("replace automatic selector root");
    let root = create_test_sysfs_root("device-selector-no-usable-card");
    create_test_sysfs_card(&root, 3, "2-1:1.0");
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &automatic,
            &mut device_match,
        ),
        AUDIO_UNSUPPORTED
    );

    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().device_count = -1);
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &automatic,
            &mut device_match,
        ),
        AUDIO_PORTAUDIO_ERROR
    );
    fs::remove_dir_all(root).expect("remove automatic selector test root");
}

#[cfg(unix)]
#[test]
fn usb_device_selector_rejects_unusable_output_and_oversized_serial() {
    let _serial = lock_fake();
    reset_fake_functions();
    let root = create_test_sysfs_root("device-selector-output");
    create_test_sysfs_card(&root, 4, "3-1:1.0");
    let selector = ffi_usb_device_selector(USB_SELECTION_EXACT, c"hw:4".as_ptr(), ptr::null());
    let mut device_match = ffi_usb_device_match();
    let match_size = device_match.struct_size;

    FAKE_DEVICE_INFO.with(|info| info.borrow_mut().max_output_channels = 0);
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &selector,
            &mut device_match,
        ),
        AUDIO_UNSUPPORTED
    );
    reset_fake_functions();
    fs::remove_dir_all(&root).expect("replace selector output root");

    let root = create_test_sysfs_root("device-selector-long-serial");
    let serial = "A".repeat(USB_SERIAL_CAPACITY);
    create_test_sysfs_card_with_serial(&root, 4, "3-1:1.0", Some(&serial));
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &selector,
            &mut device_match,
        ),
        AUDIO_UNSUPPORTED
    );
    assert_eq!(device_match.struct_size, match_size);
    assert_eq!(device_match.abi_version, 0);
    assert_eq!(identity_component(&device_match.usb_interface_path), "");
    assert_eq!(identity_component(&device_match.usb_serial), "");
    assert_eq!(device_match.selection.abi_version, 0);
    assert_eq!(device_match.selection.alsa_card_index, 0);
    assert_eq!(device_match.selection.input_device_index, 0);
    assert_eq!(device_match.selection.output_device_index, 0);
    fs::remove_dir_all(root).expect("remove selector long-serial root");
}

#[cfg(unix)]
#[test]
fn usb_device_selector_rejects_invalid_abi_and_selection_combinations() {
    let _serial = lock_fake();
    reset_fake_functions();
    let root = create_test_sysfs_root("device-selector-validation");
    let valid = ffi_usb_device_selector(USB_SELECTION_EXACT, c"hw:4".as_ptr(), ptr::null());
    let mut device_match = ffi_usb_device_match();

    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            ptr::null(),
            &mut device_match,
        ),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        usb_device_select_with_functions_and_root(&TEST_FUNCTIONS, &root, &valid, ptr::null_mut(),),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        usb_device_select(ptr::null(), ptr::null_mut()),
        AUDIO_INVALID_ARGUMENT
    );

    let mut reject = |mutate: fn(&mut UsbDeviceSelector)| {
        let mut candidate =
            ffi_usb_device_selector(USB_SELECTION_EXACT, c"hw:4".as_ptr(), ptr::null());
        mutate(&mut candidate);
        assert_eq!(
            usb_device_select_with_functions_and_root(
                &TEST_FUNCTIONS,
                &root,
                &candidate,
                &mut device_match,
            ),
            AUDIO_INVALID_ARGUMENT
        );
        assert_eq!(device_match.abi_version, 0);
    };
    reject(|value| value.struct_size = 0);
    reject(|value| value.selection_policy = 2);
    reject(|value| value.device_identifier = c"hw:4,abc".as_ptr());
    reject(|value| value.device_identifier = c"".as_ptr());
    reject(|value| value.device_identifier = c"../3-1:1.0".as_ptr());
    reject(|value| value.device_identifier = TEST_DEVICE_NAME_INVALID_UTF8.as_ptr().cast());
    reject(|value| value.usb_serial = c"bad\nserial".as_ptr());
    reject(|value| value.usb_serial = TEST_DEVICE_NAME_INVALID_UTF8.as_ptr().cast());
    reject(|value| value.input_device_channels = 0);
    reject(|value| value.output_device_channels = 3);

    let automatic_with_identifier = ffi_usb_device_selector(
        USB_SELECTION_AUTOMATIC_LOWEST_ALSA_CARD,
        c"hw:4".as_ptr(),
        ptr::null(),
    );
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &automatic_with_identifier,
            &mut device_match,
        ),
        AUDIO_INVALID_ARGUMENT
    );
    let automatic_with_serial = ffi_usb_device_selector(
        USB_SELECTION_AUTOMATIC_LOWEST_ALSA_CARD,
        ptr::null(),
        c"CM119-A".as_ptr(),
    );
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &automatic_with_serial,
            &mut device_match,
        ),
        AUDIO_INVALID_ARGUMENT
    );
    let exact_without_identity =
        ffi_usb_device_selector(USB_SELECTION_EXACT, ptr::null(), ptr::null());
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &exact_without_identity,
            &mut device_match,
        ),
        AUDIO_INVALID_ARGUMENT
    );
    device_match.struct_size = 0;
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &valid,
            &mut device_match,
        ),
        AUDIO_INVALID_ARGUMENT
    );
    fs::remove_dir_all(root).expect("remove selector validation sysfs root");
}

#[cfg(unix)]
#[test]
fn usb_device_selector_propagates_exact_inventory_and_runtime_failures() {
    let _serial = lock_fake();
    reset_fake_functions();
    let root = create_test_sysfs_root("device-selector-errors");
    create_test_sysfs_card_with_serial(&root, 4, "3-1:1.0", Some("CM119-A"));
    let mut device_match = ffi_usb_device_match();

    let missing_topology =
        ffi_usb_device_selector(USB_SELECTION_EXACT, c"missing".as_ptr(), ptr::null());
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &missing_topology,
            &mut device_match,
        ),
        AUDIO_UNSUPPORTED
    );
    let missing_serial =
        ffi_usb_device_selector(USB_SELECTION_EXACT, ptr::null(), c"missing".as_ptr());
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &missing_serial,
            &mut device_match,
        ),
        AUDIO_UNSUPPORTED
    );
    let missing_card = ffi_usb_device_selector(USB_SELECTION_EXACT, c"hw:99".as_ptr(), ptr::null());
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &missing_card,
            &mut device_match,
        ),
        AUDIO_UNSUPPORTED
    );

    FAKE_PORTAUDIO.with(|state| state.borrow_mut().initialize_result = -90);
    let valid = ffi_usb_device_selector(USB_SELECTION_EXACT, c"hw:4".as_ptr(), ptr::null());
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &valid,
            &mut device_match,
        ),
        AUDIO_PORTAUDIO_ERROR
    );
    reset_fake_functions();
    fs::remove_dir_all(&root).expect("remove selector error root");

    let automatic = ffi_usb_device_selector(
        USB_SELECTION_AUTOMATIC_LOWEST_ALSA_CARD,
        ptr::null(),
        ptr::null(),
    );
    assert_eq!(
        usb_device_select_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &automatic,
            &mut device_match,
        ),
        AUDIO_UNSUPPORTED
    );
}

#[cfg(unix)]
#[test]
fn usb_device_identity_rejects_ambiguous_portaudio_matches() {
    let _serial = lock_fake();
    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().device_count = 3);
    let root = create_test_sysfs_root("device-ambiguity");
    create_test_sysfs_card(&root, 4, "3-1:1.0");
    let identity = ffi_usb_device_identity(c"3-1:1.0".as_ptr(), ptr::null());
    let mut selection = UsbDeviceSelection {
        struct_size: size_of::<UsbDeviceSelection>() as u32,
        ..UsbDeviceSelection::default()
    };

    assert_eq!(
        usb_device_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &identity,
            &mut selection,
        ),
        AUDIO_UNSUPPORTED
    );
    assert_eq!(selection.abi_version, 0);
    fs::remove_dir_all(root).expect("remove device ambiguity sysfs root");
}

#[cfg(unix)]
#[test]
fn usb_device_identity_rejects_portaudio_plugin_aliases() {
    let _serial = lock_fake();
    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().use_plugin_name_for_first_device = true);
    let root = create_test_sysfs_root("device-plugin");
    create_test_sysfs_card(&root, 4, "3-1:1.0");
    let identity = ffi_usb_device_identity(c"3-1".as_ptr(), ptr::null());
    let mut selection = UsbDeviceSelection {
        struct_size: size_of::<UsbDeviceSelection>() as u32,
        ..UsbDeviceSelection::default()
    };

    assert_eq!(
        usb_device_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &identity,
            &mut selection,
        ),
        AUDIO_UNSUPPORTED
    );
    fs::remove_dir_all(root).expect("remove device plugin sysfs root");
}

#[cfg(unix)]
#[test]
fn usb_device_resolver_rejects_unusable_portaudio_inventory_entries() {
    let _serial = lock_fake();
    let root = create_test_sysfs_root("device-resolver-errors");
    create_test_sysfs_card(&root, 4, "3-1:1.0");
    let identity = ffi_usb_device_identity(c"3-1:1.0".as_ptr(), ptr::null());
    let mut selection = UsbDeviceSelection {
        struct_size: size_of::<UsbDeviceSelection>() as u32,
        ..UsbDeviceSelection::default()
    };

    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().device_count = -1);
    assert_eq!(
        usb_device_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &identity,
            &mut selection,
        ),
        AUDIO_PORTAUDIO_ERROR
    );

    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().device_info_available = false);
    assert_eq!(
        usb_device_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &identity,
            &mut selection,
        ),
        AUDIO_PORTAUDIO_ERROR
    );

    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().use_null_name_for_first_device = true);
    assert_eq!(
        usb_device_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &identity,
            &mut selection,
        ),
        AUDIO_PORTAUDIO_ERROR
    );

    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().use_invalid_utf8_name_for_first_device = true);
    assert_eq!(
        usb_device_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &identity,
            &mut selection,
        ),
        AUDIO_UNSUPPORTED
    );

    reset_fake_functions();
    FAKE_DEVICE_INFO.with(|info| info.borrow_mut().max_input_channels = 0);
    assert_eq!(
        usb_device_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &identity,
            &mut selection,
        ),
        AUDIO_UNSUPPORTED
    );

    reset_fake_functions();
    FAKE_DEVICE_INFO.with(|info| info.borrow_mut().max_output_channels = 0);
    assert_eq!(
        usb_device_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &identity,
            &mut selection,
        ),
        AUDIO_UNSUPPORTED
    );

    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().initialize_result = -91);
    assert_eq!(
        usb_device_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &identity,
            &mut selection,
        ),
        AUDIO_PORTAUDIO_ERROR
    );
    fs::remove_dir_all(root).expect("remove device resolver error root");
}

#[cfg(unix)]
#[test]
fn usb_sysfs_identity_rejects_missing_identity_and_unusable_entries() {
    let _serial = lock_fake();
    let root = create_test_sysfs_root("sysfs-identity-errors");
    assert_eq!(
        resolve_alsa_card_index_from_sysfs_identity(&root, None, None),
        Err(AUDIO_INVALID_ARGUMENT)
    );
    fs::create_dir(root.join(std::ffi::OsString::from_vec(vec![
        b'c', b'a', b'r', b'd', 0xff,
    ])))
    .expect("create non-UTF-8 sysfs entry");
    fs::create_dir(root.join("card9")).expect("create sysfs entry without device link");
    assert_eq!(
        resolve_alsa_card_index_from_sysfs_identity(&root, Some("3-1:1.0"), None),
        Err(AUDIO_UNSUPPORTED)
    );
    fs::remove_dir_all(root).expect("remove sysfs identity error root");
}

#[test]
fn function_table_releases_failed_device_initialization() {
    let _serial = lock_fake();
    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().device_count = -1);
    let config = ffi_stream_config();
    let mut stream = ptr::null_mut();

    assert_eq!(
        stream_create_with_functions(&TEST_FUNCTIONS, &config, &mut stream),
        AUDIO_PORTAUDIO_ERROR
    );
    assert!(stream.is_null());
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.initialize_count, 1);
        assert_eq!(state.terminate_count, 1);
        assert_eq!(state.open_count, 0);
    });
}

#[test]
fn portaudio_runtime_reuses_matching_functions_and_rejects_mismatches() {
    let _serial = lock_fake();
    reset_fake_functions();
    let config = ffi_stream_config();
    let first = fake_stream(&config);
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().device_count = 4);
    let mut second_config = config;
    second_config.input_device_index = 2;
    second_config.output_device_index = 3;
    let second = fake_stream(&second_config);
    let alternate = Box::leak(Box::new(TEST_FUNCTIONS));

    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.initialize_count, 1);
        assert_eq!(state.open_count, 4);
    });
    assert_eq!(portaudio_acquire(alternate), Err(AUDIO_PORTAUDIO_ERROR));
    portaudio_release(alternate);

    stream_destroy(first);
    FAKE_PORTAUDIO.with(|state| assert_eq!(state.borrow().terminate_count, 0));
    stream_destroy(second);
    portaudio_release(&TEST_FUNCTIONS);
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.terminate_count, 1);
        assert_eq!(state.close_count, 4);
    });
}

#[test]
fn stream_create_exclusively_leases_resolved_devices_until_destroy() {
    let _serial = lock_fake();
    reset_fake_functions();
    let config = ffi_stream_config();
    let first = fake_stream(&config);
    let mut conflicting_config = ffi_stream_config();
    conflicting_config.output_device_index = 0;
    let mut conflicting = NonNull::<AudioStream>::dangling().as_ptr();

    assert_eq!(
        stream_create_with_functions(&TEST_FUNCTIONS, &conflicting_config, &mut conflicting,),
        AUDIO_DEVICE_BUSY
    );
    assert!(conflicting.is_null());
    FAKE_PORTAUDIO.with(|state| assert_eq!(state.borrow().open_count, 2));

    FAKE_PORTAUDIO.with(|state| state.borrow_mut().device_count = 3);
    let mut output_conflict = ffi_stream_config();
    output_conflict.input_device_index = 2;
    output_conflict.output_device_index = 1;
    assert_eq!(
        stream_create_with_functions(&TEST_FUNCTIONS, &output_conflict, &mut conflicting,),
        AUDIO_DEVICE_BUSY
    );
    assert!(conflicting.is_null());
    FAKE_PORTAUDIO.with(|state| assert_eq!(state.borrow().open_count, 2));

    stream_destroy(first);
    let replacement = fake_stream(&config);
    stream_destroy(replacement);
}

#[test]
fn stream_create_releases_the_device_lease_after_an_open_failure() {
    let _serial = lock_fake();
    reset_fake_functions();
    let config = ffi_stream_config();
    let mut stream = NonNull::<AudioStream>::dangling().as_ptr();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().open_result = -12);

    assert_eq!(
        stream_create_with_functions(&TEST_FUNCTIONS, &config, &mut stream),
        AUDIO_PORTAUDIO_ERROR
    );
    assert!(stream.is_null());

    FAKE_PORTAUDIO.with(|state| state.borrow_mut().open_result = ffi::PA_NO_ERROR);
    let recovered = fake_stream(&config);
    stream_destroy(recovered);
}

#[test]
fn device_lease_handles_one_device_and_recovers_from_a_poisoned_lock() {
    let _serial = lock_fake();
    let single_device = DeviceLease::acquire(8, 8).expect("reserve one physical device");
    assert_eq!(single_device.count, 1);
    drop(single_device);

    let poisoned = std::panic::catch_unwind(|| {
        let _guard = device_lease_registry()
            .lock()
            .expect("test lease registry starts unpoisoned");
        panic!("intentionally poison the device-lease mutex");
    });
    assert!(poisoned.is_err());
    assert!(device_lease_registry().is_poisoned());

    let recovered = DeviceLease::acquire(8, 9).expect("recover from poisoned lease mutex");
    assert_eq!(recovered.count, 2);
    drop(recovered);
}

#[test]
fn portaudio_runtime_reports_initialization_failure_without_a_reference() {
    let _serial = lock_fake();
    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().initialize_result = -101);

    assert_eq!(
        portaudio_acquire(&TEST_FUNCTIONS),
        Err(AUDIO_PORTAUDIO_ERROR)
    );
    portaudio_release(&TEST_FUNCTIONS);
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.initialize_count, 1);
        assert_eq!(state.terminate_count, 0);
    });
}

#[test]
fn portaudio_runtime_recovers_from_a_poisoned_control_plane_lock() {
    let _serial = lock_fake();
    reset_fake_functions();
    let poisoned = std::panic::catch_unwind(|| {
        let _guard = portaudio_runtime()
            .lock()
            .expect("test runtime starts unpoisoned");
        panic!("intentionally poison the control-plane mutex");
    });
    assert!(poisoned.is_err());
    assert!(portaudio_runtime().is_poisoned());

    assert_eq!(portaudio_acquire(&TEST_FUNCTIONS), Ok(()));
    portaudio_release(&TEST_FUNCTIONS);
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!((state.initialize_count, state.terminate_count), (1, 1));
    });
}

#[test]
fn resolve_device_uses_default_low_latency_and_explicit_indices() {
    let _serial = lock_fake();
    reset_fake_functions();

    let (input_device, input) = resolve_device(&TEST_FUNCTIONS, DEFAULT_DEVICE, 1, true).unwrap();
    let (output_device, output) =
        resolve_device(&TEST_FUNCTIONS, DEFAULT_DEVICE, 2, false).unwrap();
    let (explicit_device, explicit) = resolve_device(&TEST_FUNCTIONS, 0, 1, false).unwrap();

    assert_eq!(input_device, 0);
    assert_eq!(input.suggested_latency, 0.004);
    assert_eq!(input.channel_count, 1);
    assert_eq!(output_device, 1);
    assert_eq!(output.suggested_latency, 0.006);
    assert_eq!(output.channel_count, 2);
    assert_eq!(explicit_device, 0);
    assert_eq!(explicit.sample_format, ffi::PA_FLOAT_32);
}

#[test]
fn resolve_device_rejects_unavailable_or_unsupported_devices() {
    let _serial = lock_fake();
    reset_fake_functions();

    FAKE_PORTAUDIO.with(|state| state.borrow_mut().device_count = -1);
    assert!(matches!(
        resolve_device(&TEST_FUNCTIONS, DEFAULT_DEVICE, 1, true),
        Err(AUDIO_PORTAUDIO_ERROR)
    ));
    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().default_input_device = ffi::PA_NO_DEVICE);
    assert!(matches!(
        resolve_device(&TEST_FUNCTIONS, DEFAULT_DEVICE, 1, true),
        Err(AUDIO_UNSUPPORTED)
    ));
    reset_fake_functions();
    assert!(matches!(
        resolve_device(&TEST_FUNCTIONS, 2, 1, true),
        Err(AUDIO_UNSUPPORTED)
    ));
    assert!(matches!(
        resolve_device(&TEST_FUNCTIONS, -2, 1, false),
        Err(AUDIO_UNSUPPORTED)
    ));
    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().device_info_available = false);
    assert!(matches!(
        resolve_device(&TEST_FUNCTIONS, 0, 1, true),
        Err(AUDIO_PORTAUDIO_ERROR)
    ));
    reset_fake_functions();
    FAKE_DEVICE_INFO.with(|info| info.borrow_mut().max_input_channels = 0);
    assert!(matches!(
        resolve_device(&TEST_FUNCTIONS, 0, 1, true),
        Err(AUDIO_UNSUPPORTED)
    ));
    reset_fake_functions();
    FAKE_DEVICE_INFO.with(|info| info.borrow_mut().max_output_channels = 1);
    assert!(matches!(
        resolve_device(&TEST_FUNCTIONS, 1, 2, false),
        Err(AUDIO_UNSUPPORTED)
    ));
}

#[test]
fn callback_preserves_f32_samples_and_splits_oversized_blocks() {
    let mut stream = prepared_test_stream(
        &ffi::PRODUCTION_FUNCTIONS,
        test_config(2, 1, 1, copy_input_to_output),
    );
    let input = [0.123_456_7, -0.75, 1.0];
    let mut output = [0.0; 3];

    let result = unsafe {
        stream.process_callback(
            input.as_ptr(),
            output.as_mut_ptr(),
            input.len(),
            ffi::PA_INPUT_OVERFLOW | ffi::PA_OUTPUT_UNDERFLOW,
        )
    };

    assert_eq!(result, ffi::PA_CONTINUE);
    assert_eq!(output, input);
    assert_eq!(
        stream
            .stats
            .oversized_callback_count
            .load(Ordering::Acquire),
        1
    );
    assert_eq!(stream.stats.callback_count.load(Ordering::Acquire), 1);
    assert_eq!(stream.stats.callback_frame_count.load(Ordering::Acquire), 3);
    assert_eq!(stream.stats.input_overflow_count.load(Ordering::Acquire), 1);
    assert_eq!(
        stream.stats.output_underflow_count.load(Ordering::Acquire),
        1
    );
    assert_eq!(
        stream.stats.input_clip_sample_count.load(Ordering::Acquire),
        1
    );
    assert_eq!(
        stream
            .stats
            .output_clip_sample_count
            .load(Ordering::Acquire),
        1
    );
}

#[test]
fn callback_failure_silences_the_current_device_block() {
    let mut stream =
        prepared_test_stream(&ffi::PRODUCTION_FUNCTIONS, test_config(2, 2, 2, fail_tick));
    let input = [0.25, -0.25, 0.5, -0.5];
    let mut output = [1.0; 4];

    let result = unsafe { stream.process_callback(input.as_ptr(), output.as_mut_ptr(), 2, 0) };

    assert_eq!(result, ffi::PA_ABORT);
    assert_eq!(output, [0.0; 4]);
    assert_eq!(
        stream
            .stats
            .native_tick_failure_count
            .load(Ordering::Acquire),
        1
    );
}

#[test]
fn callback_failure_silences_unprocessed_oversized_output() {
    let mut invocation = 0_usize;
    let mut stream = prepared_test_stream(
        &ffi::PRODUCTION_FUNCTIONS,
        ValidatedStreamConfig {
            native_tick_context: (&mut invocation as *mut usize).cast::<c_void>(),
            ..test_config(2, 1, 1, fail_on_second_tick)
        },
    );
    let input = [0.25, -0.5, 0.75];
    let mut output = [1.0; 3];

    let result = unsafe { stream.process_callback(input.as_ptr(), output.as_mut_ptr(), 3, 0) };

    assert_eq!(result, ffi::PA_ABORT);
    assert_eq!(output, [0.25, -0.5, 0.0]);
    assert_eq!(invocation, 2);
}

#[test]
fn callback_timing_uses_the_previous_period_and_strict_lateness_threshold() {
    let shared = SharedStats::default();
    let mut stats = StreamStats::default();
    shared.snapshot(&mut stats);
    assert_eq!(stats.callback_late_start_tolerance_ns, 1_000_000);
    assert_eq!(stats.callback_last_duration_ns, 0);
    assert_eq!(stats.callback_max_duration_ns, 0);
    assert_eq!(stats.callback_last_start_delay_ns, 0);
    assert_eq!(stats.callback_max_start_delay_ns, 0);
    assert_eq!(stats.callback_late_start_count, 0);
    assert_eq!(stats.last_input_xrun_monotonic_ns, 0);
    assert_eq!(stats.last_output_xrun_monotonic_ns, 0);
    assert_eq!(stats.callback_clock_error_count, 0);

    // A first callback has no arrival baseline. The next callback uses this
    // block's 20 ms duration, not its own shorter 5 ms duration.
    shared.callback_begin(100_000_000, 20_000_000, 0);
    shared.callback_begin(121_000_000, 5_000_000, 0);
    shared.snapshot(&mut stats);
    assert_eq!(stats.callback_last_start_delay_ns, 1_000_000);
    assert_eq!(stats.callback_late_start_count, 0);
    shared.callback_begin(127_000_001, 10_000_000, 0);
    shared.snapshot(&mut stats);
    assert_eq!(stats.callback_last_start_delay_ns, 1_000_001);
    assert_eq!(stats.callback_max_start_delay_ns, 1_000_001);
    assert_eq!(stats.callback_late_start_count, 1);

    // On-time and early starts clear the latest delay, not its historical max.
    shared.callback_begin(137_000_001, 20_000_000, 0);
    shared.callback_begin(140_000_000, 20_000_000, 0);
    shared.snapshot(&mut stats);
    assert_eq!(stats.callback_last_start_delay_ns, 0);
    assert_eq!(stats.callback_max_start_delay_ns, 1_000_001);
    shared.callback_begin(300_000_000, 20_000_000, 0);
    shared.snapshot(&mut stats);
    assert_eq!(stats.callback_last_start_delay_ns, 140_000_000);
    assert_eq!(stats.callback_max_start_delay_ns, 140_000_000);
    assert_eq!(stats.callback_late_start_count, 2);

    // A zero-length predecessor has no expected audio cadence.
    shared.callback_begin(320_000_000, 0, 0);
    shared.callback_begin(900_000_000, 20_000_000, 0);
    shared.snapshot(&mut stats);
    assert_eq!(stats.callback_last_start_delay_ns, 0);
    assert_eq!(stats.callback_late_start_count, 2);
}

#[test]
fn callback_duration_tracks_extrema_and_failed_clocks_reset_the_arrival_baseline() {
    let shared = SharedStats::default();
    let mut stats = StreamStats::default();
    shared.callback_begin(100_000_000, 20_000_000, 0);
    shared.callback_end(100_000_000, 102_000_000);
    shared.callback_end(120_000_000, 121_000_000);
    shared.snapshot(&mut stats);
    assert_eq!(stats.callback_last_duration_ns, 1_000_000);
    assert_eq!(stats.callback_max_duration_ns, 2_000_000);
    shared.callback_end(140_000_000, 143_000_000);
    shared.callback_end(150_000_000, 149_000_000);
    shared.snapshot(&mut stats);
    assert_eq!(stats.callback_last_duration_ns, 0);
    assert_eq!(stats.callback_max_duration_ns, 3_000_000);

    shared.callback_begin(0, 20_000_000, 0);
    shared.callback_end(0, 0);
    shared.callback_end(0, 160_000_000);
    shared.callback_end(160_000_000, 0);
    shared.callback_begin(1_000_000_000, 20_000_000, 0);
    shared.snapshot(&mut stats);
    assert_eq!(stats.callback_clock_error_count, 3);
    assert_eq!(stats.callback_last_duration_ns, 0);
    assert_eq!(stats.callback_max_duration_ns, 3_000_000);
    assert_eq!(stats.callback_last_start_delay_ns, 0);
    assert_eq!(stats.callback_late_start_count, 0);
    // A backward test clock cannot underflow into an enormous delay.
    shared.callback_begin(900_000_000, 20_000_000, 0);
    shared.snapshot(&mut stats);
    assert_eq!(stats.callback_last_start_delay_ns, 0);
}

#[test]
fn callback_xrun_timestamps_are_independent_and_ignore_failed_clock_reads() {
    let shared = SharedStats::default();
    let mut stats = StreamStats::default();
    shared.callback_begin(100_000_000, 20_000_000, ffi::PA_INPUT_OVERFLOW);
    shared.snapshot(&mut stats);
    assert_eq!(stats.last_input_xrun_monotonic_ns, 100_000_000);
    assert_eq!(stats.last_output_xrun_monotonic_ns, 0);
    shared.callback_begin(120_000_000, 20_000_000, ffi::PA_OUTPUT_UNDERFLOW);
    shared.snapshot(&mut stats);
    assert_eq!(stats.last_input_xrun_monotonic_ns, 100_000_000);
    assert_eq!(stats.last_output_xrun_monotonic_ns, 120_000_000);
    shared.callback_begin(
        140_000_000,
        20_000_000,
        ffi::PA_INPUT_OVERFLOW | ffi::PA_OUTPUT_UNDERFLOW,
    );
    shared.callback_begin(160_000_000, 20_000_000, 0);
    shared.callback_begin(
        0,
        20_000_000,
        ffi::PA_INPUT_OVERFLOW | ffi::PA_OUTPUT_UNDERFLOW,
    );
    shared.snapshot(&mut stats);
    assert_eq!(stats.last_input_xrun_monotonic_ns, 140_000_000);
    assert_eq!(stats.last_output_xrun_monotonic_ns, 140_000_000);
    assert_eq!(stats.callback_clock_error_count, 1);
}

#[test]
fn monotonic_clock_conversion_and_failures_are_deterministic() {
    unsafe extern "C" fn known_time(clock: c_int, time: *mut ffi::Timespec) -> c_int {
        assert_eq!(clock, 1);
        unsafe {
            *time = ffi::Timespec {
                seconds: 123,
                nanoseconds: 456_789_012,
            }
        };
        0
    }
    unsafe extern "C" fn failed_time(clock: c_int, _time: *mut ffi::Timespec) -> c_int {
        assert_eq!(clock, 1);
        -1
    }
    assert_eq!(ffi::monotonic_ns_with(known_time), 123_456_789_012);
    assert_eq!(ffi::monotonic_ns_with(failed_time), 0);
    assert!(ffi::monotonic_ns() > 0);
}

#[test]
fn stream_stats_preserve_original_and_future_caller_allocation_bounds() {
    #[repr(C, align(8))]
    struct LegacyStorage {
        prefix: [u8; STREAM_STATS_V1_SIZE],
        canary: [u8; 32],
    }
    #[repr(C)]
    struct FutureStorage {
        current: StreamStats,
        canary: [u8; 32],
    }
    #[repr(C, align(8))]
    struct TimingStorage {
        prefix: [u8; STREAM_STATS_TIMING_SIZE],
        canary: [u8; 32],
    }
    assert_eq!(STREAM_STATS_V1_SIZE, 144);
    assert_eq!(STREAM_STATS_TIMING_SIZE, 216);
    assert_eq!(size_of::<StreamStats>(), 264);
    let stream = prepared_test_stream(
        &ffi::PRODUCTION_FUNCTIONS,
        test_config(2, 2, 2, copy_input_to_output),
    );
    stream.stats.callback_count.store(17, Ordering::Relaxed);
    stream.stats.record_portaudio_error(-321);
    stream.stats.callback_end(1, 987_655);
    let mut timing = TimingStorage {
        prefix: [0; STREAM_STATS_TIMING_SIZE],
        canary: [0x6c; 32],
    };
    timing.prefix[..4].copy_from_slice(&(STREAM_STATS_TIMING_SIZE as u32).to_ne_bytes());
    assert_eq!(
        stream_get_stats(&stream, timing.prefix.as_mut_ptr().cast()),
        AUDIO_OK
    );
    assert_eq!(timing.canary, [0x6c; 32]);
    let duration_offset = offset_of!(StreamStats, callback_last_duration_ns);
    assert_eq!(
        u64::from_ne_bytes(
            timing.prefix[duration_offset..duration_offset + 8]
                .try_into()
                .unwrap()
        ),
        987_654
    );
    let mut legacy = LegacyStorage {
        prefix: [0; STREAM_STATS_V1_SIZE],
        canary: [0xa5; 32],
    };
    legacy.prefix[..4].copy_from_slice(&(STREAM_STATS_V1_SIZE as u32).to_ne_bytes());
    assert_eq!(
        stream_get_stats(&stream, legacy.prefix.as_mut_ptr().cast()),
        AUDIO_OK
    );
    assert_eq!(legacy.canary, [0xa5; 32]);
    assert_eq!(
        u32::from_ne_bytes(legacy.prefix[..4].try_into().unwrap()),
        STREAM_STATS_V1_SIZE as u32
    );
    let callback_offset = std::mem::offset_of!(StreamStats, callback_count);
    assert_eq!(
        u64::from_ne_bytes(
            legacy.prefix[callback_offset..callback_offset + 8]
                .try_into()
                .unwrap()
        ),
        17
    );
    let error_offset = std::mem::offset_of!(StreamStats, last_portaudio_error);
    assert_eq!(
        i32::from_ne_bytes(
            legacy.prefix[error_offset..error_offset + 4]
                .try_into()
                .unwrap()
        ),
        -321
    );

    let mut future = FutureStorage {
        current: StreamStats {
            struct_size: size_of::<FutureStorage>() as u32,
            ..StreamStats::default()
        },
        canary: [0x5a; 32],
    };
    assert_eq!(stream_get_stats(&stream, &mut future.current), AUDIO_OK);
    assert_eq!(future.canary, [0x5a; 32]);
    assert_eq!(
        future.current.struct_size,
        size_of::<FutureStorage>() as u32
    );
    assert_eq!(future.current.callback_count, 17);
    assert_eq!(future.current.callback_last_duration_ns, 987_654);
    assert_eq!(future.current.callback_late_start_tolerance_ns, 1_000_000);
}

#[test]
fn real_callback_records_duration_and_xruns_on_success_and_native_failure() {
    for (tick, expected) in [
        (
            copy_input_to_output
                as unsafe extern "C" fn(*mut c_void, *const f32, *mut f32, u32) -> c_int,
            ffi::PA_CONTINUE,
        ),
        (fail_tick, ffi::PA_ABORT),
    ] {
        let stream = prepared_test_stream(&ffi::PRODUCTION_FUNCTIONS, test_config(2, 1, 2, tick));
        let input = [0.25_f32, -0.25, 0.5, -0.5];
        let mut output = [1.0_f32; 4];
        let before = ffi::monotonic_ns();
        assert_eq!(
            unsafe {
                capture_callback(
                    input.as_ptr().cast(),
                    ptr::null_mut(),
                    2,
                    ptr::null(),
                    ffi::PA_INPUT_OVERFLOW,
                    stream.capture.get().cast(),
                )
            },
            ffi::PA_CONTINUE
        );
        let result = unsafe {
            portaudio_callback(
                input.as_ptr().cast(),
                output.as_mut_ptr().cast(),
                2,
                ptr::null(),
                ffi::PA_INPUT_OVERFLOW | ffi::PA_OUTPUT_UNDERFLOW,
                stream.playback.get().cast(),
            )
        };
        let after = ffi::monotonic_ns();
        assert_eq!(result, expected);
        let mut stats = StreamStats {
            struct_size: size_of::<StreamStats>() as u32,
            ..StreamStats::default()
        };
        assert_eq!(stream_get_stats(&stream, &mut stats), AUDIO_OK);
        assert!(stats.callback_last_duration_ns > 0);
        assert_eq!(
            stats.callback_max_duration_ns,
            stats.callback_last_duration_ns
        );
        assert!(stats.callback_last_duration_ns <= after - before);
        assert_eq!(stats.callback_last_start_delay_ns, 0);
        assert_eq!(stats.callback_late_start_count, 0);
        assert_eq!(stats.callback_clock_error_count, 0);
        assert!((before..=after).contains(&stats.last_input_xrun_monotonic_ns));
        assert!(
            (stats.last_input_xrun_monotonic_ns..=after)
                .contains(&stats.last_output_xrun_monotonic_ns)
        );
        assert_eq!(stats.input_overflow_count, 1);
        assert_eq!(stats.output_underflow_count, 1);
        assert_eq!(
            stats.native_tick_failure_count,
            u64::from(expected == ffi::PA_ABORT)
        );
        assert_eq!(output, [0.0; 4]);
        assert_eq!(stats.capture_callback_count, 1);
        assert_eq!(stats.capture_startup_wait_frames, 2);
    }
}

#[test]
fn stats_report_capture_queue_and_device_errors() {
    let stream = prepared_test_stream(
        &ffi::PRODUCTION_FUNCTIONS,
        test_config(2, 2, 2, copy_input_to_output),
    );
    stream.stats.record_portaudio_error(-321);
    let mut stats = StreamStats {
        struct_size: size_of::<StreamStats>() as u32,
        ..StreamStats::default()
    };

    assert_eq!(stream_get_stats(&stream, &mut stats), AUDIO_OK);
    assert_eq!(stats.device_error_count, 1);
    assert_eq!(stats.last_portaudio_error, -321);
    assert_eq!(stats.input_queue_capacity_frames, 512);
    assert_eq!(stats.capture_ring_target_frames, 98);
    assert_eq!(stats.input_queue_occupancy_frames, 0);
    assert_eq!(stats.output_queue_capacity_frames, 0);
    assert_eq!(stats.output_queue_occupancy_frames, 0);
    assert_eq!(stats.output_queue_dropped_frame_count, 0);
}

#[test]
fn stream_timing_reports_actual_portaudio_values_and_clears_failed_output() {
    let _serial = lock_fake();
    reset_fake_functions();
    let stream = fake_stream(&ffi_stream_config());
    let mut timing = ffi_stream_timing();

    assert_eq!(stream_get_timing(stream, &mut timing), AUDIO_OK);
    assert_eq!(timing.abi_version, ABI_VERSION);
    assert_eq!(timing.input_latency_seconds, 0.008);
    assert_eq!(timing.output_latency_seconds, 0.012);
    assert_eq!(timing.sample_rate_hz, 47_999.5);

    let timing_size = timing.struct_size;
    timing.abi_version = 99;
    timing.input_latency_seconds = 1.0;
    timing.output_latency_seconds = 1.0;
    timing.sample_rate_hz = 1.0;
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().stream_info_available = false);
    assert_eq!(
        stream_get_timing(stream, &mut timing),
        AUDIO_PORTAUDIO_ERROR
    );
    assert_eq!(timing.struct_size, timing_size);
    assert_eq!(timing.abi_version, 0);
    assert_eq!(timing.input_latency_seconds, 0.0);
    assert_eq!(timing.output_latency_seconds, 0.0);
    assert_eq!(timing.sample_rate_hz, 0.0);

    let mut stats = StreamStats {
        struct_size: size_of::<StreamStats>() as u32,
        ..StreamStats::default()
    };
    assert_eq!(stream_get_stats(stream, &mut stats), AUDIO_OK);
    assert_eq!(stats.device_error_count, 1);
    assert_eq!(stats.last_portaudio_error, ffi::PA_INTERNAL_ERROR);
    stream_destroy(stream);
}

#[test]
fn stream_timing_accepts_portaudio_alsa_zero_struct_version() {
    let _serial = lock_fake();
    reset_fake_functions();
    let stream = fake_stream(&ffi_stream_config());
    FAKE_STREAM_INFO.with(|info| info.borrow_mut().struct_version = 0);
    let mut timing = ffi_stream_timing();
    assert_eq!(stream_get_timing(stream, &mut timing), AUDIO_OK);
    assert_eq!(timing.abi_version, ABI_VERSION);
    assert_eq!(timing.input_latency_seconds, 0.008);
    assert_eq!(timing.output_latency_seconds, 0.012);
    assert_eq!(timing.sample_rate_hz, 47_999.5);
    let mut stats = StreamStats {
        struct_size: size_of::<StreamStats>() as u32,
        ..StreamStats::default()
    };
    assert_eq!(stream_get_stats(stream, &mut stats), AUDIO_OK);
    assert_eq!(stats.device_error_count, 0);
    assert_eq!(stats.last_portaudio_error, ffi::PA_NO_ERROR);
    stream_destroy(stream);
}

#[test]
fn stream_timing_rejects_invalid_arguments_and_malformed_portaudio_data() {
    let _serial = lock_fake();
    reset_fake_functions();
    let stream = fake_stream(&ffi_stream_config());
    let mut timing = ffi_stream_timing();

    assert_eq!(
        stream_get_timing(ptr::null(), &mut timing),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        stream_get_timing(stream, ptr::null_mut()),
        AUDIO_INVALID_ARGUMENT
    );
    timing.struct_size = 0;
    assert_eq!(
        stream_get_timing(stream, &mut timing),
        AUDIO_INVALID_ARGUMENT
    );

    let timing_size = size_of::<StreamTiming>() as u32;
    let mut reject = |mutate: fn(&mut ffi::PaStreamInfo)| {
        reset_fake_stream_info();
        FAKE_PORTAUDIO.with(|state| state.borrow_mut().stream_info_available = true);
        FAKE_STREAM_INFO.with(|info| mutate(&mut info.borrow_mut()));
        timing = StreamTiming {
            struct_size: timing_size,
            abi_version: 99,
            input_latency_seconds: 1.0,
            output_latency_seconds: 1.0,
            sample_rate_hz: 1.0,
        };
        assert_eq!(
            stream_get_timing(stream, &mut timing),
            AUDIO_PORTAUDIO_ERROR
        );
        assert_eq!(timing.struct_size, timing_size);
        assert_eq!(timing.abi_version, 0);
        assert_eq!(timing.input_latency_seconds, 0.0);
        assert_eq!(timing.output_latency_seconds, 0.0);
        assert_eq!(timing.sample_rate_hz, 0.0);
    };
    reject(|info| info.struct_version = -1);
    reject(|info| info.input_latency = f64::NAN);
    reject(|info| info.output_latency = f64::INFINITY);
    reject(|info| info.sample_rate = f64::NAN);
    reject(|info| info.input_latency = -0.001);
    reject(|info| info.output_latency = -0.001);
    reject(|info| info.sample_rate = 0.0);
    reject(|info| info.sample_rate = -1.0);
    stream_destroy(stream);
}

#[test]
fn stream_create_rejects_invalid_abi_before_opening_a_device() {
    let _serial = lock_fake();
    reset_fake_functions();
    let config = ffi_stream_config();
    let mut stream = NonNull::<AudioStream>::dangling().as_ptr();

    assert_eq!(
        stream_create_with_functions(&TEST_FUNCTIONS, &config, ptr::null_mut()),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        stream_create_with_functions(&TEST_FUNCTIONS, ptr::null(), &mut stream),
        AUDIO_INVALID_ARGUMENT
    );
    assert!(stream.is_null());
    assert_eq!(
        stream_create(&config, ptr::null_mut()),
        AUDIO_INVALID_ARGUMENT
    );
    FAKE_PORTAUDIO.with(|state| assert_eq!(state.borrow().initialize_count, 0));
}

#[test]
fn stream_create_releases_runtime_after_all_device_and_open_failures() {
    let _serial = lock_fake();
    let config = ffi_stream_config();
    let mut stream = ptr::null_mut();

    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().initialize_result = -11);
    assert_eq!(
        stream_create_with_functions(&TEST_FUNCTIONS, &config, &mut stream),
        AUDIO_PORTAUDIO_ERROR
    );
    assert!(stream.is_null());
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.initialize_count, 1);
        assert_eq!(state.terminate_count, 0);
    });

    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().default_input_device = ffi::PA_NO_DEVICE);
    assert_eq!(
        stream_create_with_functions(&TEST_FUNCTIONS, &config, &mut stream),
        AUDIO_UNSUPPORTED
    );
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(
            (
                state.initialize_count,
                state.terminate_count,
                state.open_count
            ),
            (1, 1, 0)
        );
    });

    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().default_output_device = ffi::PA_NO_DEVICE);
    assert_eq!(
        stream_create_with_functions(&TEST_FUNCTIONS, &config, &mut stream),
        AUDIO_UNSUPPORTED
    );
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(
            (
                state.initialize_count,
                state.terminate_count,
                state.open_count
            ),
            (1, 1, 0)
        );
    });

    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().open_result = -12);
    assert_eq!(
        stream_create_with_functions(&TEST_FUNCTIONS, &config, &mut stream),
        AUDIO_PORTAUDIO_ERROR
    );
    assert!(stream.is_null());
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(
            (
                state.initialize_count,
                state.terminate_count,
                state.open_count
            ),
            (1, 1, 1)
        );
        assert_eq!(state.close_count, 0);
    });
}

#[test]
fn stream_start_inherits_highest_fifo_and_restores_each_caller_policy() {
    let _serial = lock_fake();
    for (policy, priority, maximum) in [(0, 0, 99), (2, 10, 99), (1, 99, 99), (2, 10, 73)] {
        reset_fake_functions();
        FAKE_SCHEDULING.with(|state| {
            let mut state = state.borrow_mut();
            state.policy = policy;
            state.priority = priority;
            state.maximum = maximum;
        });
        let stream = fake_stream(&ffi_stream_config());
        assert_eq!(stream_start(stream), AUDIO_OK);
        FAKE_SCHEDULING.with(|state| {
            let state = state.borrow();
            assert_eq!(state.start_schedule, Some((ffi::SCHED_FIFO, maximum)));
            assert_eq!((state.policy, state.priority), (policy, priority));
            assert_eq!(
                state.set_requests,
                [(ffi::SCHED_FIFO, maximum), (policy, priority)]
            );
            assert_eq!(
                state.events,
                ["self", "get", "max", "set", "start", "start", "set"]
            );
        });
        // An already-running callback needs no second scheduling or start operation.
        assert_eq!(stream_start(stream), AUDIO_OK);
        FAKE_SCHEDULING.with(|state| assert_eq!(state.borrow().set_requests.len(), 2));
        FAKE_PORTAUDIO.with(|state| assert_eq!(state.borrow().start_count, 2));
        stream_destroy(stream);
    }
}

#[test]
fn independent_streams_reject_stereo_capture_and_unmatched_rate_before_open() {
    let _serial = lock_fake();
    reset_fake_functions();
    for (channels, rate) in [(2, 48_000), (1, 44_100)] {
        let mut config = ffi_stream_config();
        config.input_device_channels = channels;
        config.native_sample_rate_hz = rate;
        let mut stream = NonNull::<AudioStream>::dangling().as_ptr();
        assert_eq!(
            stream_create_with_functions(&TEST_FUNCTIONS, &config, &mut stream),
            AUDIO_UNSUPPORTED
        );
        assert!(stream.is_null());
    }
    FAKE_PORTAUDIO.with(|state| assert_eq!(state.borrow().initialize_count, 0));
}

#[test]
fn second_endpoint_open_and_start_failures_release_or_quiesce_capture() {
    let _serial = lock_fake();
    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().open_results.extend([0, -41]));
    let mut stream = ptr::null_mut();
    assert_eq!(
        stream_create_with_functions(&TEST_FUNCTIONS, &ffi_stream_config(), &mut stream),
        AUDIO_PORTAUDIO_ERROR
    );
    assert!(stream.is_null());
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(
            (state.open_count, state.close_count, state.terminate_count),
            (2, 1, 1)
        );
    });
    let stream = fake_stream(&ffi_stream_config());
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().start_results.extend([0, -42]));
    assert_eq!(stream_start(stream), AUDIO_PORTAUDIO_ERROR);
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.start_count, 2);
        assert_eq!(state.abort_count, 1);
        assert!(state.endpoint_active.iter().all(|active| *active == 0));
    });
    FAKE_SCHEDULING
        .with(|state| assert_eq!((state.borrow().policy, state.borrow().priority), (2, 10)));
    assert_eq!(stream_start(stream), AUDIO_OK);
    stream_destroy(stream);
}

#[test]
fn inactive_started_endpoint_is_joined_and_capture_queue_reset_before_restart() {
    let _serial = lock_fake();
    reset_fake_functions();
    let stream = fake_stream(&ffi_stream_config());
    assert_eq!(stream_start(stream), AUDIO_OK);
    unsafe {
        (*stream).ring.push(&[0.5; 2]).unwrap();
    }
    // A callback may return PA_ABORT before control notices it has finished.
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().force_active(0));
    assert_eq!(stream_start(stream), AUDIO_OK);
    FAKE_PORTAUDIO.with(|state| assert_eq!(state.borrow().stop_count, 2));
    assert_eq!(
        unsafe { (*stream).ring.snapshot().unwrap().occupancy_frames },
        0
    );
    stream_destroy(stream);
}

#[test]
fn capture_can_publish_while_native_tick_runs_in_disjoint_playback_context() {
    use std::sync::Barrier;
    unsafe extern "C" fn held_tick(
        context: *mut c_void,
        _input: *const f32,
        output: *mut f32,
        frames: u32,
    ) -> c_int {
        let barrier = unsafe { &*context.cast::<Barrier>() };
        barrier.wait();
        barrier.wait();
        unsafe { std::slice::from_raw_parts_mut(output, frames as usize * 2) }.fill(0.0);
        0
    }
    let barrier = Barrier::new(2);
    let mut config = test_config(960, 1, 2, held_tick);
    config.native_tick_context = (&barrier as *const Barrier).cast_mut().cast();
    let stream = prepared_test_stream(&ffi::PRODUCTION_FUNCTIONS, config);
    let playback = stream.playback.get() as usize;
    let worker = std::thread::spawn(move || {
        let mut output = [1.0_f32; 1920];
        let status = unsafe {
            portaudio_callback(
                ptr::null(),
                output.as_mut_ptr().cast(),
                960,
                ptr::null(),
                0,
                playback as *mut c_void,
            )
        };
        assert_eq!(status, ffi::PA_CONTINUE);
        assert_eq!(output, [0.0; 1920]);
    });
    barrier.wait();
    let input = [1.0_f32; 960];
    assert_eq!(
        unsafe {
            capture_callback(
                input.as_ptr().cast(),
                ptr::null_mut(),
                960,
                ptr::null(),
                ffi::PA_INPUT_OVERFLOW,
                stream.capture.get().cast(),
            )
        },
        ffi::PA_CONTINUE
    );
    let mut stats = StreamStats {
        struct_size: size_of::<StreamStats>() as u32,
        ..StreamStats::default()
    };
    assert_eq!(stream_get_stats(&stream, &mut stats), AUDIO_OK);
    assert_eq!(stats.capture_callback_count, 1);
    assert_eq!(stats.input_clip_sample_count, 960);
    assert_eq!(stats.input_peak, 1.0);
    assert_eq!(stats.input_rms, 1.0);
    assert_eq!(stats.input_queue_occupancy_frames, 960);
    assert_eq!(stats.capture_startup_wait_frames, 960);
    assert_eq!(stats.capture_ring_missing_frames, 0);
    barrier.wait();
    worker.join().unwrap();
}

#[test]
fn stream_start_rejects_scheduling_setup_failures_before_callbacks_start() {
    let _serial = lock_fake();
    for failure in 0..4 {
        reset_fake_functions();
        FAKE_SCHEDULING.with(|state| {
            let mut state = state.borrow_mut();
            match failure {
                0 => state.get_result = 22,
                1 => state.maximum = -1,
                2 => state.maximum = 0,
                _ => state.set_results.push_back(1), // EPERM from pthread_setschedparam.
            }
        });
        let stream = fake_stream(&ffi_stream_config());
        assert_eq!(stream_start(stream), AUDIO_PORTAUDIO_ERROR);
        FAKE_PORTAUDIO.with(|state| {
            let state = state.borrow();
            assert_eq!(
                (state.start_count, state.abort_count, state.active),
                (0, 0, 0)
            );
        });
        FAKE_SCHEDULING.with(|state| {
            let state = state.borrow();
            assert_eq!((state.policy, state.priority), (2, 10));
            assert_eq!(state.start_schedule, None);
            let expected: &[&str] = match failure {
                0 => &["self", "get"],
                1 | 2 => &["self", "get", "max"],
                _ => &["self", "get", "max", "set"],
            };
            assert_eq!(state.events, expected);
        });
        let mut stats = StreamStats {
            struct_size: size_of::<StreamStats>() as u32,
            ..StreamStats::default()
        };
        assert_eq!(stream_get_stats(stream, &mut stats), AUDIO_OK);
        assert_eq!(stats.device_error_count, 1);
        assert_eq!(stats.last_portaudio_error, ffi::PA_INTERNAL_ERROR);
        stream_destroy(stream);
    }
}

#[test]
fn failed_portaudio_start_still_restores_the_caller() {
    let _serial = lock_fake();
    reset_fake_functions();
    let stream = fake_stream(&ffi_stream_config());
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().start_result = -22);
    assert_eq!(stream_start(stream), AUDIO_PORTAUDIO_ERROR);
    FAKE_SCHEDULING.with(|state| {
        let state = state.borrow();
        assert_eq!((state.policy, state.priority), (2, 10));
        assert_eq!(state.start_schedule, Some((ffi::SCHED_FIFO, 99)));
        assert_eq!(state.set_requests, [(ffi::SCHED_FIFO, 99), (2, 10)]);
        assert_eq!(state.events, ["self", "get", "max", "set", "start", "set"]);
    });
    FAKE_PORTAUDIO.with(|state| assert_eq!(state.borrow().abort_count, 0));
    let mut stats = StreamStats {
        struct_size: size_of::<StreamStats>() as u32,
        ..StreamStats::default()
    };
    assert_eq!(stream_get_stats(stream, &mut stats), AUDIO_OK);
    assert_eq!(stats.last_portaudio_error, -22);
    stream_destroy(stream);
}

#[test]
fn scheduling_restore_failure_aborts_a_started_stream_and_never_succeeds() {
    let _serial = lock_fake();
    for (start_result, abort_result) in [(0, 0), (0, -25), (-22, 0)] {
        reset_fake_functions();
        let stream = fake_stream(&ffi_stream_config());
        FAKE_PORTAUDIO.with(|state| {
            let mut state = state.borrow_mut();
            state.start_result = start_result;
            state.abort_result = abort_result;
        });
        FAKE_SCHEDULING.with(|state| state.borrow_mut().set_results.extend([0, 1]));
        assert_eq!(stream_start(stream), AUDIO_PORTAUDIO_ERROR);
        FAKE_SCHEDULING.with(|state| {
            let state = state.borrow();
            assert_eq!(state.set_requests, [(ffi::SCHED_FIFO, 99), (2, 10)]);
            if start_result == 0 {
                assert_eq!(
                    state.events,
                    [
                        "self", "get", "max", "set", "start", "start", "set", "abort", "abort"
                    ]
                );
            } else {
                assert_eq!(state.events, ["self", "get", "max", "set", "start", "set"]);
            }
        });
        FAKE_PORTAUDIO.with(|state| {
            let state = state.borrow();
            assert_eq!(state.abort_count, 2 * u32::from(start_result == 0));
            assert_eq!(
                state.active,
                i32::from(start_result == 0 && abort_result != 0)
            );
        });
        let mut stats = StreamStats {
            struct_size: size_of::<StreamStats>() as u32,
            ..StreamStats::default()
        };
        assert_eq!(stream_get_stats(stream, &mut stats), AUDIO_OK);
        assert_eq!(
            stats.device_error_count,
            if start_result == 0 && abort_result != 0 {
                3
            } else {
                1
            }
        );
        assert_eq!(stats.last_portaudio_error, ffi::PA_INTERNAL_ERROR);
        stream_destroy(stream);
    }
}

#[test]
fn stream_control_reports_errors_and_is_idempotent() {
    let _serial = lock_fake();
    reset_fake_functions();
    assert_eq!(stream_start(ptr::null_mut()), AUDIO_INVALID_ARGUMENT);
    assert_eq!(stream_stop(ptr::null_mut()), AUDIO_INVALID_ARGUMENT);
    let config = ffi_stream_config();
    let stream = fake_stream(&config);

    FAKE_PORTAUDIO.with(|state| state.borrow_mut().force_active(1));
    assert_eq!(stream_start(stream), AUDIO_OK);
    FAKE_PORTAUDIO.with(|state| assert_eq!(state.borrow().start_count, 0));

    FAKE_PORTAUDIO.with(|state| state.borrow_mut().force_active(-21));
    assert_eq!(stream_start(stream), AUDIO_PORTAUDIO_ERROR);
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.force_active(0);
        state.start_result = -22;
    });
    assert_eq!(stream_start(stream), AUDIO_PORTAUDIO_ERROR);
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().start_result = 0);
    assert_eq!(stream_start(stream), AUDIO_OK);

    FAKE_PORTAUDIO.with(|state| state.borrow_mut().force_active(0));
    assert_eq!(stream_stop(stream), AUDIO_OK);
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().force_active(-23));
    assert_eq!(stream_stop(stream), AUDIO_PORTAUDIO_ERROR);
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.force_active(1);
        state.stop_result = -24;
    });
    assert_eq!(stream_stop(stream), AUDIO_PORTAUDIO_ERROR);
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().stop_result = 0);
    assert_eq!(stream_stop(stream), AUDIO_OK);

    let mut stats = StreamStats {
        struct_size: size_of::<StreamStats>() as u32,
        ..StreamStats::default()
    };
    assert_eq!(stream_get_stats(stream, &mut stats), AUDIO_OK);
    assert_eq!(stats.device_error_count, 6);
    assert_eq!(stats.last_portaudio_error, -24);
    stream_destroy(stream);
}

#[test]
fn stream_destroy_stops_aborts_or_closes_as_required() {
    let _serial = lock_fake();
    reset_fake_functions();
    stream_destroy(ptr::null_mut());
    let config = ffi_stream_config();
    let stream = fake_stream(&config);
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.force_active(1);
        state.stop_result = -31;
        state.abort_result = -32;
        state.close_result = -33;
    });
    stream_destroy(stream);
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.stop_count, 2);
        assert_eq!(state.abort_count, 2);
        assert_eq!(state.close_count, 2);
        assert_eq!(state.terminate_count, 0);
    });

    // Failed closes retain userdata, runtime and lease. Retry after recovery.
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.stop_result = 0;
        state.abort_result = 0;
        state.close_result = 0;
    });
    stream_destroy(stream);
    FAKE_PORTAUDIO.with(|state| assert_eq!(state.borrow().terminate_count, 1));

    reset_fake_functions();
    let recovered = fake_stream(&config);
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.force_active(1);
        state.stop_result = -34;
    });
    stream_destroy(recovered);
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.abort_count, 2);
        assert_eq!(state.close_count, 2);
    });

    reset_fake_functions();
    let stopped_cleanly = fake_stream(&config);
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().force_active(1));
    stream_destroy(stopped_cleanly);
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.stop_count, 2);
        assert_eq!(state.abort_count, 0);
        assert_eq!(state.close_count, 2);
        assert_eq!(state.terminate_count, 1);
    });

    let raw = Box::into_raw(Box::new(prepared_test_stream(
        &TEST_FUNCTIONS,
        test_config(2, 1, 1, copy_input_to_output),
    )));
    stream_destroy(raw);
}

#[test]
fn stream_stats_reject_invalid_pointers_and_short_structures() {
    let stream = prepared_test_stream(
        &ffi::PRODUCTION_FUNCTIONS,
        test_config(2, 1, 1, copy_input_to_output),
    );
    let mut stats = StreamStats::default();

    assert_eq!(
        stream_get_stats(ptr::null(), &mut stats),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        stream_get_stats(&stream, ptr::null_mut()),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        stream_get_stats(&stream, &mut stats),
        AUDIO_INVALID_ARGUMENT
    );
}

#[test]
fn callback_handles_null_input_null_output_and_zero_frames() {
    let mut stream = prepared_test_stream(
        &ffi::PRODUCTION_FUNCTIONS,
        test_config(2, 1, 1, copy_input_to_output),
    );
    let mut output = [1.0; 2];
    assert_eq!(
        unsafe { stream.process_callback(ptr::null(), output.as_mut_ptr(), 2, 0) },
        ffi::PA_CONTINUE
    );
    assert_eq!(output, [0.0; 2]);
    assert_eq!(
        unsafe { stream.process_callback(ptr::null(), ptr::null_mut(), 2, 0) },
        ffi::PA_ABORT
    );
    let mut no_frames: [f32; 0] = [];
    assert_eq!(
        unsafe { stream.process_callback(ptr::null(), no_frames.as_mut_ptr(), 0, 0) },
        ffi::PA_CONTINUE
    );
    assert_eq!(stream.stats.callback_count.load(Ordering::Acquire), 2);
    assert_eq!(
        stream
            .stats
            .native_tick_failure_count
            .load(Ordering::Acquire),
        1
    );
    assert_eq!(
        unsafe {
            portaudio_callback(
                ptr::null(),
                ptr::null_mut(),
                0,
                ptr::null(),
                0,
                ptr::null_mut(),
            )
        },
        ffi::PA_ABORT
    );
}

#[test]
fn callback_preserves_stereo_physical_channels() {
    let mut stream = prepared_test_stream(
        &ffi::PRODUCTION_FUNCTIONS,
        test_config(2, 2, 2, copy_input_to_output),
    );
    let input = [0.125, -0.25, 0.5, -0.75];
    let mut output = [0.0; 4];

    assert_eq!(
        unsafe { stream.process_callback(input.as_ptr(), output.as_mut_ptr(), 2, 0) },
        ffi::PA_CONTINUE
    );
    assert_eq!(output, input);
}

#[test]
fn mixer_create_validates_its_abi_strings_direction_and_channel() {
    let _serial = lock_fake();
    reset_fake_functions();
    let config = ffi_mixer_config(0, 0);
    let mut mixer = NonNull::<AudioMixer>::dangling().as_ptr();

    assert_eq!(
        mixer_create_with_functions(&TEST_FUNCTIONS, ptr::null(), &mut mixer),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        mixer_create_with_functions(&TEST_FUNCTIONS, &config, ptr::null_mut()),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        mixer_create(ptr::null(), ptr::null_mut()),
        AUDIO_INVALID_ARGUMENT
    );
    mixer = ptr::null_mut();

    let mut reject = |mutate: fn(&mut MixerConfig)| {
        let mut candidate = ffi_mixer_config(0, 0);
        mutate(&mut candidate);
        assert_eq!(
            mixer_create_with_functions(&TEST_FUNCTIONS, &candidate, &mut mixer),
            AUDIO_INVALID_ARGUMENT
        );
        assert!(mixer.is_null());
    };
    reject(|value| value.struct_size = 0);
    reject(|value| value.direction = 2);
    reject(|value| value.channel = 2);
    reject(|value| value.card = ptr::null());
    reject(|value| value.element = ptr::null());
    reject(|value| value.card = c"".as_ptr());
    reject(|value| value.element = c"".as_ptr());
}

#[cfg(unix)]
#[test]
fn usb_interface_mixer_create_validates_configuration_before_sysfs_lookup() {
    let _serial = lock_fake();
    reset_fake_functions();
    let root = create_test_sysfs_root("validation");
    let mut mixer = NonNull::<AudioMixer>::dangling().as_ptr();
    let config = ffi_usb_mixer_config(0, 0);

    assert_eq!(
        mixer_create_for_usb_interface_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            ptr::null(),
            &mut mixer,
        ),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        mixer_create_for_usb_interface(ptr::null(), ptr::null_mut()),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        mixer_create_for_usb_interface_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &config,
            ptr::null_mut(),
        ),
        AUDIO_INVALID_ARGUMENT
    );
    mixer = ptr::null_mut();

    let mut reject = |mutate: fn(&mut UsbMixerConfig)| {
        let mut candidate = ffi_usb_mixer_config(0, 0);
        mutate(&mut candidate);
        assert_eq!(
            mixer_create_for_usb_interface_with_functions_and_root(
                &TEST_FUNCTIONS,
                &root,
                &candidate,
                &mut mixer,
            ),
            AUDIO_INVALID_ARGUMENT
        );
        assert!(mixer.is_null());
    };
    reject(|value| value.struct_size = 0);
    reject(|value| value.direction = 2);
    reject(|value| value.channel = 2);
    reject(|value| value.usb_interface_path = ptr::null());
    reject(|value| value.element = ptr::null());
    reject(|value| value.usb_interface_path = c"".as_ptr());
    reject(|value| value.usb_interface_path = c"../3-1:1.0".as_ptr());
    reject(|value| value.usb_interface_path = c".".as_ptr());
    reject(|value| value.usb_interface_path = c"..".as_ptr());
    reject(|value| value.usb_interface_path = TEST_DEVICE_NAME_INVALID_UTF8.as_ptr().cast());
    reject(|value| value.element = c"".as_ptr());
    assert_eq!(
        mixer_create_for_usb_interface_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            &config,
            &mut mixer,
        ),
        AUDIO_UNSUPPORTED
    );
    assert!(mixer.is_null());
    fs::remove_dir_all(root).expect("remove validation sysfs root");
}

#[cfg(unix)]
#[test]
fn cm119_mixer_resolver_discovers_legacy_rx_tx_and_compatibility_paths() {
    let _serial = lock_fake();
    reset_fake_functions();
    let root = create_test_sysfs_root("cm119-mixer-paths");
    create_test_sysfs_card(&root, 4, "3-1:1.0");
    FAKE_ALSA.with(|state| {
        state.borrow_mut().inventory = vec![
            FakeMixerElement::new(
                "Mic",
                0,
                FakeMixerCapabilities {
                    capture_volume: true,
                    playback_volume: true,
                    capture_switch: true,
                    playback_switch: true,
                    capture_channels: [true, false],
                    playback_channels: [true, false],
                },
            ),
            FakeMixerElement::new(
                "Speaker",
                0,
                FakeMixerCapabilities {
                    capture_volume: false,
                    playback_volume: true,
                    capture_switch: false,
                    playback_switch: true,
                    capture_channels: [false, false],
                    playback_channels: [true, true],
                },
            ),
            FakeMixerElement::new(
                "Unused Playback",
                0,
                FakeMixerCapabilities {
                    capture_volume: false,
                    playback_volume: true,
                    capture_switch: false,
                    playback_switch: true,
                    capture_channels: [false, false],
                    playback_channels: [true, false],
                },
            ),
            FakeMixerElement::new(
                "Auto Gain Control",
                0,
                FakeMixerCapabilities {
                    capture_volume: false,
                    playback_volume: false,
                    capture_switch: false,
                    playback_switch: true,
                    capture_channels: [false, false],
                    playback_channels: [true, false],
                },
            ),
        ];
    });
    let mut paths = ffi_cm119_mixer_paths();

    assert_eq!(
        cm119_mixer_paths_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            c"3-1:1.0".as_ptr(),
            &mut paths,
        ),
        AUDIO_OK
    );
    assert_eq!(paths.abi_version, ABI_VERSION);
    assert_eq!(paths.rx_capture_path_count, 1);
    assert_eq!(paths.tx_playback_path_count, 2);
    assert_eq!(paths.sidetone_path_count, 1);
    assert_eq!(paths.rx_compatibility_switch_path_count, 1);
    assert_eq!(cm119_path_element(&paths.rx_capture_paths[0]), "Mic");
    assert_eq!(paths.rx_capture_paths[0].direction, 0);
    assert_eq!(
        paths.rx_capture_paths[0].capabilities,
        CM119_MIXER_PATH_VOLUME | CM119_MIXER_PATH_SWITCH
    );
    assert_eq!(cm119_path_element(&paths.tx_playback_paths[0]), "Speaker");
    assert_eq!(paths.tx_playback_paths[0].channel, 0);
    assert_eq!(cm119_path_element(&paths.tx_playback_paths[1]), "Speaker");
    assert_eq!(paths.tx_playback_paths[1].channel, 1);
    assert_eq!(cm119_path_element(&paths.sidetone_paths[0]), "Mic");
    assert_eq!(
        cm119_path_element(&paths.rx_compatibility_switch_paths[0]),
        "Auto Gain Control"
    );
    assert_eq!(
        paths.rx_compatibility_switch_paths[0].capabilities,
        CM119_MIXER_PATH_SWITCH
    );
    FAKE_ALSA.with(|state| {
        let state = state.borrow();
        assert_eq!(state.attached_card, "hw:4");
        assert_eq!(state.close_count, 1);
    });
    fs::remove_dir_all(root).expect("remove CM119 mixer sysfs root");
}

#[cfg(unix)]
#[test]
fn cm119_mixer_resolver_accepts_headphone_layout_and_rejects_incomplete_layouts() {
    let _serial = lock_fake();
    reset_fake_functions();
    let root = create_test_sysfs_root("cm119-headphone-paths");
    create_test_sysfs_card(&root, 4, "3-1:1.0");
    FAKE_ALSA.with(|state| {
        state.borrow_mut().inventory = vec![
            FakeMixerElement::new(
                "Mic",
                0,
                FakeMixerCapabilities {
                    capture_volume: true,
                    playback_volume: false,
                    capture_switch: true,
                    playback_switch: false,
                    capture_channels: [true, false],
                    playback_channels: [false, false],
                },
            ),
            FakeMixerElement::new(
                "Headphone",
                0,
                FakeMixerCapabilities {
                    capture_volume: false,
                    playback_volume: true,
                    capture_switch: false,
                    playback_switch: true,
                    capture_channels: [false, false],
                    playback_channels: [true, true],
                },
            ),
        ];
    });
    let mut paths = ffi_cm119_mixer_paths();
    assert_eq!(
        cm119_mixer_paths_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            c"3-1:1.0".as_ptr(),
            &mut paths,
        ),
        AUDIO_OK
    );
    assert_eq!(paths.sidetone_path_count, 0);
    assert_eq!(cm119_path_element(&paths.tx_playback_paths[0]), "Headphone");
    assert_eq!(cm119_path_element(&paths.tx_playback_paths[1]), "Headphone");

    FAKE_ALSA.with(|state| state.borrow_mut().inventory.truncate(1));
    let paths_size = paths.struct_size;
    paths.abi_version = 123;
    assert_eq!(
        cm119_mixer_paths_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            c"3-1:1.0".as_ptr(),
            &mut paths,
        ),
        AUDIO_UNSUPPORTED
    );
    assert_eq!(paths.struct_size, paths_size);
    assert_eq!(paths.abi_version, 0);
    assert_eq!(paths.tx_playback_path_count, 0);
    fs::remove_dir_all(root).expect("remove CM119 headphone sysfs root");
}

#[cfg(unix)]
#[test]
fn cm119_mixer_resolver_rejects_invalid_or_ambiguous_input_without_guessing() {
    let _serial = lock_fake();
    reset_fake_functions();
    let root = create_test_sysfs_root("cm119-mixer-validation");
    create_test_sysfs_card(&root, 4, "3-1:1.0");
    let mut paths = ffi_cm119_mixer_paths();

    assert_eq!(
        cm119_mixer_paths_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            ptr::null(),
            &mut paths,
        ),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        cm119_mixer_paths_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            c"../3-1:1.0".as_ptr(),
            &mut paths,
        ),
        AUDIO_INVALID_ARGUMENT
    );
    paths.struct_size = 0;
    assert_eq!(
        cm119_mixer_paths_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            c"3-1:1.0".as_ptr(),
            &mut paths,
        ),
        AUDIO_INVALID_ARGUMENT
    );
    paths = ffi_cm119_mixer_paths();
    create_test_sysfs_card(&root, 5, "3-1:1.0");
    assert_eq!(
        cm119_mixer_paths_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            c"3-1:1.0".as_ptr(),
            &mut paths,
        ),
        AUDIO_UNSUPPORTED
    );
    assert_eq!(paths.abi_version, 0);
    fs::remove_dir_all(root).expect("remove CM119 mixer validation sysfs root");
}

#[cfg(unix)]
#[test]
fn usb_device_identity_validates_all_identity_and_selection_arguments() {
    let _serial = lock_fake();
    reset_fake_functions();
    let root = create_test_sysfs_root("device-validation");
    let valid = ffi_usb_device_identity(c"3-1:1.0".as_ptr(), ptr::null());
    let mut selection = UsbDeviceSelection {
        struct_size: size_of::<UsbDeviceSelection>() as u32,
        ..UsbDeviceSelection::default()
    };

    assert_eq!(
        usb_device_resolve_with_functions_and_root(
            &TEST_FUNCTIONS,
            &root,
            ptr::null(),
            &mut selection,
        ),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        usb_device_resolve_with_functions_and_root(&TEST_FUNCTIONS, &root, &valid, ptr::null_mut(),),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        usb_device_resolve(ptr::null(), ptr::null_mut()),
        AUDIO_INVALID_ARGUMENT
    );

    let mut reject = |mutate: fn(&mut UsbDeviceIdentity)| {
        let mut candidate = ffi_usb_device_identity(c"3-1:1.0".as_ptr(), ptr::null());
        mutate(&mut candidate);
        assert_eq!(
            usb_device_resolve_with_functions_and_root(
                &TEST_FUNCTIONS,
                &root,
                &candidate,
                &mut selection,
            ),
            AUDIO_INVALID_ARGUMENT
        );
        assert_eq!(selection.abi_version, 0);
    };
    reject(|value| value.struct_size = 0);
    reject(|value| value.usb_interface_path = ptr::null());
    reject(|value| value.usb_serial = c"".as_ptr());
    reject(|value| value.usb_serial = c"bad\nserial".as_ptr());
    reject(|value| value.usb_interface_path = TEST_DEVICE_NAME_INVALID_UTF8.as_ptr().cast());
    reject(|value| value.usb_serial = TEST_DEVICE_NAME_INVALID_UTF8.as_ptr().cast());
    reject(|value| value.usb_interface_path = c"../3-1:1.0".as_ptr());
    reject(|value| value.input_device_channels = 0);
    reject(|value| value.output_device_channels = 3);

    selection.struct_size = 0;
    assert_eq!(
        usb_device_resolve_with_functions_and_root(&TEST_FUNCTIONS, &root, &valid, &mut selection,),
        AUDIO_INVALID_ARGUMENT
    );
    fs::remove_dir_all(root).expect("remove device validation sysfs root");
}

#[test]
fn mixer_create_releases_partial_alsa_initialization() {
    let _serial = lock_fake();
    let config = ffi_mixer_config(0, 0);
    let mut mixer = ptr::null_mut();

    reset_fake_functions();
    FAKE_ALSA.with(|state| state.borrow_mut().open_result = -41);
    assert_eq!(
        mixer_create_with_functions(&TEST_FUNCTIONS, &config, &mut mixer),
        AUDIO_ALSA_ERROR
    );
    FAKE_ALSA.with(|state| assert_eq!(state.borrow().close_count, 0));

    reset_fake_functions();
    FAKE_ALSA.with(|state| state.borrow_mut().attach_result = -42);
    assert_eq!(
        mixer_create_with_functions(&TEST_FUNCTIONS, &config, &mut mixer),
        AUDIO_ALSA_ERROR
    );
    FAKE_ALSA.with(|state| {
        let state = state.borrow();
        assert_eq!(
            (state.attach_count, state.register_count, state.load_count),
            (1, 0, 0)
        );
        assert_eq!(state.close_count, 1);
    });

    reset_fake_functions();
    FAKE_ALSA.with(|state| state.borrow_mut().register_result = -43);
    assert_eq!(
        mixer_create_with_functions(&TEST_FUNCTIONS, &config, &mut mixer),
        AUDIO_ALSA_ERROR
    );
    FAKE_ALSA.with(|state| {
        let state = state.borrow();
        assert_eq!(
            (state.attach_count, state.register_count, state.load_count),
            (1, 1, 0)
        );
        assert_eq!(state.close_count, 1);
    });

    reset_fake_functions();
    FAKE_ALSA.with(|state| state.borrow_mut().load_result = -44);
    assert_eq!(
        mixer_create_with_functions(&TEST_FUNCTIONS, &config, &mut mixer),
        AUDIO_ALSA_ERROR
    );
    FAKE_ALSA.with(|state| {
        let state = state.borrow();
        assert_eq!(
            (state.attach_count, state.register_count, state.load_count),
            (1, 1, 1)
        );
        assert_eq!(state.close_count, 1);
    });

    reset_fake_functions();
    FAKE_ALSA.with(|state| state.borrow_mut().id_malloc_result = -45);
    assert_eq!(
        mixer_create_with_functions(&TEST_FUNCTIONS, &config, &mut mixer),
        AUDIO_NO_MEMORY
    );
    FAKE_ALSA.with(|state| {
        let state = state.borrow();
        assert_eq!(state.id_malloc_count, 1);
        assert_eq!(state.id_free_count, 0);
        assert_eq!(state.close_count, 1);
    });

    reset_fake_functions();
    FAKE_ALSA.with(|state| state.borrow_mut().element_available = false);
    assert_eq!(
        mixer_create_with_functions(&TEST_FUNCTIONS, &config, &mut mixer),
        AUDIO_UNSUPPORTED
    );
    FAKE_ALSA.with(|state| {
        let state = state.borrow();
        assert_eq!(state.id_free_count, 1);
        assert_eq!(state.close_count, 1);
    });

    reset_fake_functions();
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.capture_volume_available = false;
        state.capture_switch_available = false;
    });
    assert_eq!(
        mixer_create_with_functions(&TEST_FUNCTIONS, &config, &mut mixer),
        AUDIO_UNSUPPORTED
    );
    FAKE_ALSA.with(|state| assert_eq!(state.borrow().close_count, 1));

    reset_fake_functions();
    let playback = ffi_mixer_config(1, 1);
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.playback_volume_available = false;
        state.playback_switch_available = false;
    });
    assert_eq!(
        mixer_create_with_functions(&TEST_FUNCTIONS, &playback, &mut mixer),
        AUDIO_UNSUPPORTED
    );
    FAKE_ALSA.with(|state| assert_eq!(state.borrow().close_count, 1));
}

#[test]
fn mixer_switch_only_path_is_explicit_and_rejects_volume_operations() {
    let _serial = lock_fake();
    reset_fake_functions();
    FAKE_ALSA.with(|state| state.borrow_mut().capture_volume_available = false);
    let config = ffi_mixer_config(0, 0);
    let mixer = fake_mixer(&config);
    let mut enabled = 0;
    let mut value = 0;

    assert_eq!(mixer_get_switch(mixer, &mut enabled), AUDIO_OK);
    assert_eq!(enabled, 1);
    assert_eq!(mixer_set_switch(mixer, 0), AUDIO_OK);
    assert_eq!(mixer_get_switch(mixer, &mut enabled), AUDIO_OK);
    assert_eq!(enabled, 0);
    assert_eq!(mixer_set_switch(mixer, 2), AUDIO_INVALID_ARGUMENT);
    let mut minimum = 0;
    let mut maximum = 0;
    assert_eq!(
        mixer_get_range_centibels(mixer, &mut minimum, &mut maximum),
        AUDIO_UNSUPPORTED
    );
    assert_eq!(mixer_get_steps(mixer, &mut value), AUDIO_UNSUPPORTED);
    assert_eq!(
        mixer_get_range_steps(mixer, &mut minimum, &mut maximum),
        AUDIO_UNSUPPORTED
    );
    assert_eq!(mixer_get_centibels(mixer, &mut value), AUDIO_UNSUPPORTED);
    assert_eq!(mixer_set_centibels(mixer, 0), AUDIO_UNSUPPORTED);
    assert_eq!(mixer_set_steps(mixer, 0), AUDIO_UNSUPPORTED);
    let mut normalized = 0;
    assert_eq!(
        mixer_get_normalized(mixer, &mut normalized),
        AUDIO_UNSUPPORTED
    );
    assert_eq!(mixer_set_normalized(mixer, 0), AUDIO_UNSUPPORTED);
    mixer_destroy(mixer);

    reset_fake_functions();
    FAKE_ALSA.with(|state| state.borrow_mut().capture_switch_available = false);
    let mixer = fake_mixer(&config);
    assert_eq!(mixer_get_switch(mixer, &mut enabled), AUDIO_UNSUPPORTED);
    assert_eq!(mixer_set_switch(mixer, 0), AUDIO_UNSUPPORTED);
    mixer_destroy(mixer);
}

#[test]
fn mixer_controls_support_capture_and_playback_and_report_errors() {
    let _serial = lock_fake();
    reset_fake_functions();
    let capture_config = ffi_mixer_config(0, 0);
    let capture = fake_mixer(&capture_config);
    let mut minimum = 0;
    let mut maximum = 0;
    let mut value = 0;

    assert_eq!(
        mixer_get_range_centibels(capture, &mut minimum, &mut maximum),
        AUDIO_OK
    );
    assert_eq!(mixer_get_centibels(capture, &mut value), AUDIO_OK);
    assert_eq!(mixer_set_centibels(capture, -2_100), AUDIO_OK);
    FAKE_ALSA.with(|state| assert_eq!(state.borrow().last_channel, 0));
    FAKE_ALSA.with(|state| state.borrow_mut().capture_range_result = -51);
    assert_eq!(
        mixer_get_range_centibels(capture, &mut minimum, &mut maximum),
        AUDIO_ALSA_ERROR
    );
    FAKE_ALSA.with(|state| state.borrow_mut().capture_get_result = -52);
    assert_eq!(mixer_get_centibels(capture, &mut value), AUDIO_ALSA_ERROR);
    FAKE_ALSA.with(|state| state.borrow_mut().capture_set_result = -53);
    assert_eq!(mixer_set_centibels(capture, -2_200), AUDIO_ALSA_ERROR);
    FAKE_ALSA.with(|state| state.borrow_mut().capture_step_get_result = -54);
    assert_eq!(mixer_get_steps(capture, &mut value), AUDIO_ALSA_ERROR);
    FAKE_ALSA.with(|state| state.borrow_mut().capture_step_get_result = 0);
    FAKE_ALSA.with(|state| state.borrow_mut().capture_step_set_result = -55);
    assert_eq!(mixer_set_steps(capture, 12), AUDIO_ALSA_ERROR);
    FAKE_ALSA.with(|state| state.borrow_mut().capture_step_set_result = 0);
    assert_eq!(mixer_set_steps(capture, -1), AUDIO_INVALID_ARGUMENT);
    let mut enabled = 0;
    FAKE_ALSA.with(|state| state.borrow_mut().capture_switch_get_result = -56);
    assert_eq!(mixer_get_switch(capture, &mut enabled), AUDIO_ALSA_ERROR);
    FAKE_ALSA.with(|state| state.borrow_mut().capture_switch_get_result = 0);
    FAKE_ALSA.with(|state| state.borrow_mut().capture_switch_set_result = -57);
    assert_eq!(mixer_set_switch(capture, 1), AUDIO_ALSA_ERROR);
    mixer_destroy(capture);

    reset_fake_functions();
    let playback_config = ffi_mixer_config(1, 1);
    let playback = fake_mixer(&playback_config);
    assert_eq!(
        mixer_get_range_centibels(playback, &mut minimum, &mut maximum),
        AUDIO_OK
    );
    assert_eq!(mixer_get_centibels(playback, &mut value), AUDIO_OK);
    assert_eq!(mixer_set_centibels(playback, -1_900), AUDIO_OK);
    FAKE_ALSA.with(|state| assert_eq!(state.borrow().last_channel, 1));
    FAKE_ALSA.with(|state| state.borrow_mut().playback_range_result = -54);
    assert_eq!(
        mixer_get_range_centibels(playback, &mut minimum, &mut maximum),
        AUDIO_ALSA_ERROR
    );
    FAKE_ALSA.with(|state| state.borrow_mut().playback_get_result = -55);
    assert_eq!(mixer_get_centibels(playback, &mut value), AUDIO_ALSA_ERROR);
    FAKE_ALSA.with(|state| state.borrow_mut().playback_set_result = -56);
    assert_eq!(mixer_set_centibels(playback, -1_800), AUDIO_ALSA_ERROR);
    FAKE_ALSA.with(|state| state.borrow_mut().playback_step_get_result = -57);
    assert_eq!(mixer_get_steps(playback, &mut value), AUDIO_ALSA_ERROR);
    FAKE_ALSA.with(|state| state.borrow_mut().playback_step_get_result = 0);
    FAKE_ALSA.with(|state| state.borrow_mut().playback_step_set_result = -58);
    assert_eq!(mixer_set_steps(playback, 12), AUDIO_ALSA_ERROR);
    FAKE_ALSA.with(|state| state.borrow_mut().playback_step_set_result = 0);
    assert_eq!(mixer_set_steps(playback, 32), AUDIO_INVALID_ARGUMENT);
    FAKE_ALSA.with(|state| state.borrow_mut().playback_switch_get_result = -59);
    assert_eq!(mixer_get_switch(playback, &mut enabled), AUDIO_ALSA_ERROR);
    FAKE_ALSA.with(|state| state.borrow_mut().playback_switch_get_result = 0);
    FAKE_ALSA.with(|state| state.borrow_mut().playback_switch_set_result = -60);
    assert_eq!(mixer_set_switch(playback, 1), AUDIO_ALSA_ERROR);
    mixer_destroy(playback);
}

#[test]
fn mixer_controls_propagate_refresh_failure_before_cached_access() {
    let _serial = lock_fake();
    reset_fake_functions();
    for direction in [0, 1] {
        let mixer = fake_mixer(&ffi_mixer_config(direction, 1));
        FAKE_ALSA.with(|state| {
            let mut state = state.borrow_mut();
            state.handle_events_count = 0;
            state.handle_events_result = -5;
            state.last_channel = -1;
        });
        let mut minimum = 123;
        let mut maximum = 456;
        let mut value = 789;
        let mut normalized = 321;
        assert_eq!(
            mixer_get_range_centibels(mixer, &mut minimum, &mut maximum),
            AUDIO_ALSA_ERROR
        );
        assert_eq!(mixer_get_centibels(mixer, &mut value), AUDIO_ALSA_ERROR);
        assert_eq!(mixer_set_centibels(mixer, -100), AUDIO_ALSA_ERROR);
        assert_eq!(
            mixer_get_range_steps(mixer, &mut minimum, &mut maximum),
            AUDIO_ALSA_ERROR
        );
        assert_eq!(mixer_get_steps(mixer, &mut value), AUDIO_ALSA_ERROR);
        assert_eq!(mixer_set_steps(mixer, 12), AUDIO_ALSA_ERROR);
        assert_eq!(
            mixer_get_normalized(mixer, &mut normalized),
            AUDIO_ALSA_ERROR
        );
        assert_eq!(mixer_set_normalized(mixer, 500), AUDIO_ALSA_ERROR);
        assert_eq!(mixer_get_switch(mixer, &mut normalized), AUDIO_ALSA_ERROR);
        assert_eq!(mixer_set_switch(mixer, 0), AUDIO_ALSA_ERROR);
        assert_eq!((minimum, maximum, value, normalized), (123, 456, 789, 321));
        FAKE_ALSA.with(|state| {
            let state = state.borrow();
            assert_eq!(state.handle_events_count, 10);
            assert_eq!(state.last_channel, -1);
            assert_eq!(state.steps, 18);
            assert_eq!(state.centibels, -1_200);
            assert!(state.switch_enabled);
        });
        mixer_destroy(mixer);
        FAKE_ALSA.with(|state| state.borrow_mut().handle_events_result = 0);
    }
}

/// Model ALSA's per-handle cache and full-stereo control writes.
struct FakeStereoMixerState {
    hardware: [i64; 2],
    cached: [[i64; 2]; 2],
}

thread_local! {
    static FAKE_STEREO_MIXER: RefCell<FakeStereoMixerState> = const { RefCell::new(FakeStereoMixerState {
        hardware: [151, 107],
        cached: [[151, 107]; 2],
    }) };
}

unsafe extern "C" fn fake_stereo_refresh(mixer: *mut SndMixer) -> c_int {
    let result = unsafe { (TEST_FUNCTIONS.alsa.mixer_handle_events)(mixer) };
    if result < 0 {
        return result;
    }
    let index = fake_inventory_index(mixer.cast()).expect("test mixer cache index");
    FAKE_STEREO_MIXER.with(|state| {
        let mut state = state.borrow_mut();
        state.cached[index] = state.hardware;
    });
    1
}

unsafe extern "C" fn fake_stereo_get(
    element: *mut SndMixerElem,
    channel: c_int,
    value: *mut std::ffi::c_long,
) -> c_int {
    let index = fake_inventory_index(element).expect("test mixer cache index");
    FAKE_STEREO_MIXER
        .with(|state| unsafe { *value = state.borrow().cached[index][channel as usize] });
    0
}

unsafe extern "C" fn fake_stereo_set(
    element: *mut SndMixerElem,
    channel: c_int,
    value: std::ffi::c_long,
) -> c_int {
    let index = fake_inventory_index(element).expect("test mixer cache index");
    FAKE_STEREO_MIXER.with(|state| {
        let mut state = state.borrow_mut();
        state.cached[index][channel as usize] = value;
        state.hardware = state.cached[index];
    });
    0
}

unsafe extern "C" fn fake_stereo_set_db(
    element: *mut SndMixerElem,
    channel: c_int,
    value: std::ffi::c_long,
    _direction: c_int,
) -> c_int {
    unsafe { fake_stereo_set(element, channel, value) }
}

unsafe extern "C" fn fake_stereo_get_switch(
    element: *mut SndMixerElem,
    channel: c_int,
    value: *mut c_int,
) -> c_int {
    let mut raw = 0;
    unsafe { fake_stereo_get(element, channel, &mut raw) };
    unsafe { *value = i32::from(raw != 0) };
    0
}

unsafe extern "C" fn fake_stereo_set_switch(
    element: *mut SndMixerElem,
    channel: c_int,
    value: c_int,
) -> c_int {
    unsafe { fake_stereo_set(element, channel, i64::from(value)) }
}

static STEREO_FUNCTIONS: ffi::FunctionTable = ffi::FunctionTable {
    alsa: ffi::AlsaFunctions {
        mixer_handle_events: fake_stereo_refresh,
        selem_get_playback_volume: fake_stereo_get,
        selem_set_playback_volume: fake_stereo_set,
        selem_get_playback_db: fake_stereo_get,
        selem_set_playback_db: fake_stereo_set_db,
        selem_get_playback_switch: fake_stereo_get_switch,
        selem_set_playback_switch: fake_stereo_set_switch,
        ..TEST_FUNCTIONS.alsa
    },
    ..TEST_FUNCTIONS
};

#[test]
fn mixer_refresh_preserves_sibling_channels_across_independent_handles() {
    let _serial = lock_fake();
    reset_fake_functions();
    FAKE_ALSA.with(|state| state.borrow_mut().playback_step_maximum = 151);
    FAKE_STEREO_MIXER.with(|state| {
        *state.borrow_mut() = FakeStereoMixerState {
            hardware: [151, 107],
            cached: [[151, 107]; 2],
        }
    });
    let mixers = [MixerChannel::Left, MixerChannel::Right].map(|channel| {
        let element = fake_inventory_element(channel.as_alsa_channel() as usize);
        Box::into_raw(Box::new(AudioMixer {
            functions: &STEREO_FUNCTIONS,
            mixer: element.cast(),
            element,
            channel,
            direction: MixerDirection::Playback,
        }))
    });
    assert_eq!(mixer_set_steps(mixers[0], 150), AUDIO_OK);
    assert_eq!(mixer_set_steps(mixers[1], 106), AUDIO_OK);
    FAKE_STEREO_MIXER.with(|state| assert_eq!(state.borrow().hardware, [150, 106]));
    FAKE_STEREO_MIXER.with(|state| state.borrow_mut().hardware[0] = 149);
    let mut value = 0;
    assert_eq!(mixer_get_steps(mixers[0], &mut value), AUDIO_OK);
    assert_eq!(value, 149);
    assert_eq!(mixer_set_normalized(mixers[1], 999), AUDIO_OK);
    FAKE_STEREO_MIXER.with(|state| assert_eq!(state.borrow().hardware, [149, 151]));
    let mut normalized = 0;
    assert_eq!(mixer_get_normalized(mixers[0], &mut normalized), AUDIO_OK);
    assert_eq!(normalized, normalized_from_steps(149, 0, 151).unwrap());
    assert_eq!(mixer_set_centibels(mixers[0], -1_000), AUDIO_OK);
    assert_eq!(mixer_set_centibels(mixers[1], -2_000), AUDIO_OK);
    assert_eq!(mixer_get_centibels(mixers[0], &mut value), AUDIO_OK);
    assert_eq!(value, -1_000);
    FAKE_STEREO_MIXER.with(|state| assert_eq!(state.borrow().hardware, [-1_000, -2_000]));
    FAKE_STEREO_MIXER.with(|state| {
        *state.borrow_mut() = FakeStereoMixerState {
            hardware: [1, 1],
            cached: [[1, 1]; 2],
        }
    });
    assert_eq!(mixer_set_switch(mixers[0], 0), AUDIO_OK);
    assert_eq!(mixer_set_switch(mixers[1], 0), AUDIO_OK);
    assert_eq!(mixer_get_switch(mixers[0], &mut normalized), AUDIO_OK);
    assert_eq!(normalized, 0);
    FAKE_STEREO_MIXER.with(|state| assert_eq!(state.borrow().hardware, [0, 0]));
    for mixer in mixers {
        mixer_destroy(mixer);
    }
}

#[test]
fn normalized_mixer_controls_propagate_each_relevant_alsa_error() {
    let _serial = lock_fake();
    reset_fake_functions();
    let mixer = fake_mixer(&ffi_mixer_config(0, 0));
    let mut normalized = 0;
    let mut minimum = 0;
    let mut maximum = 0;

    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.capture_step_minimum = 32;
        state.capture_step_maximum = 31;
    });
    assert_eq!(
        mixer_get_range_steps(mixer, &mut minimum, &mut maximum),
        AUDIO_ALSA_ERROR
    );
    assert_eq!(mixer_set_steps(mixer, 10), AUDIO_ALSA_ERROR);
    assert_eq!(
        mixer_get_normalized(mixer, &mut normalized),
        AUDIO_ALSA_ERROR
    );
    assert_eq!(mixer_set_normalized(mixer, 500), AUDIO_ALSA_ERROR);

    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.capture_step_minimum = 0;
        state.capture_step_maximum = 31;
        state.capture_step_get_result = -71;
    });
    assert_eq!(
        mixer_get_normalized(mixer, &mut normalized),
        AUDIO_ALSA_ERROR
    );

    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.capture_step_get_result = 0;
        state.steps = 32;
    });
    assert_eq!(
        mixer_get_normalized(mixer, &mut normalized),
        AUDIO_ALSA_ERROR
    );
    assert_eq!(mixer_set_normalized(mixer, 1_000), AUDIO_INVALID_ARGUMENT);

    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.steps = 18;
        state.capture_step_set_result = -72;
    });
    assert_eq!(mixer_set_normalized(mixer, 500), AUDIO_ALSA_ERROR);
    mixer_destroy(mixer);
}

#[test]
fn mixer_controls_reject_null_handles_and_output_pointers() {
    let mut minimum = 0;
    let mut maximum = 0;
    let mut value = 0;
    let mut normalized = 0;
    let mut enabled = 0;

    assert_eq!(
        mixer_get_range_centibels(ptr::null(), &mut minimum, &mut maximum),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        mixer_get_centibels(ptr::null(), &mut value),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        mixer_set_centibels(ptr::null_mut(), 0),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        mixer_get_range_steps(ptr::null(), &mut minimum, &mut maximum),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        mixer_get_steps(ptr::null(), &mut value),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(mixer_set_steps(ptr::null_mut(), 0), AUDIO_INVALID_ARGUMENT);
    assert_eq!(
        mixer_get_normalized(ptr::null(), &mut normalized),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        mixer_set_normalized(ptr::null_mut(), 0),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        mixer_get_switch(ptr::null(), &mut enabled),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(mixer_set_switch(ptr::null_mut(), 0), AUDIO_INVALID_ARGUMENT);

    let mixer = AudioMixer {
        functions: &TEST_FUNCTIONS,
        mixer: ptr::null_mut(),
        element: NonNull::<SndMixerElem>::dangling().as_ptr(),
        channel: MixerChannel::Left,
        direction: MixerDirection::Capture,
    };
    assert_eq!(
        mixer_get_range_centibels(&mixer, ptr::null_mut(), &mut maximum),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        mixer_get_range_centibels(&mixer, &mut minimum, ptr::null_mut()),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        mixer_get_centibels(&mixer, ptr::null_mut()),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        mixer_get_range_steps(&mixer, ptr::null_mut(), &mut maximum),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        mixer_get_range_steps(&mixer, &mut minimum, ptr::null_mut()),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        mixer_get_steps(&mixer, ptr::null_mut()),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        mixer_get_normalized(&mixer, ptr::null_mut()),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        mixer_get_switch(&mixer, ptr::null_mut()),
        AUDIO_INVALID_ARGUMENT
    );
    mixer_destroy(ptr::null_mut());
}
