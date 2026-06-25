use std::convert::TryInto;
use std::ffi::c_void;
use std::os::raw::{c_float, c_int};
use std::ptr::{self, NonNull};
use std::slice;

use crate::{SuppressionParams, FRAME_SIZE};

#[repr(C)]
struct RawDenoiseState {
    _private: [u8; 0],
}

#[repr(C)]
struct RawRnnModel {
    _private: [u8; 0],
}

unsafe extern "C" {
    static rnnoise_v2_weights: u8;
    static rnnoise_v2_weights_end: u8;

    fn rnnoise_model_from_buffer(ptr: *const c_void, len: c_int) -> *mut RawRnnModel;
    fn rnnoise_model_free(model: *mut RawRnnModel);
    fn rnnoise_create(model: *mut RawRnnModel) -> *mut RawDenoiseState;
    fn rnnoise_clone(st: *mut RawDenoiseState) -> *mut RawDenoiseState;
    fn rnnoise_destroy(st: *mut RawDenoiseState);
    fn rnnoise_set_suppression_params(
        st: *mut RawDenoiseState,
        gain_exponent: c_float,
        attack: c_float,
    );
    fn rnnoise_process_frame(
        st: *mut RawDenoiseState,
        out: *mut c_float,
        input: *const c_float,
    ) -> c_float;
}

pub(crate) struct V2DenoiseState {
    state: NonNull<RawDenoiseState>,
    model: NonNull<RawRnnModel>,
}

unsafe impl Send for V2DenoiseState {}
unsafe impl Sync for V2DenoiseState {}

impl V2DenoiseState {
    pub(crate) fn new() -> Self {
        let model = create_model();
        let state = unsafe { rnnoise_create(model.as_ptr()) };
        let Some(state) = NonNull::new(state) else {
            unsafe {
                rnnoise_model_free(model.as_ptr());
            }
            panic!("failed to create RNNoise V2 state");
        };
        Self { state, model }
    }

    pub(crate) fn set_suppression_params(&mut self, params: SuppressionParams) {
        unsafe {
            rnnoise_set_suppression_params(
                self.state.as_ptr(),
                params.gain_exponent,
                params.attack,
            );
        }
    }

    pub(crate) fn process_frame(&mut self, output: &mut [f32], input: &[f32]) -> f32 {
        assert_eq!(output.len(), FRAME_SIZE);
        assert_eq!(input.len(), FRAME_SIZE);
        unsafe { rnnoise_process_frame(self.state.as_ptr(), output.as_mut_ptr(), input.as_ptr()) }
    }
}

impl Clone for V2DenoiseState {
    fn clone(&self) -> Self {
        let model = create_model();
        let state = unsafe { rnnoise_clone(self.state.as_ptr()) };
        let Some(state) = NonNull::new(state) else {
            unsafe {
                rnnoise_model_free(model.as_ptr());
            }
            panic!("failed to clone RNNoise V2 state");
        };
        Self { state, model }
    }
}

impl Drop for V2DenoiseState {
    fn drop(&mut self) {
        unsafe {
            rnnoise_destroy(self.state.as_ptr());
            rnnoise_model_free(self.model.as_ptr());
        }
    }
}

fn create_model() -> NonNull<RawRnnModel> {
    let model = model_bytes();
    let len: c_int = model
        .len()
        .try_into()
        .expect("RNNoise V2 model blob length exceeds c_int");
    let model = unsafe { rnnoise_model_from_buffer(model.as_ptr().cast(), len) };
    NonNull::new(model).expect("failed to load RNNoise V2 model blob")
}

fn model_bytes() -> &'static [u8] {
    unsafe {
        let start = ptr::addr_of!(rnnoise_v2_weights);
        let end = ptr::addr_of!(rnnoise_v2_weights_end);
        let len = end as usize - start as usize;
        slice::from_raw_parts(start, len)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn embedded_model_blob_is_avx_aligned() {
        assert_eq!(super::model_bytes().as_ptr() as usize % 32, 0);
    }
}
