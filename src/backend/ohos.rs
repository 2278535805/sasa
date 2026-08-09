use anyhow::Result;
use std::{
    ffi::c_void,
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use ohos_audio_sys::{
    OH_AudioCapturer, OH_AudioCapturer_Callbacks, OH_AudioCapturer_GetSamplingRate,
    OH_AudioCapturer_Release, OH_AudioCapturer_Start, OH_AudioInterrupt_ForceType,
    OH_AudioInterrupt_Hint, OH_AudioInterrupt_Hint_AUDIOSTREAM_INTERRUPT_HINT_PAUSE,
    OH_AudioInterrupt_Hint_AUDIOSTREAM_INTERRUPT_HINT_STOP, OH_AudioRenderer,
    OH_AudioRenderer_Callbacks, OH_AudioRenderer_GetSamplingRate, OH_AudioRenderer_Release,
    OH_AudioRenderer_Start, OH_AudioStreamBuilder, OH_AudioStreamBuilder_Create,
    OH_AudioStreamBuilder_Destroy, OH_AudioStreamBuilder_GenerateCapturer,
    OH_AudioStreamBuilder_GenerateRenderer, OH_AudioStreamBuilder_SetCapturerCallback,
    OH_AudioStreamBuilder_SetCapturerInfo, OH_AudioStreamBuilder_SetChannelCount,
    OH_AudioStreamBuilder_SetFrameSizeInCallback, OH_AudioStreamBuilder_SetLatencyMode,
    OH_AudioStreamBuilder_SetRendererCallback, OH_AudioStreamBuilder_SetRendererInfo,
    OH_AudioStreamBuilder_SetSampleFormat, OH_AudioStreamBuilder_SetSamplingRate,
    OH_AudioStream_LatencyMode_AUDIOSTREAM_LATENCY_MODE_FAST,
    OH_AudioStream_LatencyMode_AUDIOSTREAM_LATENCY_MODE_NORMAL,
    OH_AudioStream_SampleFormat_AUDIOSTREAM_SAMPLE_F32LE,
    OH_AudioStream_SourceType_AUDIOSTREAM_SOURCE_TYPE_MIC,
    OH_AudioStream_Type_AUDIOSTREAM_TYPE_CAPTURER,
    OH_AudioStream_Type_AUDIOSTREAM_TYPE_RENDERER,
    OH_AudioStream_Usage,
    OH_AudioStream_Usage_AUDIOSTREAM_USAGE_GAME,
    OH_AudioStream_Usage_AUDIOSTREAM_USAGE_MOVIE,
    OH_AudioStream_Usage_AUDIOSTREAM_USAGE_MUSIC,
    OH_AudioStream_Usage_AUDIOSTREAM_USAGE_VOICE_COMMUNICATION,
    OH_AudioStream_Usage_AUDIOSTREAM_USAGE_VOICE_ASSISTANT,
};
use super::{BackendSetup, RecorderBackendSetup, RecorderStateCell, StateCell};
use crate::{Backend, RecorderBackend};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OhosLatencyMode {
    Normal,
    Fast,
}

impl OhosLatencyMode {
    fn as_sys(&self) -> u32 {
        match self {
            OhosLatencyMode::Normal => OH_AudioStream_LatencyMode_AUDIOSTREAM_LATENCY_MODE_NORMAL,
            OhosLatencyMode::Fast => OH_AudioStream_LatencyMode_AUDIOSTREAM_LATENCY_MODE_FAST,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OhosUsage {
    Music,
    VoiceCommunication,
    VoiceAssistant,
    Movie,
    Game,
}

impl OhosUsage {
    fn as_sys(&self) -> OH_AudioStream_Usage {
        match self {
            OhosUsage::Music => OH_AudioStream_Usage_AUDIOSTREAM_USAGE_MUSIC,
            OhosUsage::VoiceCommunication => {
                OH_AudioStream_Usage_AUDIOSTREAM_USAGE_VOICE_COMMUNICATION
            }
            OhosUsage::VoiceAssistant => {
                OH_AudioStream_Usage_AUDIOSTREAM_USAGE_VOICE_ASSISTANT
            }
            OhosUsage::Movie => OH_AudioStream_Usage_AUDIOSTREAM_USAGE_MOVIE,
            OhosUsage::Game => OH_AudioStream_Usage_AUDIOSTREAM_USAGE_GAME,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OhosSettings {
    pub buffer_size: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: u16,
    pub latency_mode: OhosLatencyMode,
    pub usage: OhosUsage,
}

impl Default for OhosSettings {
    fn default() -> Self {
        Self {
            buffer_size: None,
            sample_rate: None,
            channels: 2,
            latency_mode: OhosLatencyMode::Normal,
            usage: OhosUsage::Music,
        }
    }
}

pub struct OhosBackend {
    settings: OhosSettings,
    state: Option<Arc<StateCell>>,
    broken: Arc<AtomicBool>,
    stream: Option<*mut OH_AudioRenderer>,
}

impl OhosBackend {
    pub fn new(settings: OhosSettings) -> Self {
        Self {
            settings,
            state: None,
            broken: Arc::default(),
            stream: None,
        }
    }

    pub fn settings(&self) -> &OhosSettings {
        &self.settings
    }
}

impl Backend for OhosBackend {
    fn setup(&mut self, setup: BackendSetup) -> Result<()> {
        self.state = Some(Arc::new(setup.into()));
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        unsafe {
            let mut builder: *mut OH_AudioStreamBuilder = ptr::null_mut();
            let channels = self.settings.channels as i32;
            OH_AudioStreamBuilder_Create(
                &mut builder as *mut *mut OH_AudioStreamBuilder,
                OH_AudioStream_Type_AUDIOSTREAM_TYPE_RENDERER,
            );
            if let Some(sample_rate) = self.settings.sample_rate {
                OH_AudioStreamBuilder_SetSamplingRate(builder, sample_rate as i32);
            }
            OH_AudioStreamBuilder_SetChannelCount(builder, channels);
            OH_AudioStreamBuilder_SetSampleFormat(
                builder,
                OH_AudioStream_SampleFormat_AUDIOSTREAM_SAMPLE_F32LE,
            );
            OH_AudioStreamBuilder_SetLatencyMode(
                builder,
                self.settings.latency_mode.as_sys(),
            );
            OH_AudioStreamBuilder_SetRendererInfo(
                builder,
                self.settings.usage.as_sys(),
            );

            if let Some(buffer_size) = self.settings.buffer_size {
                OH_AudioStreamBuilder_SetFrameSizeInCallback(builder, buffer_size as i32);
            }

            let callback_data = Box::new(OhosCallbackData::new(
                Arc::clone(self.state.as_ref().unwrap()),
                Arc::clone(&self.broken),
                self.settings.channels,
            ));
            let user_data = Box::into_raw(callback_data) as *mut c_void;

            let callbacks = OH_AudioRenderer_Callbacks {
                OH_AudioRenderer_OnWriteData: Some(audio_renderer_on_write_data),
                OH_AudioRenderer_OnStreamEvent: None,
                OH_AudioRenderer_OnInterruptEvent: Some(audio_renderer_on_interrupt),
                OH_AudioRenderer_OnError: None,
            };

            OH_AudioStreamBuilder_SetRendererCallback(builder, callbacks, user_data);
            let mut renderer: *mut OH_AudioRenderer = ptr::null_mut();
            OH_AudioStreamBuilder_GenerateRenderer(builder, &mut renderer);
            OH_AudioStreamBuilder_Destroy(builder);
            let mut actual_sample_rate: i32 = 0;
            OH_AudioRenderer_GetSamplingRate(renderer, &mut actual_sample_rate);
            self.state.as_ref().unwrap().get().0.sample_rate = actual_sample_rate as u32;
            OH_AudioRenderer_Start(renderer);
            self.stream = Some(renderer);
            Ok(())
        }
    }

    fn close(&mut self) -> Result<()> {
        if let Some(renderer) = self.stream.take() {
            unsafe {
                OH_AudioRenderer_Release(renderer);
            }
        }
        Ok(())
    }

    fn consume_broken(&self) -> bool {
        self.broken.fetch_and(false, Ordering::Relaxed)
    }
}

struct OhosCallbackData {
    state: Arc<StateCell>,
    broken: Arc<AtomicBool>,
    channels: u16,
}

impl OhosCallbackData {
    fn new(state: Arc<StateCell>, broken: Arc<AtomicBool>, channels: u16) -> Self {
        Self {
            state,
            broken,
            channels,
        }
    }
}

extern "C" fn audio_renderer_on_write_data(
    _renderer: *mut OH_AudioRenderer,
    user_data: *mut c_void,
    buffer: *mut c_void,
    length: i32,
) -> i32 {
    if user_data.is_null() || buffer.is_null() || length <= 0 {
        return -1;
    }

    let callback_data = unsafe { &mut *(user_data as *mut OhosCallbackData) };
    let (mixer, _) = callback_data.state.get();

    let sample_count = length as usize / size_of::<f32>();

    let f32_buffer = unsafe { std::slice::from_raw_parts_mut(buffer as *mut f32, sample_count) };

    if callback_data.channels == 1 {
        mixer.render_mono(f32_buffer);
    } else {
        mixer.render_stereo(f32_buffer);
    }

    0
}

extern "C" fn audio_renderer_on_interrupt(
    _renderer: *mut OH_AudioRenderer,
    user_data: *mut c_void,
    _force_type: OH_AudioInterrupt_ForceType,
    hint: OH_AudioInterrupt_Hint,
) -> i32 {
    if user_data.is_null() {
        return -1;
    }

    let callback_data = unsafe { &*(user_data as *mut OhosCallbackData) };

    #[allow(non_upper_case_globals)]
    if matches!(
        hint,
        OH_AudioInterrupt_Hint_AUDIOSTREAM_INTERRUPT_HINT_PAUSE
            | OH_AudioInterrupt_Hint_AUDIOSTREAM_INTERRUPT_HINT_STOP
    ) {
        callback_data.broken.store(true, Ordering::Relaxed);
    }

    0
}

impl Drop for OhosBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

pub struct OhosRecorderBackend {
    settings: OhosSettings,
    state: Option<Arc<RecorderStateCell>>,
    broken: Arc<AtomicBool>,
    stream: Option<*mut OH_AudioCapturer>,
}

impl OhosRecorderBackend {
    pub fn new(settings: OhosSettings) -> Self {
        Self {
            settings,
            state: None,
            broken: Arc::default(),
            stream: None,
        }
    }

    pub fn settings(&self) -> &OhosSettings {
        &self.settings
    }
}

impl RecorderBackend for OhosRecorderBackend {
    fn setup(&mut self, setup: RecorderBackendSetup) -> Result<()> {
        self.state = Some(Arc::new(setup.into()));
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        unsafe {
            let mut builder: *mut OH_AudioStreamBuilder = ptr::null_mut();
            let sample_rate = self.settings.sample_rate.unwrap_or(48000) as i32;
            let channels = self.settings.channels as i32;
            OH_AudioStreamBuilder_Create(
                &mut builder as *mut *mut OH_AudioStreamBuilder,
                OH_AudioStream_Type_AUDIOSTREAM_TYPE_CAPTURER,
            );
            OH_AudioStreamBuilder_SetSamplingRate(builder, sample_rate);
            OH_AudioStreamBuilder_SetChannelCount(builder, channels);
            OH_AudioStreamBuilder_SetSampleFormat(
                builder,
                OH_AudioStream_SampleFormat_AUDIOSTREAM_SAMPLE_F32LE,
            );
            OH_AudioStreamBuilder_SetLatencyMode(
                builder,
                self.settings.latency_mode.as_sys(),
            );
            OH_AudioStreamBuilder_SetCapturerInfo(
                builder,
                OH_AudioStream_SourceType_AUDIOSTREAM_SOURCE_TYPE_MIC,
            );

            if let Some(buffer_size) = self.settings.buffer_size {
                OH_AudioStreamBuilder_SetFrameSizeInCallback(builder, buffer_size as i32);
            }

            let callback_data = Box::new(OhosRecorderCallbackData::new(
                Arc::clone(self.state.as_ref().unwrap()),
                Arc::clone(&self.broken),
                self.settings.channels,
            ));
            let user_data = Box::into_raw(callback_data) as *mut c_void;

            let callbacks = OH_AudioCapturer_Callbacks {
                OH_AudioCapturer_OnReadData: Some(audio_capturer_on_read_data),
                OH_AudioCapturer_OnStreamEvent: None,
                OH_AudioCapturer_OnInterruptEvent: Some(audio_capturer_on_interrupt),
                OH_AudioCapturer_OnError: None,
            };

            OH_AudioStreamBuilder_SetCapturerCallback(builder, callbacks, user_data);
            let mut capturer: *mut OH_AudioCapturer = ptr::null_mut();
            OH_AudioStreamBuilder_GenerateCapturer(builder, &mut capturer);
            OH_AudioStreamBuilder_Destroy(builder);
            let mut actual_sample_rate: i32 = 0;
            OH_AudioCapturer_GetSamplingRate(capturer, &mut actual_sample_rate);
            self.state
                .as_ref()
                .unwrap()
                .get()
                .0
                .sample_rate = actual_sample_rate as u32;
            OH_AudioCapturer_Start(capturer);
            self.stream = Some(capturer);
            Ok(())
        }
    }

    fn close(&mut self) -> Result<()> {
        if let Some(capturer) = self.stream.take() {
            unsafe {
                OH_AudioCapturer_Release(capturer);
            }
        }
        Ok(())
    }

    fn consume_broken(&self) -> bool {
        self.broken.fetch_and(false, Ordering::Relaxed)
    }
}

struct OhosRecorderCallbackData {
    state: Arc<RecorderStateCell>,
    broken: Arc<AtomicBool>,
    channels: u16,
}

impl OhosRecorderCallbackData {
    fn new(state: Arc<RecorderStateCell>, broken: Arc<AtomicBool>, channels: u16) -> Self {
        Self {
            state,
            broken,
            channels,
        }
    }
}

extern "C" fn audio_capturer_on_read_data(
    _capturer: *mut OH_AudioCapturer,
    user_data: *mut c_void,
    buffer: *mut c_void,
    length: i32,
) -> i32 {
    if user_data.is_null() || buffer.is_null() || length <= 0 {
        return -1;
    }

    let callback_data = unsafe { &mut *(user_data as *mut OhosRecorderCallbackData) };
    let (mixer, _) = callback_data.state.get();

    let sample_count = length as usize / size_of::<f32>();
    let f32_slice = unsafe { std::slice::from_raw_parts(buffer as *const f32, sample_count) };

    if callback_data.channels == 1 {
        mixer.record_mono(f32_slice);
    } else {
        mixer.record_stereo(f32_slice);
    }

    0
}

extern "C" fn audio_capturer_on_interrupt(
    _capturer: *mut OH_AudioCapturer,
    user_data: *mut c_void,
    _force_type: OH_AudioInterrupt_ForceType,
    hint: OH_AudioInterrupt_Hint,
) -> i32 {
    if user_data.is_null() {
        return -1;
    }

    let callback_data = unsafe { &*(user_data as *mut OhosRecorderCallbackData) };

    #[allow(non_upper_case_globals)]
    if matches!(
        hint,
        OH_AudioInterrupt_Hint_AUDIOSTREAM_INTERRUPT_HINT_PAUSE
            | OH_AudioInterrupt_Hint_AUDIOSTREAM_INTERRUPT_HINT_STOP
    ) {
        callback_data.broken.store(true, Ordering::Relaxed);
    }

    0
}

impl Drop for OhosRecorderBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
