//! Observational backend timestamps, never a physical presentation guarantee.

use super::{ABI_VERSION, NATIVE_SAMPLE_RATE_HZ, ffi};
use std::mem::size_of;

/// Borrowed ABI-2 record for one bounded transmit-worker block.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct OutputTiming {
    /// Size of this record; consumers check it before reading optional tails.
    pub(crate) struct_size: u32,
    /// Descriptor ABI that produced this record.
    pub(crate) abi_version: u32,
    /// One only when the backend estimate passed observational validity checks.
    pub(crate) estimate_valid: u32,
    /// Initialized ABI alignment padding, reserved as zero.
    pub(crate) reserved: u32,
    /// Existing CLOCK_MONOTONIC callback-entry observation, or zero on failure.
    pub(crate) callback_monotonic_ns: u64,
    /// Backend stream-clock callback time in seconds; zero when unknown.
    pub(crate) stream_current_time_seconds: f64,
    /// First frame's backend stream-clock DAC estimate; zero when unknown.
    pub(crate) first_frame_dac_time_seconds: f64,
    /// Always UINT64_MAX: no finite physical-DAC or cross-clock error bound.
    pub(crate) maximum_error_ns: u64,
}

impl OutputTiming {
    /// Unknown retains only the independent callback-entry observation.
    pub(crate) fn unknown(callback_monotonic_ns: u64) -> Self {
        Self {
            struct_size: size_of::<Self>() as u32,
            abi_version: ABI_VERSION,
            estimate_valid: 0,
            reserved: 0,
            callback_monotonic_ns,
            stream_current_time_seconds: 0.0,
            first_frame_dac_time_seconds: 0.0,
            maximum_error_ns: u64::MAX,
        }
    }

    /// Offset from the original host block, without cumulative chunk rounding.
    pub(crate) fn at_frame(self, offset: usize) -> Self {
        let mut result = self;
        if self.estimate_valid != 0 {
            result.first_frame_dac_time_seconds += offset as f64 / f64::from(NATIVE_SAMPLE_RATE_HZ);
        }
        result
    }
}

/// Playback-owned accepted last-sample high-water marks; faults never erase them.
#[derive(Default)]
pub(crate) struct OutputTimingHistory {
    previous: Option<(f64, f64, u64)>,
}

impl OutputTimingHistory {
    /// Observe same-domain ordering without asserting that the clocks align.
    pub(crate) fn observe(
        &mut self,
        info: Option<&ffi::PaStreamCallbackTimeInfo>,
        flags: ffi::PaStreamCallbackFlags,
        monotonic_ns: u64,
        frame_count: usize,
    ) -> OutputTiming {
        let mut result = OutputTiming::unknown(monotonic_ns);
        if frame_count == 0 {
            return result;
        }
        let Some(info) = info else {
            return result;
        };
        let current = info.current_time;
        let dac = info.output_buffer_dac_time;
        let discontinuity =
            ffi::PA_OUTPUT_UNDERFLOW | ffi::PA_OUTPUT_OVERFLOW | ffi::PA_PRIMING_OUTPUT;
        if monotonic_ns == 0
            || flags & discontinuity != 0
            || !current.is_finite()
            || !dac.is_finite()
            || dac < current
            || dac + 1.0 / f64::from(NATIVE_SAMPLE_RATE_HZ) <= dac
            || self
                .previous
                .is_some_and(|(previous_current, previous_dac, previous_ns)| {
                    current < previous_current || dac < previous_dac || monotonic_ns < previous_ns
                })
        {
            return result;
        }
        // Cover the complete accepted host span, independently of worker splits.
        // Equality at the previous last sample is only a nondecreasing estimate,
        // not proof of physical delivery. A later worker abort remains conservative.
        let last_dac = dac + (frame_count - 1) as f64 / f64::from(NATIVE_SAMPLE_RATE_HZ);
        self.previous = Some((current, last_dac, monotonic_ns));
        result.estimate_valid = 1;
        result.stream_current_time_seconds = current;
        result.first_frame_dac_time_seconds = dac;
        result
    }
}
