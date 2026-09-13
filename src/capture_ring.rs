//! Node-prototype mono capture clock recovery through the released F32 ring.
//!
//! This wrapper does not reinterpret interleaved stereo as mono PCM. The audio
//! adapter rejects stereo capture and duplicates corrected mono at its native
//! stereo boundary. All conversion and drift recovery remain in the shared DSO.

use std::cell::UnsafeCell;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::mem::size_of;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{AUDIO_INVALID_ARGUMENT, AUDIO_NO_MEMORY, AUDIO_PORTAUDIO_ERROR, AUDIO_UNSUPPORTED};

const ABI: u32 = 2;
const RATE: u32 = 48_000;
const RESERVE: u64 = 96;
const CAPABILITY: &CStr = c"rptadv.rate-adjusting-pcm-ring.f32";

#[repr(C)]
struct Config {
    struct_size: u32,
    abi_version: u32,
    capacity_samples: u64,
    input_rate_hz: u32,
    output_rate_hz: u32,
    quality: u32,
}

#[repr(C)]
#[derive(Default)]
struct Observation {
    struct_size: u32,
    abi_version: u32,
    capacity_samples: u64,
    available_samples: u64,
    reserve_samples: u64,
    filtered_occupancy_samples: u64,
    target_samples: u64,
    ratio_correction_ppm: i32,
    reserved: u32,
    discarded_samples: u64,
    missing_samples: u64,
    consecutive_shortfall_samples: u64,
    shortfall_average_milli: u64,
    adapter_error_count: u64,
}

type Create = unsafe extern "C" fn(*const Config, *mut *mut c_void) -> c_int;
type Destroy = unsafe extern "C" fn(*mut c_void);
type Push = unsafe extern "C" fn(*mut c_void, *const f32, u64, *mut u64) -> c_int;
type Render = unsafe extern "C" fn(*mut c_void, *mut f32, u64, u64, u64, *mut u64) -> c_int;
type Observe = unsafe extern "C" fn(*const c_void, *mut Observation) -> c_int;

#[repr(C)]
struct Descriptor {
    struct_size: u32,
    abi_version: u32,
    capability_name: *const c_char,
    create: Option<Create>,
    destroy: Option<Destroy>,
    push_sample: Option<unsafe extern "C" fn(*mut c_void, f32, *mut bool) -> c_int>,
    push: Option<Push>,
    render_sample: Option<unsafe extern "C" fn(*mut c_void, *mut f32, u64, *mut bool) -> c_int>,
    render: Option<Render>,
    observe: Option<Observe>,
}

#[link(name = "rate_adjusting_pcm_ring2")]
unsafe extern "C" {
    fn rpcr2_descriptor() -> *const Descriptor;
}

/// Validated functions copied from the immutable released descriptor.
#[derive(Clone, Copy)]
struct Functions {
    create: Create,
    destroy: Destroy,
    push: Push,
    render: Render,
    observe: Observe,
}

impl Functions {
    fn released() -> Result<Self, c_int> {
        Self::from_descriptor(unsafe { rpcr2_descriptor().as_ref() })
    }

    fn from_descriptor(descriptor: Option<&Descriptor>) -> Result<Self, c_int> {
        let descriptor = descriptor.ok_or(AUDIO_UNSUPPORTED)?;
        if descriptor.struct_size < size_of::<Descriptor>() as u32
            || descriptor.abi_version != ABI
            || descriptor.capability_name.is_null()
            || unsafe { CStr::from_ptr(descriptor.capability_name) } != CAPABILITY
        {
            return Err(AUDIO_UNSUPPORTED);
        }
        Ok(Self {
            create: descriptor.create.ok_or(AUDIO_UNSUPPORTED)?,
            destroy: descriptor.destroy.ok_or(AUDIO_UNSUPPORTED)?,
            push: descriptor.push.ok_or(AUDIO_UNSUPPORTED)?,
            render: descriptor.render.ok_or(AUDIO_UNSUPPORTED)?,
            observe: descriptor.observe.ok_or(AUDIO_UNSUPPORTED)?,
        })
    }

    fn create_ring(self, capacity: u64) -> Result<NonNull<c_void>, c_int> {
        let config = Config {
            struct_size: size_of::<Config>() as u32,
            abi_version: ABI,
            capacity_samples: capacity,
            input_rate_hz: RATE,
            output_rate_hz: RATE,
            quality: 0, // Released RPCR2_QUALITY_BEST; never lower the requested quality.
        };
        let mut ring = ptr::null_mut();
        let result = unsafe { (self.create)(&config, &mut ring) };
        if result != 0 {
            if !ring.is_null() {
                unsafe { (self.destroy)(ring) };
            }
            return Err(if result == -2 {
                AUDIO_NO_MEMORY
            } else {
                AUDIO_PORTAUDIO_ERROR
            });
        }
        NonNull::new(ring).ok_or(AUDIO_PORTAUDIO_ERROR)
    }
}

/// Individually current mono-frame counters; startup silence is not an xrun.
#[derive(Debug, Default)]
pub(crate) struct CaptureSnapshot {
    pub(crate) capacity_frames: u64,
    pub(crate) occupancy_frames: u64,
    pub(crate) target_frames: u64,
    pub(crate) dropped_frames: u64,
    pub(crate) shortfall_frames: u64,
    pub(crate) startup_silence_frames: u64,
    pub(crate) adapter_error_count: u64,
    pub(crate) ratio_correction_ppm: i32,
}

/// One released mono ring shared by exactly one capture and one playback owner.
pub(crate) struct CaptureRing {
    functions: Functions,
    handle: UnsafeCell<NonNull<c_void>>,
    maximum: usize,
    capacity: u64,
    target: u64,
    prime: u64,
    primed: UnsafeCell<bool>,
    startup_silence: AtomicU64,
}

// Endpoints enforce the released SPSC contract. Only the playback owner touches
// `primed`; handle replacement additionally requires every endpoint/observer to
// be quiescent, as required by reset's unsafe contract.
unsafe impl Send for CaptureRing {}
unsafe impl Sync for CaptureRing {}

impl CaptureRing {
    /// Allocate SINC_BEST conversion and four maximum blocks before callbacks run.
    pub(crate) fn new(maximum: usize) -> Result<Self, c_int> {
        let capacity = maximum
            .checked_mul(4)
            .filter(|count| maximum != 0 && *count <= u32::MAX as usize)
            .ok_or(AUDIO_INVALID_ARGUMENT)?
            .max(512) as u64;
        // Live capture still had shortfalls at 1280 without hardware xruns.
        // This trial adds 5.333 ms of headroom: 1536 frames for 960-frame bursts.
        // This node-only policy is not a claim of arbitrary-clock qualification.
        let target = maximum as u64 + (maximum as u64 * 3 / 5).max(RESERVE);
        // A 960-frame callback must not start after only one captured block.
        // Whole-block priming also covers SINC_BEST's initial filter lookahead;
        // the 2 ms reserve is diagnostic, not a shared-ring playout threshold.
        let prime = ((target.div_ceil(maximum as u64)) * maximum as u64).max(512);
        let functions = Functions::released()?;
        let handle = functions.create_ring(capacity)?;
        Ok(Self {
            functions,
            handle: UnsafeCell::new(handle),
            maximum,
            capacity,
            target,
            prime,
            primed: UnsafeCell::new(false),
            startup_silence: AtomicU64::new(0),
        })
    }

    fn handle(&self) -> *mut c_void {
        unsafe { (*self.handle.get()).as_ptr() }
    }

    /// Publish chronological mono PCM; overflow drops only the incoming suffix.
    ///
    /// # Safety
    /// Exactly one capture owner may call push, and reset must not overlap it.
    pub(crate) unsafe fn push(&self, input: &[f32]) -> Result<usize, c_int> {
        let mut accepted = 0;
        let result = unsafe {
            (self.functions.push)(
                self.handle(),
                input.as_ptr(),
                input.len() as u64,
                &mut accepted,
            )
        };
        if result == 0 {
            Ok(accepted as usize)
        } else {
            Err(AUDIO_PORTAUDIO_ERROR)
        }
    }

    /// Render corrected mono PCM or initial silence, never waiting for capture.
    ///
    /// # Safety
    /// Exactly one playback owner may call pull, and reset must not overlap it.
    pub(crate) unsafe fn pull(&self, output: &mut [f32]) -> Result<usize, c_int> {
        if output.len() > self.maximum {
            output.fill(0.0);
            return Err(AUDIO_INVALID_ARGUMENT);
        }
        if !unsafe { *self.primed.get() } {
            output.fill(0.0);
            if self.snapshot()?.occupancy_frames < self.prime {
                self.startup_silence
                    .fetch_add(output.len() as u64, Ordering::Relaxed);
                return Ok(0);
            }
            unsafe { *self.primed.get() = true };
        }
        let mut real = 0;
        let result = unsafe {
            (self.functions.render)(
                self.handle(),
                output.as_mut_ptr(),
                output.len() as u64,
                RESERVE,
                self.target,
                &mut real,
            )
        };
        if result == 0 {
            Ok(real as usize)
        } else {
            output.fill(0.0);
            Err(AUDIO_PORTAUDIO_ERROR)
        }
    }

    /// Copy released atomic diagnostics without touching consumer DSP state.
    pub(crate) fn snapshot(&self) -> Result<CaptureSnapshot, c_int> {
        let mut observation = Observation {
            struct_size: size_of::<Observation>() as u32,
            ..Observation::default()
        };
        if unsafe { (self.functions.observe)(self.handle(), &mut observation) } != 0 {
            return Err(AUDIO_PORTAUDIO_ERROR);
        }
        Ok(CaptureSnapshot {
            capacity_frames: observation.capacity_samples,
            occupancy_frames: observation.available_samples,
            target_frames: self.target,
            dropped_frames: observation.discarded_samples,
            shortfall_frames: observation.missing_samples,
            startup_silence_frames: self.startup_silence.load(Ordering::Relaxed),
            adapter_error_count: observation.adapter_error_count,
            ratio_correction_ppm: observation.ratio_correction_ppm,
        })
    }

    /// Replace a stopped ring so restart cannot play stale captured audio.
    ///
    /// # Safety
    /// Both callback endpoints and every snapshot observer must be quiescent
    /// until this call returns. An allocation failure keeps the old ring owned.
    pub(crate) unsafe fn reset(&self) -> Result<(), c_int> {
        let replacement = self.functions.create_ring(self.capacity)?;
        let previous = unsafe { self.handle.get().replace(replacement) };
        unsafe { (self.functions.destroy)(previous.as_ptr()) };
        unsafe { *self.primed.get() = false };
        self.startup_silence.store(0, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for CaptureRing {
    fn drop(&mut self) {
        unsafe { (self.functions.destroy)(self.handle()) };
    }
}

#[cfg(test)]
#[path = "tests/capture_ring.rs"]
mod tests;
