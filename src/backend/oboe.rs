pub use oboe::{
    AudioApi, AudioFormat, ChannelCount, ContentType, InputPreset, PerformanceMode,
    SampleRateConversionQuality, SessionId, SharingMode, StreamState, Usage,
};

use super::{BackendSetup, BackendStreamInfo, RecorderBackendSetup, RecorderStateCell, StateCell};
use crate::{Backend, RecorderBackend};
use anyhow::Result;
use oboe::{
    AudioInputCallback, AudioInputStreamSafe, AudioOutputCallback, AudioOutputStreamSafe,
    AudioStream, AudioStreamAsync, AudioStreamBase, AudioStreamSafe, AudioStreamBuilder,
    DataCallbackResult, Input, Output, Stereo,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
pub struct OboeStreamInfo {
    pub settings: OboeSettings,
    pub sample_rate: Option<i32>,
    pub channel_count: Option<ChannelCount>,
    pub format: Option<AudioFormat>,
    pub actual_buffer_size: Option<i32>,
    pub buffer_capacity: Option<i32>,
    pub frames_per_callback: Option<i32>,
    pub frames_per_burst: Option<i32>,
    pub actual_sharing_mode: Option<SharingMode>,
    pub actual_performance_mode: Option<PerformanceMode>,
    pub device_id: Option<i32>,
    pub actual_usage: Option<Usage>,
    pub content_type: Option<ContentType>,
    pub input_preset: Option<InputPreset>,
    pub session_id: Option<SessionId>,
    pub channel_conversion_allowed: Option<bool>,
    pub format_conversion_allowed: Option<bool>,
    pub sample_rate_conversion_quality: Option<SampleRateConversionQuality>,
    pub stream_state: Option<StreamState>,
    pub xrun_count: Option<i32>,
    pub frames_written: Option<i64>,
    pub frames_read: Option<i64>,
    pub latency_millis: Option<f64>,
    pub mmap_used: Option<bool>,
    pub actual_audio_api: Option<AudioApi>,
    pub available_frames: Option<i32>,
}

impl std::fmt::Display for OboeStreamInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = &self.settings;
        writeln!(f, "settings.buffer_size: {:?}", s.buffer_size)?;
        writeln!(f, "settings.performance_mode: {:?}", s.performance_mode)?;
        writeln!(f, "settings.audio_api: {:?}", s.audio_api)?;
        writeln!(f, "settings.sharing_mode: {:?}", s.sharing_mode)?;
        writeln!(f, "settings.usage: {:?}", s.usage)?;
        writeln!(f, "settings.mmap: {:?}", s.mmap)?;
        writeln!(f, "sample_rate: {:?}", self.sample_rate)?;
        writeln!(f, "channel_count: {:?}", self.channel_count)?;
        writeln!(f, "format: {:?}", self.format)?;
        writeln!(f, "actual_buffer_size: {:?}", self.actual_buffer_size)?;
        writeln!(f, "buffer_capacity: {:?}", self.buffer_capacity)?;
        writeln!(f, "frames_per_callback: {:?}", self.frames_per_callback)?;
        writeln!(f, "frames_per_burst: {:?}", self.frames_per_burst)?;
        writeln!(f, "actual_sharing_mode: {:?}", self.actual_sharing_mode)?;
        writeln!(f, "actual_performance_mode: {:?}", self.actual_performance_mode)?;
        writeln!(f, "device_id: {:?}", self.device_id)?;
        writeln!(f, "actual_usage: {:?}", self.actual_usage)?;
        writeln!(f, "content_type: {:?}", self.content_type)?;
        writeln!(f, "input_preset: {:?}", self.input_preset)?;
        writeln!(f, "session_id: {:?}", self.session_id)?;
        writeln!(f, "channel_conversion_allowed: {:?}", self.channel_conversion_allowed)?;
        writeln!(f, "format_conversion_allowed: {:?}", self.format_conversion_allowed)?;
        writeln!(f, "sample_rate_conversion_quality: {:?}", self.sample_rate_conversion_quality)?;
        writeln!(f, "stream_state: {:?}", self.stream_state)?;
        writeln!(f, "xrun_count: {:?}", self.xrun_count)?;
        writeln!(f, "frames_written: {:?}", self.frames_written)?;
        writeln!(f, "frames_read: {:?}", self.frames_read)?;
        writeln!(f, "latency_millis: {:?}", self.latency_millis)?;
        writeln!(f, "mmap_used: {:?}", self.mmap_used)?;
        writeln!(f, "actual_audio_api: {:?}", self.actual_audio_api)?;
        writeln!(f, "available_frames: {:?}", self.available_frames)?;
        Ok(())
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
    fn setup(&mut self, setup: BackendSetup) {
        self.state = Some(Arc::new(setup.into()));
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

    fn stream_info(&mut self) -> BackendStreamInfo {
        let info = OboeStreamInfo {
            settings: self.settings.clone(),
            sample_rate: self.stream.as_ref().map(|s| s.get_sample_rate()),
            channel_count: self.stream.as_ref().map(|s| s.get_channel_count()),
            format: self.stream.as_ref().map(|s| s.get_format()),
            actual_buffer_size: self.stream.as_ref().map(|s| s.get_buffer_size_in_frames()),
            buffer_capacity: self.stream.as_ref().map(|s| s.get_buffer_capacity_in_frames()),
            frames_per_callback: self.stream.as_ref().map(|s| s.get_frames_per_callback()),
            frames_per_burst: self.stream.as_mut().map(|s| s.get_frames_per_burst()),
            actual_sharing_mode: self.stream.as_ref().map(|s| s.get_sharing_mode()),
            actual_performance_mode: self.stream.as_ref().map(|s| s.get_performance_mode()),
            device_id: self.stream.as_ref().map(|s| s.get_device_id()),
            actual_usage: self.stream.as_ref().map(|s| s.get_usage()),
            content_type: self.stream.as_ref().map(|s| s.get_content_type()),
            input_preset: None,
            session_id: self.stream.as_ref().map(|s| s.get_session_id()),
            channel_conversion_allowed: self
                .stream
                .as_ref()
                .map(|s| s.is_channel_conversion_allowed()),
            format_conversion_allowed: self
                .stream
                .as_ref()
                .map(|s| s.is_format_conversion_allowed()),
            sample_rate_conversion_quality: self
                .stream
                .as_ref()
                .map(|s| s.get_sample_rate_conversion_quality()),
            stream_state: self.stream.as_ref().map(|s| s.get_state()),
            xrun_count: match self.stream.as_ref().map(|s| s.get_xrun_count()) {
                Some(Ok(v)) => Some(v),
                _ => None,
            },
            frames_written: self.stream.as_mut().map(|s| s.get_frames_written()),
            frames_read: None,
            latency_millis: match self
                .stream
                .as_mut()
                .and_then(|s| s.calculate_latency_millis().ok())
            {
                Some(v) => Some(v),
                _ => None,
            },
            mmap_used: self.stream.as_ref().map(|s| s.is_mmap_used()),
            actual_audio_api: self.stream.as_ref().map(|s| s.get_audio_api()),
            available_frames: match self
                .stream
                .as_mut()
                .and_then(|s| s.get_available_frames().ok())
            {
                Some(v) => Some(v),
                _ => None,
            },
        };
        BackendStreamInfo::Oboe(info)
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
            let _ = stream.set_buffer_size_in_frames(*buffer_size as i32);
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

    fn stream_info(&mut self) -> BackendStreamInfo {
        let info = OboeStreamInfo {
            settings: self.settings.clone(),
            sample_rate: self.stream.as_ref().map(|s| s.get_sample_rate()),
            channel_count: self.stream.as_ref().map(|s| s.get_channel_count()),
            format: self.stream.as_ref().map(|s| s.get_format()),
            actual_buffer_size: self.stream.as_ref().map(|s| s.get_buffer_size_in_frames()),
            buffer_capacity: self.stream.as_ref().map(|s| s.get_buffer_capacity_in_frames()),
            frames_per_callback: self.stream.as_ref().map(|s| s.get_frames_per_callback()),
            frames_per_burst: self.stream.as_mut().map(|s| s.get_frames_per_burst()),
            actual_sharing_mode: self.stream.as_ref().map(|s| s.get_sharing_mode()),
            actual_performance_mode: self.stream.as_ref().map(|s| s.get_performance_mode()),
            device_id: self.stream.as_ref().map(|s| s.get_device_id()),
            actual_usage: self.stream.as_ref().map(|s| s.get_usage()),
            content_type: self.stream.as_ref().map(|s| s.get_content_type()),
            input_preset: self.stream.as_ref().map(|s| s.get_input_preset()),
            session_id: self.stream.as_ref().map(|s| s.get_session_id()),
            channel_conversion_allowed: self
                .stream
                .as_ref()
                .map(|s| s.is_channel_conversion_allowed()),
            format_conversion_allowed: self
                .stream
                .as_ref()
                .map(|s| s.is_format_conversion_allowed()),
            sample_rate_conversion_quality: self
                .stream
                .as_ref()
                .map(|s| s.get_sample_rate_conversion_quality()),
            stream_state: self.stream.as_ref().map(|s| s.get_state()),
            xrun_count: match self.stream.as_ref().map(|s| s.get_xrun_count()) {
                Some(Ok(v)) => Some(v),
                _ => None,
            },
            frames_written: None,
            frames_read: self.stream.as_mut().map(|s| s.get_frames_read()),
            latency_millis: match self
                .stream
                .as_mut()
                .and_then(|s| s.calculate_latency_millis().ok())
            {
                Some(v) => Some(v),
                _ => None,
            },
            mmap_used: self.stream.as_ref().map(|s| s.is_mmap_used()),
            actual_audio_api: self.stream.as_ref().map(|s| s.get_audio_api()),
            available_frames: match self
                .stream
                .as_mut()
                .and_then(|s| s.get_available_frames().ok())
            {
                Some(v) => Some(v),
                _ => None,
            },
        };
        BackendStreamInfo::Oboe(info)
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
