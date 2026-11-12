//! Exercise timing through the callback registered with the fake PortAudio host.
use super::*;

#[derive(Default)]
struct TimingProbe {
    records: [Option<OutputTiming>; 16],
    frames: [u32; 16],
    count: usize,
    fail_at: usize,
}

unsafe extern "C" fn record_timing(
    context: *mut c_void,
    output: *mut f32,
    frames: u32,
    timing: *const OutputTiming,
) -> i32 {
    let probe = unsafe { &mut *context.cast::<TimingProbe>() };
    probe.records[probe.count] = Some(unsafe { *timing });
    probe.frames[probe.count] = frames;
    probe.count += 1;
    if probe.fail_at == probe.count {
        return 1;
    }
    unsafe { std::slice::from_raw_parts_mut(output, frames as usize * 2) }.fill(0.25);
    0
}

/// Quiesce fake endpoints even when an assertion panics, releasing their lease.
struct TestStream(*mut AudioStream);

impl Drop for TestStream {
    fn drop(&mut self) {
        stream_destroy(self.0);
    }
}

fn timed_stream(probe: &mut TimingProbe) -> TestStream {
    let mut config = ffi_stream_config();
    config.transmit_worker = None;
    config.transmit_worker_context = (probe as *mut TimingProbe).cast();
    let mut stream = ptr::null_mut();
    assert_eq!(
        stream_create_with_output_timing_with_functions(
            &TEST_FUNCTIONS,
            &config,
            Some(record_timing),
            &mut stream,
        ),
        AUDIO_OK
    );
    TestStream(stream)
}

fn timestamp(current: f64, dac: f64) -> ffi::PaStreamCallbackTimeInfo {
    ffi::PaStreamCallbackTimeInfo {
        input_buffer_adc_time: f64::NAN,
        current_time: current,
        output_buffer_dac_time: dac,
    }
}

fn invoke(
    output: &mut [f32],
    info: Option<&ffi::PaStreamCallbackTimeInfo>,
    flags: ffi::PaStreamCallbackFlags,
) -> c_int {
    let (callback, context) = FAKE_PORTAUDIO.with(|state| {
        let state = state.borrow();
        (state.callback.unwrap(), state.callback_context)
    });
    unsafe {
        callback(
            ptr::null(),
            output.as_mut_ptr().cast(),
            output.len() as std::ffi::c_ulong,
            info.map_or(ptr::null(), ptr::from_ref),
            flags,
            context,
        )
    }
}

fn assert_unknown(record: OutputTiming) {
    assert_eq!(record.estimate_valid, 0);
    assert_eq!(record.stream_current_time_seconds, 0.0);
    assert_eq!(record.first_frame_dac_time_seconds, 0.0);
    assert_eq!(record.maximum_error_ns, u64::MAX);
}

#[test]
fn registered_callback_forwards_estimates_for_each_split_chunk() {
    let _guard = lock_fake();
    reset_fake_functions();
    let mut probe = TimingProbe::default();
    let _stream = timed_stream(&mut probe);
    let before = ffi::monotonic_ns();
    let mut output = [1.0; 5];
    assert_eq!(
        invoke(&mut output, Some(&timestamp(10.0, 10.02)), 0),
        ffi::PA_CONTINUE
    );
    let after = ffi::monotonic_ns();
    assert_eq!(output, [0.25; 5]);
    assert_eq!(probe.frames[..3], [2, 2, 1]);
    assert_eq!(probe.count, 3);
    assert_eq!(size_of::<OutputTiming>(), 48);
    assert_eq!(
        std::mem::offset_of!(OutputTiming, callback_monotonic_ns),
        16
    );
    assert_eq!(
        std::mem::offset_of!(OutputTiming, stream_current_time_seconds),
        24
    );
    assert_eq!(
        std::mem::offset_of!(OutputTiming, first_frame_dac_time_seconds),
        32
    );
    assert_eq!(std::mem::offset_of!(OutputTiming, maximum_error_ns), 40);
    let observation = probe.records[0].unwrap().callback_monotonic_ns;
    assert!((before..=after).contains(&observation));
    for (index, offset) in [0, 2, 4].into_iter().enumerate() {
        let timing = probe.records[index].unwrap();
        assert_eq!(timing.struct_size as usize, size_of::<OutputTiming>());
        assert_eq!(timing.abi_version, ABI_VERSION);
        assert_eq!(timing.estimate_valid, 1);
        assert_eq!(timing.callback_monotonic_ns, observation);
        assert_eq!(timing.stream_current_time_seconds, 10.0);
        assert_eq!(
            timing.first_frame_dac_time_seconds,
            10.02 + f64::from(offset) / 48_000.0
        );
        assert_eq!(timing.maximum_error_ns, u64::MAX);
    }
}

#[test]
fn invalid_backend_observations_are_unknown_across_all_chunks() {
    let _guard = lock_fake();
    for (info, flags) in [
        (None, 0),
        (Some(timestamp(f64::NAN, 10.02)), 0),
        (Some(timestamp(10.0, f64::NAN)), 0),
        (Some(timestamp(f64::INFINITY, 10.02)), 0),
        (Some(timestamp(10.0, f64::INFINITY)), 0),
        (Some(timestamp(f64::MAX, f64::MAX)), 0),
        (Some(timestamp(10.0, 9.99)), 0),
        (Some(timestamp(10.0, 10.02)), ffi::PA_OUTPUT_UNDERFLOW),
        (Some(timestamp(10.0, 10.02)), ffi::PA_OUTPUT_OVERFLOW),
        (Some(timestamp(10.0, 10.02)), ffi::PA_PRIMING_OUTPUT),
    ] {
        reset_fake_functions();
        let mut probe = TimingProbe::default();
        let _stream = timed_stream(&mut probe);
        assert_eq!(
            invoke(&mut [1.0; 5], info.as_ref(), flags),
            ffi::PA_CONTINUE
        );
        assert_eq!(probe.count, 3);
        for record in probe.records[..3].iter().flatten() {
            assert_unknown(*record);
        }
    }
}

#[test]
fn regressions_retain_high_water_marks_until_a_quiesced_restart() {
    let _guard = lock_fake();
    reset_fake_functions();
    let mut probe = TimingProbe::default();
    let stream = timed_stream(&mut probe);
    assert_eq!(stream_start(stream.0), AUDIO_OK);
    for (current, dac, expected) in [
        (40.0, 40.02, 1),
        (39.0, 40.03, 0),
        (39.5, 40.04, 0),
        (40.0, 40.01, 0),
        (40.0, 40.015, 0),
        (40.1, 40.12, 1),
        (40.1, 40.13, 1),
    ] {
        assert_eq!(
            invoke(&mut [0.0; 1], Some(&timestamp(current, dac)), 0),
            ffi::PA_CONTINUE
        );
        assert_eq!(
            probe.records[probe.count - 1].unwrap().estimate_valid,
            expected
        );
    }
    assert_eq!(stream_stop(stream.0), AUDIO_OK);
    assert_eq!(stream_start(stream.0), AUDIO_OK);
    assert_eq!(
        invoke(&mut [0.0; 1], Some(&timestamp(10.0, 10.02)), 0),
        ffi::PA_CONTINUE
    );
    assert_eq!(probe.records[7].unwrap().estimate_valid, 1);
}

#[test]
fn clock_failure_does_not_claim_a_mapping_or_reset_backend_history() {
    let mut history = OutputTimingHistory::default();
    let valid = timestamp(10.0, 10.02);
    assert_eq!(history.observe(Some(&valid), 0, 100, 1).estimate_valid, 1);
    assert_unknown(history.observe(Some(&timestamp(11.0, 11.02)), 0, 0, 1));
    assert_unknown(history.observe(Some(&timestamp(9.0, 9.02)), 0, 101, 1));
    assert_unknown(history.observe(Some(&timestamp(11.0, 11.02)), 0, 99, 1));
    assert_eq!(history.observe(Some(&valid), 0, 101, 1).estimate_valid, 1);
}

#[test]
fn timed_worker_failure_preserves_zeroed_unsubmitted_tail() {
    let _guard = lock_fake();
    reset_fake_functions();
    let mut probe = TimingProbe {
        fail_at: 2,
        ..TimingProbe::default()
    };
    let stream = timed_stream(&mut probe);
    let mut output = [1.0; 5];
    assert_eq!(
        invoke(&mut output, Some(&timestamp(10.0, 10.02)), 0),
        ffi::PA_ABORT
    );
    assert_eq!(output, [0.25, 0.25, 0.0, 0.0, 0.0]);
    assert_eq!(probe.count, 2);
    assert_eq!(fake_stream_stats(stream.0).worker_failure_count, 1);
}

#[test]
fn constructor_admission_preserves_legacy_contract_without_opening_devices() {
    let _guard = lock_fake();
    reset_fake_functions();
    let mut config = ffi_stream_config();
    config.transmit_worker = None;
    let mut stream = ptr::null_mut();
    assert_eq!(
        stream_create_with_functions(&TEST_FUNCTIONS, &config, &mut stream),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        stream_create_with_output_timing_with_functions(
            &TEST_FUNCTIONS,
            &config,
            None,
            &mut stream
        ),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        stream_create_with_output_timing(ptr::null(), Some(record_timing), &mut stream),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        stream_create_with_output_timing(&config, Some(record_timing), ptr::null_mut()),
        AUDIO_INVALID_ARGUMENT
    );
    assert_eq!(
        FAKE_PORTAUDIO.with(|state| state.borrow().initialize_count),
        0
    );
    assert!(stream.is_null());
    assert_eq!(
        DESCRIPTOR.stream_create_with_output_timing as usize,
        stream_create_with_output_timing as *const () as usize
    );
}

#[test]
fn split_callback_history_covers_the_complete_accepted_span() {
    let _guard = lock_fake();
    reset_fake_functions();
    let mut probe = TimingProbe::default();
    let _stream = timed_stream(&mut probe);
    assert_eq!(
        invoke(&mut [0.0; 5], Some(&timestamp(10.0, 10.02)), 0),
        ffi::PA_CONTINUE
    );
    assert_eq!(
        invoke(
            &mut [0.0; 3],
            Some(&timestamp(10.0, 10.02 + 2.0 / 48_000.0)),
            0
        ),
        ffi::PA_CONTINUE
    );
    assert_unknown(probe.records[3].unwrap());
    assert_unknown(probe.records[4].unwrap());
    assert_eq!(
        invoke(
            &mut [0.0; 1],
            Some(&timestamp(10.0, 10.02 + 5.0 / 48_000.0)),
            0
        ),
        ffi::PA_CONTINUE
    );
    assert_eq!(probe.records[5].unwrap().estimate_valid, 1);
}

#[test]
fn failed_quiescence_never_resets_accepted_timestamp_history() {
    let _guard = lock_fake();
    reset_fake_functions();
    let mut probe = TimingProbe::default();
    let stream = timed_stream(&mut probe);
    assert_eq!(stream_start(stream.0), AUDIO_OK);
    assert_eq!(
        invoke(&mut [0.0; 1], Some(&timestamp(40.0, 40.02)), 0),
        ffi::PA_CONTINUE
    );
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.endpoint_active[0] = 0;
        state.stop_result = -1;
        state.abort_result = -1;
    });
    assert_eq!(stream_start(stream.0), AUDIO_PORTAUDIO_ERROR);
    assert_eq!(
        invoke(&mut [0.0; 1], Some(&timestamp(10.0, 10.02)), 0),
        ffi::PA_CONTINUE
    );
    assert_unknown(probe.records[1].unwrap());
    FAKE_PORTAUDIO.with(|state| {
        let mut state = state.borrow_mut();
        state.stop_result = 0;
        state.abort_result = 0;
    });
}

#[test]
fn split_partitions_preserve_backend_sample_positions() {
    let _guard = lock_fake();
    for maximum in [1, 2, 3, 8] {
        reset_fake_functions();
        let mut probe = TimingProbe::default();
        let mut config = ffi_stream_config();
        config.maximum_transmit_frame_count = maximum;
        // A non-null ordinary callback must never run for the timed constructor.
        config.transmit_worker = Some(fail_transmit);
        config.transmit_worker_context = (&mut probe as *mut TimingProbe).cast();
        let mut stream = ptr::null_mut();
        assert_eq!(
            stream_create_with_output_timing_with_functions(
                &TEST_FUNCTIONS,
                &config,
                Some(record_timing),
                &mut stream
            ),
            AUDIO_OK
        );
        let _stream = TestStream(stream);
        assert_eq!(
            invoke(&mut [0.0; 5], Some(&timestamp(10.0, 10.02)), 0),
            ffi::PA_CONTINUE
        );
        let mut offset = 0;
        for (record, frames) in probe.records[..probe.count]
            .iter()
            .zip(&probe.frames[..probe.count])
        {
            let record = record.unwrap();
            assert_eq!(record.estimate_valid, 1);
            assert_eq!(
                record.first_frame_dac_time_seconds,
                10.02 + f64::from(offset) / 48_000.0
            );
            offset += frames;
        }
        assert_eq!(offset, 5);
    }
}

#[test]
fn zero_frame_callbacks_do_not_advance_accepted_span_history() {
    let _guard = lock_fake();
    reset_fake_functions();
    let mut probe = TimingProbe::default();
    let _stream = timed_stream(&mut probe);
    assert_eq!(
        invoke(&mut [0.0; 1], Some(&timestamp(10.0, 10.02)), 0),
        ffi::PA_CONTINUE
    );
    assert_eq!(
        invoke(&mut [], Some(&timestamp(40.0, 40.02)), 0),
        ffi::PA_CONTINUE
    );
    assert_eq!(probe.count, 1);
    assert_eq!(
        invoke(&mut [0.0; 1], Some(&timestamp(10.01, 10.03)), 0),
        ffi::PA_CONTINUE
    );
    assert_eq!(probe.records[1].unwrap().estimate_valid, 1);
}
