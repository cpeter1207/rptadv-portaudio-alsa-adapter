use super::*;

#[test]
fn actual_ring_preserves_best_quality_startup_and_reports_overflow() {
    let ring = CaptureRing::new(960).unwrap();
    let mut output = [1.0; 960];
    assert_eq!(unsafe { ring.pull(&mut output) }.unwrap(), 0);
    assert!(output.iter().all(|sample| *sample == 0.0));
    assert_eq!(unsafe { ring.push(&[0.25; 960]) }.unwrap(), 960);
    assert_eq!(unsafe { ring.pull(&mut output) }.unwrap(), 0);
    let startup = ring.snapshot().unwrap();
    assert_eq!(startup.startup_silence_frames, 1920);
    assert_eq!(startup.shortfall_frames, 0);
    assert_eq!(startup.occupancy_frames, 960);
    assert_eq!(unsafe { ring.push(&[0.25; 960]) }.unwrap(), 960);
    assert_eq!(unsafe { ring.pull(&mut output) }.unwrap(), 960);
    assert!(output.iter().all(|sample| sample.is_finite()));
    assert!((output[959] - 0.25).abs() < 0.001);
    assert_eq!(ring.snapshot().unwrap().adapter_error_count, 0);
    unsafe { ring.reset() }.unwrap();
    assert_eq!(unsafe { ring.push(&[0.1; 4000]) }.unwrap(), 3840);
    let full = ring.snapshot().unwrap();
    assert_eq!(full.capacity_frames, 3840);
    assert_eq!(full.occupancy_frames, 3840);
    assert_eq!(full.dropped_frames, 160);
    assert_eq!(full.startup_silence_frames, 0);
}

#[test]
fn actual_ring_has_distinct_runtime_shortfalls_and_stopped_reset() {
    let ring = CaptureRing::new(960).unwrap();
    let mut output = [0.0; 960];
    unsafe { ring.push(&[0.125; 1920]) }.unwrap();
    for _ in 0..5 {
        unsafe { ring.pull(&mut output) }.unwrap();
    }
    let starved = ring.snapshot().unwrap();
    assert!(starved.shortfall_frames > 0);
    assert_eq!(starved.startup_silence_frames, 0);
    unsafe { ring.reset() }.unwrap();
    let reset = ring.snapshot().unwrap();
    assert_eq!(reset.occupancy_frames, 0);
    assert_eq!(reset.shortfall_frames, 0);
    assert_eq!(reset.ratio_correction_ppm, 0);
    assert_eq!(unsafe { ring.pull(&mut output) }.unwrap(), 0);
    assert_eq!(ring.snapshot().unwrap().startup_silence_frames, 960);
}

#[test]
fn wrapper_rejects_invalid_capacity_and_oversized_output() {
    assert!(matches!(CaptureRing::new(0), Err(AUDIO_INVALID_ARGUMENT)));
    assert!(matches!(
        CaptureRing::new(usize::MAX),
        Err(AUDIO_INVALID_ARGUMENT)
    ));
    assert!(matches!(
        CaptureRing::new(u32::MAX as usize),
        Err(AUDIO_INVALID_ARGUMENT)
    ));
    let ring = CaptureRing::new(960).unwrap();
    let mut output = [1.0; 961];
    assert_eq!(
        unsafe { ring.pull(&mut output) },
        Err(AUDIO_INVALID_ARGUMENT)
    );
    assert!(output.iter().all(|sample| *sample == 0.0));
}

fn simulate_clock_drift(
    ppm: i64,
    phase_ns: i64,
    playback_blocks: i64,
) -> Result<CaptureSnapshot, (i64, usize, CaptureSnapshot)> {
    const PERIOD_NS: i64 = 20_000_000;
    const JITTER_NS: [i64; 5] = [0, 350_000, -300_000, 150_000, -200_000];
    let ring = CaptureRing::new(960).unwrap();
    let input = [0.125; 960];
    let mut output = [0.0; 960];
    let mut capture_index = 0_i64;
    let mut started = false;
    for playback_index in 0_i64..playback_blocks {
        let playback_ns = playback_index * PERIOD_NS;
        loop {
            let capture_ns = phase_ns
                + capture_index * PERIOD_NS * 1_000_000 / (1_000_000 + ppm)
                + JITTER_NS[capture_index as usize % JITTER_NS.len()];
            if capture_ns > playback_ns {
                break;
            }
            let accepted = unsafe { ring.push(&input) }.unwrap();
            if accepted != 960 {
                return Err((playback_index, accepted, ring.snapshot().unwrap()));
            }
            capture_index += 1;
        }
        let real = unsafe { ring.pull(&mut output) }.unwrap();
        if real != 0 {
            started = true;
        }
        if started && real != 960 {
            return Err((playback_index, real, ring.snapshot().unwrap()));
        }
    }
    let state = ring.snapshot().unwrap();
    assert!(started);
    Ok(state)
}

#[test]
fn actual_best_quality_ring_sustains_the_observed_capture_clock_and_phase_jitter() {
    // Ten minutes at the measured-direction margin across three phases,
    // plus five minutes on either side of the measured +130 ppm offset.
    for (ppm, phase_ns, blocks) in [
        (140, 0, 30_000),
        (140, 5_000_000, 30_000),
        (140, 17_000_000, 30_000),
        (125, 17_000_000, 15_000),
        (150, 17_000_000, 15_000),
    ] {
        let state = simulate_clock_drift(ppm, phase_ns, blocks).unwrap();
        println!("capture drift ppm={ppm} phase_ns={phase_ns}: {state:?}");
        assert_eq!(state.target_frames, 1536);
        assert_eq!(state.dropped_frames, 0);
        assert_eq!(state.shortfall_frames, 0);
        assert_eq!(state.adapter_error_count, 0);
        assert!(state.startup_silence_frames > 0);
        assert!(state.occupancy_frames < state.capacity_frames);
        assert!(state.ratio_correction_ppm.abs() < 1000);
    }
}
