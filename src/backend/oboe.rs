pub use oboe::{AudioApi, PerformanceMode, SharingMode, Usage};

use super::{BackendSetup, RecorderBackendSetup, RecorderStateCell, StateCell};
use crate::{Backend, RecorderBackend};
use anyhow::Result;
use oboe::{
    AudioInputCallback, AudioInputStreamSafe, AudioOutputCallback, AudioOutputStreamSafe, AudioStream, AudioStreamAsync,
    AudioStreamBuilder, DataCallbackResult, Input, Output, Stereo,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub struct OboeSettings {
    pub buffer_size: Option<u32>,
    pub performance_mode: PerformanceMode,
    pub audio_api: AudioApi,
    pub sharing_mode: SharingMode,
    pub usage: Usage,
    pub mmap: bool,
}
impl Default for OboeSettings {
    fn default() -> Self {
        Self {
            buffer_size: None,
            performance_mode: PerformanceMode::None,
            audio_api: AudioApi::Unspecified,
            sharing_mode: SharingMode::Shared,
            usage: Usage::Media,
            mmap: true,
        }
    }
}

pub struct OboeBackend {
    settings: OboeSettings,
    stream: Option<AudioStreamAsync<Output, OboeCallback>>,
    state: Option<Arc<StateCell>>,
    broken: Arc<AtomicBool>,
}

impl OboeBackend {
    pub fn new(settings: OboeSettings) -> Self {
        Self {
            settings,
            stream: None,
            state: None,
            broken: Arc::default(),
        }
    }
}

impl Backend for OboeBackend {
    fn setup(&mut self, setup: BackendSetup) -> Result<()> {
        self.state = Some(Arc::new(setup.into()));
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        oboe::set_mmap_enabled(self.settings.mmap);
        let mut stream = AudioStreamBuilder::default()
            .set_usage(self.settings.usage)
            .set_audio_api(self.settings.audio_api)
            .set_performance_mode(self.settings.performance_mode)
            .set_sharing_mode(self.settings.sharing_mode)
            .set_channel_count::<Stereo>()
            .set_format::<f32>()
            .set_callback(OboeCallback::new(
                Arc::clone(self.state.as_ref().unwrap()),
                Arc::clone(&self.broken),
                self.settings.buffer_size,
            ))
            .open_stream()
            .unwrap();
        stream.start()?;
        self.stream = Some(stream);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if let Some(mut stream) = self.stream.take() {
            stream.close()?;
        }
        Ok(())
    }

    fn consume_broken(&self) -> bool {
        self.broken.fetch_and(false, Ordering::Relaxed)
    }
}

impl Drop for OboeBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct OboeCallback {
    state: Arc<StateCell>,
    broken: Arc<AtomicBool>,
    buffer_size: Option<u32>,
}

impl OboeCallback {
    pub fn new(state: Arc<StateCell>, broken: Arc<AtomicBool>, buffer_size: Option<u32>) -> Self {
        Self {
            state,
            broken,
            buffer_size,
        }
    }
}

impl AudioOutputCallback for OboeCallback {
    type FrameType = (f32, Stereo);

    fn on_audio_ready(
        &mut self,
        stream: &mut dyn AudioOutputStreamSafe,
        frames: &mut [(f32, f32)],
    ) -> DataCallbackResult {
        if let Some(buffer_size) = &self.buffer_size {
            let _ = stream.set_buffer_size_in_frames(
                (*buffer_size as i32).min(stream.get_buffer_size_in_frames()),
            );
        }

        let (mixer, rec) = self.state.get();
        if let Ok(latency) = stream.calculate_latency_millis() {
            rec.push(latency / 1000.);
        }
        mixer.sample_rate = stream.get_sample_rate() as u32;
        let raw = frames.as_mut_ptr();
        mixer.render_stereo(unsafe {
            std::slice::from_raw_parts_mut(raw as *mut f32, frames.len() * 2)
        });

        DataCallbackResult::Continue
    }

    fn on_error_before_close(
        &mut self,
        _audio_stream: &mut dyn oboe::AudioOutputStreamSafe,
        error: oboe::Error,
    ) {
        eprintln!("audio error: {error:?}");
        self.broken.store(true, Ordering::Relaxed);
    }

    fn on_error_after_close(
        &mut self,
        _audio_stream: &mut dyn oboe::AudioOutputStreamSafe,
        error: oboe::Error,
    ) {
        eprintln!("audio error: {error:?}");
        self.broken.store(true, Ordering::Relaxed);
    }
}

pub struct OboeRecorderBackend {
    settings: OboeSettings,
    stream: Option<AudioStreamAsync<Input, OboeRecorderCallback>>,
    state: Option<Arc<RecorderStateCell>>,
    broken: Arc<AtomicBool>,
}

impl OboeRecorderBackend {
    pub fn new(settings: OboeSettings) -> Self {
        Self {
            settings,
            stream: None,
            state: None,
            broken: Arc::default(),
        }
    }
}

impl RecorderBackend for OboeRecorderBackend {
    fn setup(&mut self, setup: RecorderBackendSetup) -> Result<()> {
        self.state = Some(Arc::new(setup.into()));
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        oboe::set_mmap_enabled(self.settings.mmap);
        let mut stream = AudioStreamBuilder::default()
            .set_input()
            .set_usage(self.settings.usage)
            .set_audio_api(self.settings.audio_api)
            .set_performance_mode(self.settings.performance_mode)
            .set_sharing_mode(self.settings.sharing_mode)
            .set_channel_count::<Stereo>()
            .set_format::<f32>()
            .set_callback(OboeRecorderCallback::new(
                Arc::clone(self.state.as_ref().unwrap()),
                Arc::clone(&self.broken),
            ))
            .open_stream()
            .unwrap();
        stream.start()?;
        self.stream = Some(stream);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if let Some(mut stream) = self.stream.take() {
            stream.close()?;
        }
        Ok(())
    }

    fn consume_broken(&self) -> bool {
        self.broken.fetch_and(false, Ordering::Relaxed)
    }
}

impl Drop for OboeRecorderBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

struct OboeRecorderCallback {
    state: Arc<RecorderStateCell>,
    broken: Arc<AtomicBool>,
}

impl OboeRecorderCallback {
    pub fn new(state: Arc<RecorderStateCell>, broken: Arc<AtomicBool>) -> Self {
        Self { state, broken }
    }
}

impl AudioInputCallback for OboeRecorderCallback {
    type FrameType = (f32, Stereo);

    fn on_audio_ready(
        &mut self,
        stream: &mut dyn AudioInputStreamSafe,
        frames: &[(f32, f32)],
    ) -> DataCallbackResult {
        let (mixer, rec) = self.state.get();
        if let Ok(latency) = stream.calculate_latency_millis() {
            rec.push(latency / 1000.);
        }
        mixer.sample_rate = stream.get_sample_rate() as u32;
        let raw = frames.as_ptr();
        mixer.record_stereo(unsafe {
            std::slice::from_raw_parts(raw as *const f32, frames.len() * 2)
        });

        DataCallbackResult::Continue
    }

    fn on_error_before_close(
        &mut self,
        _audio_stream: &mut dyn AudioInputStreamSafe,
        error: oboe::Error,
    ) {
        eprintln!("audio input error: {error:?}");
        self.broken.store(true, Ordering::Relaxed);
    }

    fn on_error_after_close(
        &mut self,
        _audio_stream: &mut dyn AudioInputStreamSafe,
        error: oboe::Error,
    ) {
        eprintln!("audio input error: {error:?}");
        self.broken.store(true, Ordering::Relaxed);
    }
}
