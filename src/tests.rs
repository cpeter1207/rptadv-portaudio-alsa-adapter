//! Rust-only unit tests for the private adapter implementation.
//!
//! Keeping test code in this file lets production coverage exclude it without
//! weakening coverage of the adapter's shipped source parts.

use super::pcm::{MeterAccumulator, canonical_output_to_device, device_input_to_canonical};
use super::*;
use std::cell::RefCell;

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
    fn reset() -> Self {
        Self {
            device_count: 2,
            default_input_device: 0,
            default_output_device: 1,
            device_info_available: true,
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
    id_malloc_count: u32,
    id_free_count: u32,
    find_count: u32,
    close_count: u32,
    centibels: i64,
    last_channel: c_int,
    open_result: c_int,
    attach_result: c_int,
    register_result: c_int,
    load_result: c_int,
    id_malloc_result: c_int,
    element_available: bool,
    capture_volume_available: bool,
    playback_volume_available: bool,
    capture_range_result: c_int,
    playback_range_result: c_int,
    capture_get_result: c_int,
    playback_get_result: c_int,
    capture_set_result: c_int,
    playback_set_result: c_int,
}

fn fake_device_info() -> ffi::PaDeviceInfo {
    ffi::PaDeviceInfo {
        struct_version: 1,
        name: TEST_DEVICE_NAME.as_ptr().cast(),
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

thread_local! {
    static FAKE_PORTAUDIO: RefCell<FakePortAudioState> = RefCell::new(FakePortAudioState::reset());
    static FAKE_ALSA: RefCell<FakeAlsaState> = RefCell::new(FakeAlsaState {
        centibels: -1_200,
        element_available: true,
        capture_volume_available: true,
        playback_volume_available: true,
        ..FakeAlsaState::default()
    });
    static FAKE_DEVICE_INFO: RefCell<ffi::PaDeviceInfo> = RefCell::new(fake_device_info());
}

static TEST_FFI_SERIAL: Mutex<()> = Mutex::new(());
static TEST_DEVICE_NAME: [u8; 5] = *b"fake\0";

fn reset_fake_functions() {
    FAKE_PORTAUDIO.with(|state| *state.borrow_mut() = FakePortAudioState::reset());
    FAKE_ALSA.with(|state| {
        *state.borrow_mut() = FakeAlsaState {
            centibels: -1_200,
            element_available: true,
            capture_volume_available: true,
            playback_volume_available: true,
            ..FakeAlsaState::default()
        };
    });
    FAKE_DEVICE_INFO.with(|info| *info.borrow_mut() = fake_device_info());
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
    let available = FAKE_PORTAUDIO.with(|state| state.borrow().device_info_available);
    if !available || !(0..2).contains(&device) {
        return ptr::null();
    }
    FAKE_DEVICE_INFO.with(RefCell::as_ptr)
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
    if stream.is_null() || input_parameters.is_null() || output_parameters.is_null() {
        return -1;
    }
    let input_parameters = unsafe { &*input_parameters };
    let output_parameters = unsafe { &*output_parameters };
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.open_count += 1;
        state.input_format = input_parameters.sample_format;
        state.output_format = output_parameters.sample_format;
        state.input_latency = input_parameters.suggested_latency;
        state.output_latency = output_parameters.suggested_latency;
        state.sample_rate = sample_rate;
        state.frames_per_buffer = frames_per_buffer;
        state.callback = callback;
        state.callback_context = callback_context;
        if state.open_result == ffi::PA_NO_ERROR {
            unsafe {
                *stream = NonNull::<PaStream>::dangling().as_ptr();
            }
        }
        state.open_result
    })
}

unsafe extern "C" fn fake_pa_start_stream(_stream: *mut PaStream) -> PaError {
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.start_count += 1;
        if state.start_result == ffi::PA_NO_ERROR {
            state.active = 1;
        }
        state.start_result
    })
}

unsafe extern "C" fn fake_pa_stop_stream(_stream: *mut PaStream) -> PaError {
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.stop_count += 1;
        if state.stop_result == ffi::PA_NO_ERROR {
            state.active = 0;
        }
        state.stop_result
    })
}

unsafe extern "C" fn fake_pa_abort_stream(_stream: *mut PaStream) -> PaError {
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.abort_count += 1;
        if state.abort_result == ffi::PA_NO_ERROR {
            state.active = 0;
        }
        state.abort_result
    })
}

unsafe extern "C" fn fake_pa_close_stream(_stream: *mut PaStream) -> PaError {
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.close_count += 1;
        state.close_result
    })
}

unsafe extern "C" fn fake_pa_is_stream_active(_stream: *mut PaStream) -> PaError {
    FAKE_PORTAUDIO.with(|state| state.borrow().active)
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

unsafe extern "C" fn fake_mixer_attach(_mixer: *mut SndMixer, _name: *const c_char) -> c_int {
    FAKE_ALSA.with(|state| {
        let mut state = state.borrow_mut();
        state.attach_count += 1;
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

unsafe extern "C" fn fake_selem_has_capture_volume(_element: *mut SndMixerElem) -> c_int {
    FAKE_ALSA.with(|state| i32::from(state.borrow().capture_volume_available))
}

unsafe extern "C" fn fake_selem_has_playback_volume(_element: *mut SndMixerElem) -> c_int {
    FAKE_ALSA.with(|state| i32::from(state.borrow().playback_volume_available))
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

static TEST_FUNCTIONS: ffi::FunctionTable = ffi::FunctionTable {
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
    },
    alsa: ffi::AlsaFunctions {
        mixer_open: fake_mixer_open,
        mixer_close: fake_mixer_close,
        mixer_attach: fake_mixer_attach,
        mixer_selem_register: fake_mixer_selem_register,
        mixer_load: fake_mixer_load,
        selem_id_malloc: fake_selem_id_malloc,
        selem_id_free: fake_selem_id_free,
        selem_id_set_name: fake_selem_id_set_name,
        selem_id_set_index: fake_selem_id_set_index,
        mixer_find_selem: fake_mixer_find_selem,
        selem_has_capture_volume: fake_selem_has_capture_volume,
        selem_has_playback_volume: fake_selem_has_playback_volume,
        selem_get_capture_db_range: fake_selem_get_capture_range,
        selem_get_playback_db_range: fake_selem_get_playback_range,
        selem_get_capture_db: fake_selem_get_capture_db,
        selem_get_playback_db: fake_selem_get_playback_db,
        selem_set_capture_db: fake_selem_set_capture_db,
        selem_set_playback_db: fake_selem_set_playback_db,
    },
};

fn fake_invoke_callback(input: &[f32], output: &mut [f32]) -> c_int {
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
        assert_eq!(state.open_count, 1);
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
    assert_eq!(output, input);
    assert_eq!(stream_stop(stream), AUDIO_OK);
    stream_destroy(stream);

    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.start_count, 1);
        assert_eq!(state.stop_count, 1);
        assert_eq!(state.close_count, 1);
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
    mixer_destroy(mixer);
    FAKE_ALSA.with(|state| assert_eq!(state.borrow().close_count, 1));
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
    let second = fake_stream(&config);
    let alternate = Box::leak(Box::new(TEST_FUNCTIONS));

    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.initialize_count, 1);
        assert_eq!(state.open_count, 2);
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
        assert_eq!(state.close_count, 2);
    });
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
    let mut stream = AudioStream::new(
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
    let mut stream = AudioStream::new(&ffi::PRODUCTION_FUNCTIONS, test_config(2, 2, 2, fail_tick));
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
    let mut stream = AudioStream::new(
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
fn stats_report_no_callback_queue_and_device_errors() {
    let stream = AudioStream::new(
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
    assert_eq!(stats.input_queue_capacity_frames, 0);
    assert_eq!(stats.input_queue_occupancy_frames, 0);
    assert_eq!(stats.output_queue_capacity_frames, 0);
    assert_eq!(stats.output_queue_occupancy_frames, 0);
    assert_eq!(stats.output_queue_dropped_frame_count, 0);
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
fn stream_control_reports_errors_and_is_idempotent() {
    let _serial = lock_fake();
    reset_fake_functions();
    assert_eq!(stream_start(ptr::null_mut()), AUDIO_INVALID_ARGUMENT);
    assert_eq!(stream_stop(ptr::null_mut()), AUDIO_INVALID_ARGUMENT);
    let config = ffi_stream_config();
    let stream = fake_stream(&config);

    FAKE_PORTAUDIO.with(|state| state.borrow_mut().active = 1);
    assert_eq!(stream_start(stream), AUDIO_OK);
    FAKE_PORTAUDIO.with(|state| assert_eq!(state.borrow().start_count, 0));

    FAKE_PORTAUDIO.with(|state| state.borrow_mut().active = -21);
    assert_eq!(stream_start(stream), AUDIO_PORTAUDIO_ERROR);
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.active = 0;
        state.start_result = -22;
    });
    assert_eq!(stream_start(stream), AUDIO_PORTAUDIO_ERROR);
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().start_result = 0);
    assert_eq!(stream_start(stream), AUDIO_OK);

    FAKE_PORTAUDIO.with(|state| state.borrow_mut().active = 0);
    assert_eq!(stream_stop(stream), AUDIO_OK);
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().active = -23);
    assert_eq!(stream_stop(stream), AUDIO_PORTAUDIO_ERROR);
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.active = 1;
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
    assert_eq!(stats.device_error_count, 4);
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
        state.active = 1;
        state.stop_result = -31;
        state.abort_result = -32;
        state.close_result = -33;
    });
    stream_destroy(stream);
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.stop_count, 1);
        assert_eq!(state.abort_count, 1);
        assert_eq!(state.close_count, 1);
        assert_eq!(state.terminate_count, 1);
    });

    reset_fake_functions();
    let recovered = fake_stream(&config);
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.active = 1;
        state.stop_result = -34;
    });
    stream_destroy(recovered);
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.abort_count, 1);
        assert_eq!(state.close_count, 1);
    });

    reset_fake_functions();
    let stopped_cleanly = fake_stream(&config);
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().active = 1);
    stream_destroy(stopped_cleanly);
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.stop_count, 1);
        assert_eq!(state.abort_count, 0);
        assert_eq!(state.close_count, 1);
        assert_eq!(state.terminate_count, 1);
    });

    let raw = Box::into_raw(Box::new(AudioStream::new(
        &TEST_FUNCTIONS,
        test_config(2, 1, 1, copy_input_to_output),
    )));
    stream_destroy(raw);
}

#[test]
fn stream_stats_reject_invalid_pointers_and_short_structures() {
    let stream = AudioStream::new(
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
    let mut stream = AudioStream::new(
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
    let mut stream = AudioStream::new(
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
    FAKE_ALSA.with(|state| state.borrow_mut().capture_volume_available = false);
    assert_eq!(
        mixer_create_with_functions(&TEST_FUNCTIONS, &config, &mut mixer),
        AUDIO_UNSUPPORTED
    );
    FAKE_ALSA.with(|state| assert_eq!(state.borrow().close_count, 1));

    reset_fake_functions();
    let playback = ffi_mixer_config(1, 1);
    FAKE_ALSA.with(|state| state.borrow_mut().playback_volume_available = false);
    assert_eq!(
        mixer_create_with_functions(&TEST_FUNCTIONS, &playback, &mut mixer),
        AUDIO_UNSUPPORTED
    );
    FAKE_ALSA.with(|state| assert_eq!(state.borrow().close_count, 1));
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
    mixer_destroy(playback);
}

#[test]
fn mixer_controls_reject_null_handles_and_output_pointers() {
    let mut minimum = 0;
    let mut maximum = 0;
    let mut value = 0;

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
    mixer_destroy(ptr::null_mut());
}
