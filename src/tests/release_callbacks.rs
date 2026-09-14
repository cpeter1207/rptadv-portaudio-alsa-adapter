use super::*;

#[derive(Default)]
struct CaptureProbe {
    calls: usize,
    frames: [u32; 4],
    samples: [f32; 12],
    sample_count: usize,
}

unsafe extern "C" fn record_capture(
    context: *mut c_void,
    input: *const f32,
    frame_count: u32,
) -> c_int {
    let probe = unsafe { &mut *context.cast::<CaptureProbe>() };
    let count = frame_count as usize * pcm::CANONICAL_CHANNELS;
    probe.frames[probe.calls] = frame_count;
    probe.samples[probe.sample_count..probe.sample_count + count]
        .copy_from_slice(unsafe { std::slice::from_raw_parts(input, count) });
    probe.calls += 1;
    probe.sample_count += count;
    0
}

unsafe extern "C" fn fail_capture(
    _context: *mut c_void,
    _input: *const f32,
    _frame_count: u32,
) -> c_int {
    1
}

#[test]
fn capture_normalizes_and_splits_without_waiting_for_playback() {
    let mut probe = CaptureProbe::default();
    let mut config = test_config(2, 1, 1, silence_output);
    config.receive_worker = record_capture;
    config.receive_worker_context = (&mut probe as *mut CaptureProbe).cast();
    let mut stream = prepared_test_stream(&ffi::PRODUCTION_FUNCTIONS, config);

    assert_eq!(
        unsafe {
            stream.capture.get_mut().process_callback(
                [0.25, -0.5, 1.0].as_ptr(),
                3,
                ffi::PA_INPUT_OVERFLOW,
            )
        },
        ffi::PA_CONTINUE
    );
    assert_eq!(probe.calls, 2);
    assert_eq!(probe.frames[..2], [2, 1]);
    assert_eq!(probe.samples[..6], [0.25, 0.25, -0.5, -0.5, 1.0, 1.0]);
    assert_eq!(
        stream.stats.capture_callback_count.load(Ordering::Relaxed),
        1
    );
    assert_eq!(
        stream
            .stats
            .oversized_callback_count
            .load(Ordering::Relaxed),
        1
    );
    assert_eq!(stream.stats.input_overflow_count.load(Ordering::Relaxed), 1);
    assert_eq!(
        stream.stats.input_clip_sample_count.load(Ordering::Relaxed),
        1
    );

    assert_eq!(
        unsafe { stream.capture.get_mut().process_callback(ptr::null(), 2, 0) },
        ffi::PA_CONTINUE
    );
    assert_eq!(probe.samples[6..10], [0.0; 4]);
    stream.stats.record_capture_overflow_timestamp(0);
    assert_eq!(
        stream
            .stats
            .callback_clock_error_count
            .load(Ordering::Relaxed),
        1
    );
    assert_eq!(
        unsafe {
            capture_callback(
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
fn worker_failures_abort_the_responsible_endpoint() {
    let mut config = test_config(2, 1, 1, fail_transmit);
    config.receive_worker = fail_capture;
    let mut stream = prepared_test_stream(&ffi::PRODUCTION_FUNCTIONS, config);
    assert_eq!(
        unsafe { stream.capture.get_mut().process_callback(ptr::null(), 2, 0) },
        ffi::PA_ABORT
    );
    let mut output = [1.0; 2];
    assert_eq!(
        unsafe {
            stream
                .playback
                .get_mut()
                .process_callback(output.as_mut_ptr(), 2, 0)
        },
        ffi::PA_ABORT
    );
    assert_eq!(output, [0.0; 2]);
    assert_eq!(stream.stats.worker_failure_count.load(Ordering::Relaxed), 2);
}

#[test]
fn playback_splits_blocks_and_handles_empty_or_missing_output() {
    let mut stream = prepared_test_stream(
        &ffi::PRODUCTION_FUNCTIONS,
        test_config(2, 1, 1, silence_output),
    );
    let mut output = [1.0_f32; 4];
    assert_eq!(
        unsafe {
            stream.playback.get_mut().process_callback(
                output.as_mut_ptr(),
                4,
                ffi::PA_OUTPUT_UNDERFLOW,
            )
        },
        ffi::PA_CONTINUE
    );
    assert_eq!(output, [0.0; 4]);
    assert_eq!(
        stream
            .stats
            .oversized_callback_count
            .load(Ordering::Relaxed),
        1
    );
    assert_eq!(
        stream.stats.output_underflow_count.load(Ordering::Relaxed),
        1
    );
    assert_eq!(
        unsafe {
            stream
                .playback
                .get_mut()
                .process_callback(ptr::null_mut(), 2, 0)
        },
        ffi::PA_ABORT
    );
    assert_eq!(
        unsafe {
            stream
                .playback
                .get_mut()
                .process_callback(output.as_mut_ptr(), 0, 0)
        },
        ffi::PA_CONTINUE
    );
    assert_eq!(
        unsafe {
            playback_callback(
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
fn start_handles_capture_activity_and_stop_failure() {
    let _serial = lock_fake();
    reset_fake_functions();
    let stream = fake_stream(&ffi_stream_config());
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().endpoint_active[0] = -17);
    assert_eq!(stream_start(stream), AUDIO_PORTAUDIO_ERROR);
    assert_eq!(
        unsafe { (*stream).stats.last_portaudio_error.load(Ordering::Relaxed) },
        -17
    );
    FAKE_PORTAUDIO.with(|state| state.borrow_mut().force_active(1));
    assert_eq!(stream_start(stream), AUDIO_OK);
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.endpoint_active[0] = 0;
        state.stop_result = -18;
    });
    assert_eq!(stream_start(stream), AUDIO_PORTAUDIO_ERROR);
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.force_active(0);
        state.stop_result = 0;
    });
    stream_destroy(stream);
}

#[test]
fn second_endpoint_failed_close_retains_live_capture_storage() {
    let _serial = lock_fake();
    reset_fake_functions();
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.open_results.extend([0, -22]);
        state.close_result = -23;
    });
    let mut stream = ptr::null_mut();
    assert_eq!(
        stream_create_with_functions(&TEST_FUNCTIONS, &ffi_stream_config(), &mut stream),
        AUDIO_PORTAUDIO_ERROR
    );
    assert!(stream.is_null());
    let capture = FAKE_PORTAUDIO.with(|state| state.borrow().capture_context);
    assert_eq!(
        unsafe { capture_callback(ptr::null(), ptr::null_mut(), 2, ptr::null(), 0, capture) },
        ffi::PA_CONTINUE
    );
    assert_eq!(
        portaudio_runtime()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .references,
        1
    );
    let mut registry = device_lease_registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    assert!(registry.claimed_devices.remove(&0));
    assert!(registry.claimed_devices.remove(&1));
    drop(registry);
    // The deliberate failed-close storage retention is the behavior under test.
    // Reset only the fake process-global accounting for independent tests.
    portaudio_release(&TEST_FUNCTIONS);
}

thread_local! {
    static CAPTURE_TIMING: RefCell<Option<ffi::PaStreamInfo>> = RefCell::new(Some(fake_stream_info()));
}

unsafe extern "C" fn start_failure_after_activation(stream: *mut PaStream) -> PaError {
    assert_eq!(unsafe { fake_pa_start_stream(stream) }, ffi::PA_NO_ERROR);
    -31
}

static LATE_START_FAILURE_FUNCTIONS: ffi::FunctionTable = ffi::FunctionTable {
    portaudio: ffi::PortAudioFunctions {
        start_stream: start_failure_after_activation,
        ..TEST_FUNCTIONS.portaudio
    },
    ..TEST_FUNCTIONS
};

#[test]
fn failed_start_aborts_an_endpoint_that_became_active_before_returning_error() {
    let _serial = lock_fake();
    reset_fake_functions();
    let mut stream = ptr::null_mut();
    assert_eq!(
        stream_create_with_functions(
            &LATE_START_FAILURE_FUNCTIONS,
            &ffi_stream_config(),
            &mut stream,
        ),
        AUDIO_OK
    );
    assert_eq!(stream_start(stream), AUDIO_PORTAUDIO_ERROR);
    FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        assert_eq!(state.abort_count, 1);
        assert!(state.endpoint_active.iter().all(|active| *active == 0));
    });
    stream_destroy(stream);
}

unsafe extern "C" fn separate_stream_info(stream: *mut PaStream) -> *const ffi::PaStreamInfo {
    if stream as usize == 1 {
        CAPTURE_TIMING.with(|info| {
            info.borrow()
                .as_ref()
                .map_or(ptr::null(), |value| value as *const _)
        })
    } else {
        unsafe { fake_pa_get_stream_info(stream) }
    }
}

static SEPARATE_TIMING_FUNCTIONS: ffi::FunctionTable = ffi::FunctionTable {
    portaudio: ffi::PortAudioFunctions {
        get_stream_info: separate_stream_info,
        ..TEST_FUNCTIONS.portaudio
    },
    ..TEST_FUNCTIONS
};

#[test]
fn timing_validates_each_independent_stream_before_publishing() {
    let _serial = lock_fake();
    reset_fake_functions();
    let mut stream = ptr::null_mut();
    assert_eq!(
        stream_create_with_functions(
            &SEPARATE_TIMING_FUNCTIONS,
            &ffi_stream_config(),
            &mut stream
        ),
        AUDIO_OK
    );
    for invalid in 0..6 {
        CAPTURE_TIMING.with(|info| {
            let mut value = fake_stream_info();
            match invalid {
                0 => {
                    *info.borrow_mut() = None;
                    return;
                }
                1 => value.struct_version = -1,
                2 => value.sample_rate = f64::NAN,
                3 => value.sample_rate = 0.0,
                4 => value.sample_rate = 48_000.0,
                5 => FAKE_STREAM_INFO.with(|output| output.borrow_mut().output_latency = -0.1),
                _ => unreachable!(),
            }
            *info.borrow_mut() = Some(value);
        });
        let mut timing = ffi_stream_timing();
        assert_eq!(
            stream_get_timing(stream, &mut timing),
            AUDIO_PORTAUDIO_ERROR
        );
        assert_eq!(timing.abi_version, 0);
    }
    stream_destroy(stream);
}
