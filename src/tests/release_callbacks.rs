use super::*;

fn replace_test_ring(stream: &mut AudioStream, ring: CaptureRing) {
    let ring = Arc::new(ring);
    stream.capture.get_mut().ring = Arc::clone(&ring);
    stream.playback.get_mut().ring = Arc::clone(&ring);
    stream.ring = ring;
}

#[test]
fn capture_handles_missing_input_and_reports_failed_push_and_clock_reads() {
    let mut stream = prepared_test_stream(
        &ffi::PRODUCTION_FUNCTIONS,
        test_config(2, 1, 1, copy_input_to_output),
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
    let capture = stream.capture.get_mut();
    assert_eq!(
        unsafe { capture.process_callback(ptr::null(), 0, 0) },
        ffi::PA_CONTINUE
    );
    assert_eq!(
        unsafe { capture.process_callback(ptr::null(), 3, 0) },
        ffi::PA_CONTINUE
    );
    assert_eq!(stream.ring.snapshot().unwrap().occupancy_frames, 3);
    assert_eq!(stream.stats.input_peak_bits.load(Ordering::Relaxed), 0);
    stream.stats.record_capture_overflow_timestamp(0);
    assert_eq!(
        stream
            .stats
            .callback_clock_error_count
            .load(Ordering::Relaxed),
        1
    );
    replace_test_ring(&mut stream, CaptureRing::test_with_failure("push"));
    assert_eq!(
        unsafe { stream.capture.get_mut().process_callback(ptr::null(), 2, 0) },
        ffi::PA_ABORT
    );
    assert_eq!(stream.stats.device_error_count.load(Ordering::Relaxed), 1);
}

#[test]
fn playback_silences_ring_errors_for_real_and_direct_input_providers() {
    let mut stream = prepared_test_stream(
        &ffi::PRODUCTION_FUNCTIONS,
        test_config(2, 1, 1, copy_input_to_output),
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
    replace_test_ring(&mut stream, CaptureRing::test_with_failure("observe"));
    output.fill(1.0);
    assert_eq!(
        unsafe {
            stream
                .playback
                .get_mut()
                .process_callback(output.as_mut_ptr(), 4, 0)
        },
        ffi::PA_ABORT
    );
    assert_eq!(output, [0.0; 4]);
    assert_eq!(
        unsafe {
            stream.process_callback_with_input_result(
                ptr::null(),
                output.as_mut_ptr(),
                4,
                0,
                Err(AUDIO_PORTAUDIO_ERROR),
            )
        },
        ffi::PA_ABORT
    );
    assert_eq!(stream.stats.device_error_count.load(Ordering::Relaxed), 2);
    let mut stats = StreamStats {
        struct_size: size_of::<StreamStats>() as u32,
        ..StreamStats::default()
    };
    assert_eq!(stream_get_stats(&stream, &mut stats), AUDIO_PORTAUDIO_ERROR);
}

#[test]
fn allocation_rejection_releases_device_reservation_and_runtime() {
    let _serial = lock_fake();
    reset_fake_functions();
    let mut config = ffi_stream_config();
    config.maximum_frame_count = u32::MAX;
    let mut stream = ptr::null_mut();
    assert_eq!(
        stream_create_with_functions(&TEST_FUNCTIONS, &config, &mut stream),
        AUDIO_INVALID_ARGUMENT
    );
    assert!(stream.is_null());
    FAKE_PORTAUDIO.with(|state| assert_eq!(state.borrow().terminate_count, 1));
    let stream = fake_stream(&ffi_stream_config());
    stream_destroy(stream);
}

#[test]
fn start_handles_capture_activity_stop_failure_and_ring_failures() {
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
        state.endpoint_active[1] = 0;
        state.stop_result = -18;
    });
    assert_eq!(stream_start(stream), AUDIO_PORTAUDIO_ERROR);
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.force_active(0);
        state.stop_result = 0;
    });
    let stream_ref = unsafe { &mut *stream };
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = stream_ref.ring_control.lock().unwrap();
        panic!("poison the quiescent ring control lock");
    }));
    assert!(poisoned.is_err());
    assert_eq!(stream_start(stream), AUDIO_PORTAUDIO_ERROR);
    let mut stats = StreamStats {
        struct_size: size_of::<StreamStats>() as u32,
        ..StreamStats::default()
    };
    assert_eq!(stream_get_stats(stream, &mut stats), AUDIO_PORTAUDIO_ERROR);
    stream_ref.ring_control.clear_poison();
    replace_test_ring(stream_ref, CaptureRing::test_with_failure("create"));
    assert_eq!(stream_start(stream), AUDIO_PORTAUDIO_ERROR);
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
    assert_eq!(portaudio_runtime().lock().unwrap().references, 1);
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
