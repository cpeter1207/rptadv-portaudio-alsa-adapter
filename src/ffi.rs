//! Minimal FFI declarations kept inside the external-dependency adapter.

use std::ffi::{c_char, c_double, c_int, c_long, c_uint, c_ulong, c_void};

pub(crate) type PaError = c_int;
pub(crate) type PaDeviceIndex = c_int;
pub(crate) type PaSampleFormat = c_ulong;
pub(crate) type PaStreamCallbackFlags = c_ulong;

pub(crate) const PA_NO_ERROR: PaError = 0;
pub(crate) const PA_CONTINUE: c_int = 0;
pub(crate) const PA_ABORT: c_int = 2;
pub(crate) const PA_FLOAT_32: PaSampleFormat = 0x0000_0001;
pub(crate) const PA_INPUT_OVERFLOW: PaStreamCallbackFlags = 0x0000_0002;
pub(crate) const PA_OUTPUT_UNDERFLOW: PaStreamCallbackFlags = 0x0000_0004;
pub(crate) const PA_NO_DEVICE: PaDeviceIndex = -1;

#[repr(C)]
pub(crate) struct PaStream {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct PaStreamParameters {
    pub(crate) device: PaDeviceIndex,
    pub(crate) channel_count: c_int,
    pub(crate) sample_format: PaSampleFormat,
    pub(crate) suggested_latency: c_double,
    pub(crate) host_api_specific_stream_info: *mut c_void,
}

#[repr(C)]
pub(crate) struct PaStreamCallbackTimeInfo {
    pub(crate) input_buffer_adc_time: c_double,
    pub(crate) current_time: c_double,
    pub(crate) output_buffer_dac_time: c_double,
}

#[repr(C)]
pub(crate) struct PaDeviceInfo {
    pub(crate) struct_version: c_int,
    pub(crate) name: *const c_char,
    pub(crate) host_api: c_int,
    pub(crate) max_input_channels: c_int,
    pub(crate) max_output_channels: c_int,
    pub(crate) default_low_input_latency: c_double,
    pub(crate) default_low_output_latency: c_double,
    pub(crate) default_high_input_latency: c_double,
    pub(crate) default_high_output_latency: c_double,
    pub(crate) default_sample_rate: c_double,
}

pub(crate) type PaStreamCallback = unsafe extern "C" fn(
    *const c_void,
    *mut c_void,
    c_ulong,
    *const PaStreamCallbackTimeInfo,
    PaStreamCallbackFlags,
    *mut c_void,
) -> c_int;

#[link(name = "portaudio")]
unsafe extern "C" {
    pub(crate) fn Pa_Initialize() -> PaError;
    pub(crate) fn Pa_Terminate() -> PaError;
    pub(crate) fn Pa_GetDeviceCount() -> PaError;
    pub(crate) fn Pa_GetDefaultInputDevice() -> PaDeviceIndex;
    pub(crate) fn Pa_GetDefaultOutputDevice() -> PaDeviceIndex;
    pub(crate) fn Pa_GetDeviceInfo(device: PaDeviceIndex) -> *const PaDeviceInfo;
    pub(crate) fn Pa_OpenStream(
        stream: *mut *mut PaStream,
        input_parameters: *const PaStreamParameters,
        output_parameters: *const PaStreamParameters,
        sample_rate: c_double,
        frames_per_buffer: c_ulong,
        stream_flags: c_ulong,
        stream_callback: Option<PaStreamCallback>,
        user_data: *mut c_void,
    ) -> PaError;
    pub(crate) fn Pa_StartStream(stream: *mut PaStream) -> PaError;
    pub(crate) fn Pa_StopStream(stream: *mut PaStream) -> PaError;
    pub(crate) fn Pa_AbortStream(stream: *mut PaStream) -> PaError;
    pub(crate) fn Pa_CloseStream(stream: *mut PaStream) -> PaError;
    pub(crate) fn Pa_IsStreamActive(stream: *mut PaStream) -> PaError;
}

#[repr(C)]
pub(crate) struct SndMixer {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct SndMixerElem {
    _private: [u8; 0],
}

#[repr(C)]
pub(crate) struct SndMixerSelemId {
    _private: [u8; 0],
}

#[link(name = "asound")]
unsafe extern "C" {
    pub(crate) fn snd_mixer_open(mixer: *mut *mut SndMixer, mode: c_int) -> c_int;
    pub(crate) fn snd_mixer_close(mixer: *mut SndMixer) -> c_int;
    pub(crate) fn snd_mixer_attach(mixer: *mut SndMixer, name: *const c_char) -> c_int;
    pub(crate) fn snd_mixer_selem_register(
        mixer: *mut SndMixer,
        options: *mut c_void,
        classp: *mut *mut c_void,
    ) -> c_int;
    pub(crate) fn snd_mixer_load(mixer: *mut SndMixer) -> c_int;
    pub(crate) fn snd_mixer_selem_id_malloc(id: *mut *mut SndMixerSelemId) -> c_int;
    pub(crate) fn snd_mixer_selem_id_free(id: *mut SndMixerSelemId);
    pub(crate) fn snd_mixer_selem_id_set_name(id: *mut SndMixerSelemId, name: *const c_char);
    pub(crate) fn snd_mixer_selem_id_set_index(id: *mut SndMixerSelemId, index: c_uint);
    pub(crate) fn snd_mixer_find_selem(
        mixer: *mut SndMixer,
        id: *const SndMixerSelemId,
    ) -> *mut SndMixerElem;
    pub(crate) fn snd_mixer_selem_has_capture_volume(elem: *mut SndMixerElem) -> c_int;
    pub(crate) fn snd_mixer_selem_has_playback_volume(elem: *mut SndMixerElem) -> c_int;
    pub(crate) fn snd_mixer_selem_get_capture_dB_range(
        elem: *mut SndMixerElem,
        minimum: *mut c_long,
        maximum: *mut c_long,
    ) -> c_int;
    pub(crate) fn snd_mixer_selem_get_playback_dB_range(
        elem: *mut SndMixerElem,
        minimum: *mut c_long,
        maximum: *mut c_long,
    ) -> c_int;
    pub(crate) fn snd_mixer_selem_get_capture_dB(
        elem: *mut SndMixerElem,
        channel: c_int,
        value: *mut c_long,
    ) -> c_int;
    pub(crate) fn snd_mixer_selem_get_playback_dB(
        elem: *mut SndMixerElem,
        channel: c_int,
        value: *mut c_long,
    ) -> c_int;
    pub(crate) fn snd_mixer_selem_set_capture_dB(
        elem: *mut SndMixerElem,
        channel: c_int,
        value: c_long,
        direction: c_int,
    ) -> c_int;
    pub(crate) fn snd_mixer_selem_set_playback_dB(
        elem: *mut SndMixerElem,
        channel: c_int,
        value: c_long,
        direction: c_int,
    ) -> c_int;
}

/// Immutable table of PortAudio entry points used by this adapter.
///
/// Keeping the foreign calls behind this table makes control-plane behavior
/// testable without an installed or opened audio device. Production always
/// uses [`PRODUCTION_FUNCTIONS`]; callback processing does not consult or
/// mutate the table.
#[derive(Clone, Copy)]
pub(crate) struct PortAudioFunctions {
    pub(crate) initialize: unsafe extern "C" fn() -> PaError,
    pub(crate) terminate: unsafe extern "C" fn() -> PaError,
    pub(crate) get_device_count: unsafe extern "C" fn() -> PaError,
    pub(crate) get_default_input_device: unsafe extern "C" fn() -> PaDeviceIndex,
    pub(crate) get_default_output_device: unsafe extern "C" fn() -> PaDeviceIndex,
    pub(crate) get_device_info: unsafe extern "C" fn(PaDeviceIndex) -> *const PaDeviceInfo,
    pub(crate) open_stream: unsafe extern "C" fn(
        *mut *mut PaStream,
        *const PaStreamParameters,
        *const PaStreamParameters,
        c_double,
        c_ulong,
        c_ulong,
        Option<PaStreamCallback>,
        *mut c_void,
    ) -> PaError,
    pub(crate) start_stream: unsafe extern "C" fn(*mut PaStream) -> PaError,
    pub(crate) stop_stream: unsafe extern "C" fn(*mut PaStream) -> PaError,
    pub(crate) abort_stream: unsafe extern "C" fn(*mut PaStream) -> PaError,
    pub(crate) close_stream: unsafe extern "C" fn(*mut PaStream) -> PaError,
    pub(crate) is_stream_active: unsafe extern "C" fn(*mut PaStream) -> PaError,
}

/// Immutable table of ALSA mixer entry points used by this adapter.
#[derive(Clone, Copy)]
pub(crate) struct AlsaFunctions {
    pub(crate) mixer_open: unsafe extern "C" fn(*mut *mut SndMixer, c_int) -> c_int,
    pub(crate) mixer_close: unsafe extern "C" fn(*mut SndMixer) -> c_int,
    pub(crate) mixer_attach: unsafe extern "C" fn(*mut SndMixer, *const c_char) -> c_int,
    pub(crate) mixer_selem_register:
        unsafe extern "C" fn(*mut SndMixer, *mut c_void, *mut *mut c_void) -> c_int,
    pub(crate) mixer_load: unsafe extern "C" fn(*mut SndMixer) -> c_int,
    pub(crate) selem_id_malloc: unsafe extern "C" fn(*mut *mut SndMixerSelemId) -> c_int,
    pub(crate) selem_id_free: unsafe extern "C" fn(*mut SndMixerSelemId),
    pub(crate) selem_id_set_name: unsafe extern "C" fn(*mut SndMixerSelemId, *const c_char),
    pub(crate) selem_id_set_index: unsafe extern "C" fn(*mut SndMixerSelemId, c_uint),
    pub(crate) mixer_find_selem:
        unsafe extern "C" fn(*mut SndMixer, *const SndMixerSelemId) -> *mut SndMixerElem,
    pub(crate) selem_has_capture_volume: unsafe extern "C" fn(*mut SndMixerElem) -> c_int,
    pub(crate) selem_has_playback_volume: unsafe extern "C" fn(*mut SndMixerElem) -> c_int,
    pub(crate) selem_get_capture_db_range:
        unsafe extern "C" fn(*mut SndMixerElem, *mut c_long, *mut c_long) -> c_int,
    pub(crate) selem_get_playback_db_range:
        unsafe extern "C" fn(*mut SndMixerElem, *mut c_long, *mut c_long) -> c_int,
    pub(crate) selem_get_capture_db:
        unsafe extern "C" fn(*mut SndMixerElem, c_int, *mut c_long) -> c_int,
    pub(crate) selem_get_playback_db:
        unsafe extern "C" fn(*mut SndMixerElem, c_int, *mut c_long) -> c_int,
    pub(crate) selem_set_capture_db:
        unsafe extern "C" fn(*mut SndMixerElem, c_int, c_long, c_int) -> c_int,
    pub(crate) selem_set_playback_db:
        unsafe extern "C" fn(*mut SndMixerElem, c_int, c_long, c_int) -> c_int,
}

/// Immutable PortAudio/ALSA dependency surface for one adapter instance.
#[derive(Clone, Copy)]
pub(crate) struct FunctionTable {
    pub(crate) portaudio: PortAudioFunctions,
    pub(crate) alsa: AlsaFunctions,
}

const PRODUCTION_PORTAUDIO_FUNCTIONS: PortAudioFunctions = PortAudioFunctions {
    initialize: Pa_Initialize,
    terminate: Pa_Terminate,
    get_device_count: Pa_GetDeviceCount,
    get_default_input_device: Pa_GetDefaultInputDevice,
    get_default_output_device: Pa_GetDefaultOutputDevice,
    get_device_info: Pa_GetDeviceInfo,
    open_stream: Pa_OpenStream,
    start_stream: Pa_StartStream,
    stop_stream: Pa_StopStream,
    abort_stream: Pa_AbortStream,
    close_stream: Pa_CloseStream,
    is_stream_active: Pa_IsStreamActive,
};

const PRODUCTION_ALSA_FUNCTIONS: AlsaFunctions = AlsaFunctions {
    mixer_open: snd_mixer_open,
    mixer_close: snd_mixer_close,
    mixer_attach: snd_mixer_attach,
    mixer_selem_register: snd_mixer_selem_register,
    mixer_load: snd_mixer_load,
    selem_id_malloc: snd_mixer_selem_id_malloc,
    selem_id_free: snd_mixer_selem_id_free,
    selem_id_set_name: snd_mixer_selem_id_set_name,
    selem_id_set_index: snd_mixer_selem_id_set_index,
    mixer_find_selem: snd_mixer_find_selem,
    selem_has_capture_volume: snd_mixer_selem_has_capture_volume,
    selem_has_playback_volume: snd_mixer_selem_has_playback_volume,
    selem_get_capture_db_range: snd_mixer_selem_get_capture_dB_range,
    selem_get_playback_db_range: snd_mixer_selem_get_playback_dB_range,
    selem_get_capture_db: snd_mixer_selem_get_capture_dB,
    selem_get_playback_db: snd_mixer_selem_get_playback_dB,
    selem_set_capture_db: snd_mixer_selem_set_capture_dB,
    selem_set_playback_db: snd_mixer_selem_set_playback_dB,
};

/// Production dependency table. It is immutable for the shared object's life.
pub(crate) static PRODUCTION_FUNCTIONS: FunctionTable = FunctionTable {
    portaudio: PRODUCTION_PORTAUDIO_FUNCTIONS,
    alsa: PRODUCTION_ALSA_FUNCTIONS,
};
