//! The only module allowed `unsafe`: hand-written declarations for `vendor/wrapper/wrapper.h`
//! and a handle that owns one C++ `SignalsmithStretch<float>` instance.
#![allow(unsafe_code)]

use std::ffi::c_int;
use std::ptr::NonNull;

#[repr(C)]
struct RawStretch {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn signalsmith_stretch_create_preset_default(channel_count: c_int, sample_rate: f32) -> *mut RawStretch;
    fn signalsmith_stretch_create_preset_cheaper(channel_count: c_int, sample_rate: f32) -> *mut RawStretch;
    fn signalsmith_stretch_destroy(handle: *mut RawStretch);
    fn signalsmith_stretch_reset(handle: *mut RawStretch);
    fn signalsmith_stretch_input_latency(handle: *mut RawStretch) -> usize;
    fn signalsmith_stretch_output_latency(handle: *mut RawStretch) -> usize;
    fn signalsmith_stretch_seek(handle: *mut RawStretch, input: *mut f32, input_length: usize, playback_rate: f64);
    fn signalsmith_stretch_set_transpose_factor(handle: *mut RawStretch, multiplier: f32, tonality_limit: f32);
    fn signalsmith_stretch_process(handle: *mut RawStretch, input: *mut f32, input_length: usize, output: *mut f32, output_length: usize);
}

/// Owns one stretcher. Every method takes interleaved buffers whose lengths the caller
/// (`crate::Stretcher`) has already checked against the channel count.
pub(crate) struct Handle {
    ptr: NonNull<RawStretch>,
}

// SAFETY: the C++ object holds no thread-affine state; `&mut self` on every call that touches it
// gives exclusive access, so moving the handle between threads is sound.
unsafe impl Send for Handle {}

impl Handle {
    pub(crate) fn new(channels: usize, sample_rate: f32, cheaper: bool) -> Option<Handle> {
        let ch = c_int::try_from(channels).ok()?;
        // SAFETY: plain constructor; a null return (allocation failure) is mapped to None.
        let raw = unsafe {
            if cheaper {
                signalsmith_stretch_create_preset_cheaper(ch, sample_rate)
            } else {
                signalsmith_stretch_create_preset_default(ch, sample_rate)
            }
        };
        NonNull::new(raw).map(|ptr| Handle { ptr })
    }

    pub(crate) fn reset(&mut self) {
        // SAFETY: `ptr` is a live instance owned by self.
        unsafe { signalsmith_stretch_reset(self.ptr.as_ptr()) }
    }

    pub(crate) fn input_latency(&self) -> usize {
        // SAFETY: read-only query on a live instance.
        unsafe { signalsmith_stretch_input_latency(self.ptr.as_ptr()) }
    }

    pub(crate) fn output_latency(&self) -> usize {
        // SAFETY: read-only query on a live instance.
        unsafe { signalsmith_stretch_output_latency(self.ptr.as_ptr()) }
    }

    pub(crate) fn set_transpose(&mut self, multiplier: f32) {
        // SAFETY: scalar setter on a live instance.
        unsafe { signalsmith_stretch_set_transpose_factor(self.ptr.as_ptr(), multiplier, 0.0) }
    }

    /// `input` holds `frames * channels` samples (checked by the caller).
    pub(crate) fn seek(&mut self, input: &mut [f32], frames: usize, rate: f64) {
        // SAFETY: the library reads `frames * channels` floats from `input`, which the caller
        // guarantees are in bounds; it does not retain the pointer.
        unsafe { signalsmith_stretch_seek(self.ptr.as_ptr(), input.as_mut_ptr(), frames, rate) }
    }

    /// `input`/`output` hold `in_frames`/`out_frames` × channels samples (checked by the caller).
    pub(crate) fn process(&mut self, input: &mut [f32], in_frames: usize, output: &mut [f32], out_frames: usize) {
        // SAFETY: the library reads exactly `in_frames * channels` floats and writes exactly
        // `out_frames * channels`; both slices are at least that long (checked by the caller) and
        // do not alias. The wrapper only reads through its (non-const) input pointer.
        unsafe { signalsmith_stretch_process(self.ptr.as_ptr(), input.as_mut_ptr(), in_frames, output.as_mut_ptr(), out_frames) }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from a create function and is destroyed exactly once.
        unsafe { signalsmith_stretch_destroy(self.ptr.as_ptr()) }
    }
}
