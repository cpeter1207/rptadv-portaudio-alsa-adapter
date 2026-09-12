//! Allocation-free canonical PCM channel mapping and meter accumulation.

pub(crate) const CANONICAL_CHANNELS: usize = 2;

#[derive(Default)]
pub(crate) struct MeterAccumulator {
    sample_count: u64,
    clip_sample_count: u64,
    peak: f32,
    sum_squares: f64,
}

impl MeterAccumulator {
    pub(crate) fn observe(&mut self, samples: &[f32]) {
        for &sample in samples {
            let magnitude = sample.abs();

            self.sample_count = self.sample_count.saturating_add(1);
            self.sum_squares += f64::from(sample) * f64::from(sample);
            self.peak = self.peak.max(magnitude);
            if magnitude >= 1.0 {
                self.clip_sample_count = self.clip_sample_count.saturating_add(1);
            }
        }
    }

    pub(crate) fn peak(&self) -> f32 {
        self.peak
    }

    pub(crate) fn rms(&self) -> f32 {
        if self.sample_count == 0 {
            0.0
        } else {
            (self.sum_squares / self.sample_count as f64).sqrt() as f32
        }
    }

    pub(crate) fn clip_sample_count(&self) -> u64 {
        self.clip_sample_count
    }
}

pub(crate) fn device_input_to_canonical(
    device_input: Option<&[f32]>,
    device_channels: usize,
    canonical_output: &mut [f32],
) {
    debug_assert_eq!(canonical_output.len() % CANONICAL_CHANNELS, 0);
    debug_assert!(matches!(device_channels, 1 | CANONICAL_CHANNELS));

    if let Some(device_input) = device_input {
        if device_channels == 1 {
            for (frame, canonical) in canonical_output
                .chunks_exact_mut(CANONICAL_CHANNELS)
                .enumerate()
            {
                let sample = device_input[frame];
                canonical[0] = sample;
                canonical[1] = sample;
            }
        } else {
            canonical_output.copy_from_slice(device_input);
        }
    } else {
        canonical_output.fill(0.0);
    }
}

pub(crate) fn canonical_output_to_device(
    canonical_input: &[f32],
    device_channels: usize,
    device_output: &mut [f32],
) {
    debug_assert_eq!(canonical_input.len() % CANONICAL_CHANNELS, 0);
    debug_assert!(matches!(device_channels, 1 | CANONICAL_CHANNELS));

    if device_channels == 1 {
        for (frame, canonical) in canonical_input.chunks_exact(CANONICAL_CHANNELS).enumerate() {
            device_output[frame] = (canonical[0] + canonical[1]) * 0.5;
        }
    } else {
        device_output.copy_from_slice(canonical_input);
    }
}
