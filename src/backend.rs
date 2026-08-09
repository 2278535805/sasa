#[cfg(feature = "cpal")]
pub mod cpal;

#[cfg(feature = "oboe")]
pub mod oboe;

#[cfg(feature = "ohos")]
pub mod ohos;

use crate::{
    mixer::{RecorderMixer, RecorderMixerCommand},
    mixer::{Mixer, RenderMixerCommand},
    LatencyRecorder,
};
use anyhow::Result;
use ringbuf::HeapConsumer;

pub struct BackendSetup {
    pub(crate) mixer_cons: HeapConsumer<RenderMixerCommand>,
    pub(crate) latency_rec: LatencyRecorder,
}

pub trait Backend {
    fn setup(&mut self, setup: BackendSetup) -> Result<()>;
    fn start(&mut self) -> Result<()>;
    fn close(&mut self) -> Result<()>;
    fn consume_broken(&self) -> bool;
}

#[repr(transparent)]
struct StateCell {
    _data: (Mixer, LatencyRecorder),
}

impl StateCell {
    pub fn get(&self) -> &mut (Mixer, LatencyRecorder) {
        #[allow(invalid_reference_casting)]
        unsafe {
            &mut *(self as *const StateCell as *const (Mixer, LatencyRecorder) as *mut _)
        }
    }
}

impl From<BackendSetup> for StateCell {
    fn from(value: BackendSetup) -> Self {
        Self {
            _data: (Mixer::new(0, value.mixer_cons), value.latency_rec),
        }
    }
}

pub struct RecorderBackendSetup {
    pub(crate) mixer_cons: HeapConsumer<RecorderMixerCommand>,
    pub(crate) latency_rec: LatencyRecorder,
}

pub trait RecorderBackend {
    fn setup(&mut self, setup: RecorderBackendSetup) -> Result<()>;
    fn start(&mut self) -> Result<()>;
    fn close(&mut self) -> Result<()>;
    fn consume_broken(&self) -> bool;
}

#[repr(transparent)]
pub(crate) struct RecorderStateCell {
    _data: (RecorderMixer, LatencyRecorder),
}

impl RecorderStateCell {
    pub fn get(&self) -> &mut (RecorderMixer, LatencyRecorder) {
        #[allow(invalid_reference_casting)]
        unsafe {
            &mut *(self as *const RecorderStateCell as *const (RecorderMixer, LatencyRecorder) as *mut _)
        }
    }
}

impl From<RecorderBackendSetup> for RecorderStateCell {
    fn from(value: RecorderBackendSetup) -> Self {
        Self {
            _data: (
                RecorderMixer::new(0, value.mixer_cons),
                value.latency_rec,
            ),
        }
    }
}
