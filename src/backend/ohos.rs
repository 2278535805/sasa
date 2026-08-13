use anyhow::{anyhow, Result};
use std::{
    ffi::c_void,
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use ohos_audio_sys::{
    OH_AudioCapturer, OH_AudioCapturer_Callbacks, OH_AudioCapturer_GetChannelCount,
    OH_AudioCapturer_GetCurrentState, OH_AudioCapturer_GetEncodingType,
    OH_AudioCapturer_GetFastStatus, OH_AudioCapturer_GetFrameSizeInCallback,
    OH_AudioCapturer_GetFramesRead, OH_AudioCapturer_GetLatencyMode,
    OH_AudioCapturer_GetOverflowCount, OH_AudioCapturer_GetSamplingRate,
    OH_AudioCapturer_GetSampleFormat, OH_AudioCapturer_GetStreamId, OH_AudioCapturer_Release,
    OH_AudioCapturer_Start, OH_AudioCapturer_Stop, OH_AudioInterrupt_ForceType,
    OH_AudioInterrupt_Hint,
    OH_AudioInterrupt_Hint_AUDIOSTREAM_INTERRUPT_HINT_PAUSE,
    OH_AudioInterrupt_Hint_AUDIOSTREAM_INTERRUPT_HINT_STOP,
    OH_AudioRenderer,
    OH_AudioRenderer_Callbacks, OH_AudioRenderer_GetChannelCount, OH_AudioRenderer_GetChannelLayout,
    OH_AudioRenderer_GetCurrentState, OH_AudioRenderer_GetEncodingType,
    OH_AudioRenderer_GetFastStatus, OH_AudioRenderer_GetFrameSizeInCallback,
    OH_AudioRenderer_GetFramesWritten, OH_AudioRenderer_GetLatencyMode,
    OH_AudioRenderer_GetSamplingRate, OH_AudioRenderer_GetSampleFormat,
    OH_AudioRenderer_GetStreamId, OH_AudioRenderer_GetUnderflowCount, OH_AudioRenderer_Release,
    OH_AudioRenderer_Start, OH_AudioRenderer_Stop, OH_AudioStreamBuilder, OH_AudioStreamBuilder_Create,
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
use super::{BackendSetup, BackendStreamInfo, RecorderBackendSetup, RecorderStateCell, StateCell};
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

#[derive(Debug, Clone)]
pub struct OhosStreamInfo {
    pub settings: OhosSettings,
    pub actual_sample_rate: Option<u32>,
    pub channel_count: Option<i32>,
    pub sample_format: Option<u32>,
    pub actual_latency_mode: Option<u32>,
    pub frame_size_in_callback: Option<i32>,
    pub stream_id: Option<u32>,
    pub encoding_type: Option<u32>,
    pub channel_layout: Option<u64>,
    pub fast_status: Option<bool>,
    pub underflow_count: Option<u32>,
    pub overflow_count: Option<u32>,
    pub frames_written: Option<i64>,
    pub frames_read: Option<i64>,
    pub stream_state: Option<i32>,
}

impl std::fmt::Display for OhosStreamInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "settings.buffer_size: {:?}", self.settings.buffer_size)?;
        writeln!(f, "settings.sample_rate: {:?}", self.settings.sample_rate)?;
        writeln!(f, "settings.channels: {:?}", self.settings.channels)?;
        writeln!(f, "settings.latency_mode: {:?}", self.settings.latency_mode)?;
        writeln!(f, "settings.usage: {:?}", self.settings.usage)?;
        writeln!(f, "actual_sample_rate: {:?}", self.actual_sample_rate)?;
        writeln!(f, "channel_count: {:?}", self.channel_count)?;
        writeln!(f, "sample_format: {:?}", self.sample_format)?;
        writeln!(f, "actual_latency_mode: {:?}", self.actual_latency_mode)?;
        writeln!(f, "frame_size_in_callback: {:?}", self.frame_size_in_callback)?;
        writeln!(f, "stream_id: {:?}", self.stream_id)?;
        writeln!(f, "encoding_type: {:?}", self.encoding_type)?;
        writeln!(f, "channel_layout: {:?}", self.channel_layout)?;
        writeln!(f, "fast_status: {:?}", self.fast_status)?;
        writeln!(f, "underflow_count: {:?}", self.underflow_count)?;
        writeln!(f, "overflow_count: {:?}", self.overflow_count)?;
        writeln!(f, "frames_written: {:?}", self.frames_written)?;
        writeln!(f, "frames_read: {:?}", self.frames_read)?;
        writeln!(f, "stream_state: {:?}", self.stream_state)?;
        Ok(())
    }
}

pub struct OhosBackend {
    settings: OhosSettings,
    state: Option<Arc<StateCell>>,
    broken: Arc<AtomicBool>,
    stream: Option<*mut OH_AudioRenderer>,
    callback_data: Option<*mut c_void>,
}

impl OhosBackend {
    pub fn new(settings: OhosSettings) -> Self {
        Self {
            settings,
            state: None,
            broken: Arc::default(),
            stream: None,
            callback_data: None,
        }
    }

    pub fn settings(&self) -> &OhosSettings {
        &self.settings
    }
}

impl Backend for OhosBackend {
    fn setup(&mut self, setup: BackendSetup) {
        self.state = Some(Arc::new(setup.into()));
    }

    fn start(&mut self) -> Result<()> {
        if self.stream.is_some() {
            self.close()?;
        }
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
            let result = OH_AudioStreamBuilder_GenerateRenderer(builder, &mut renderer);
            OH_AudioStreamBuilder_Destroy(builder);
            if result != 0 || renderer.is_null() {
                drop(Box::from_raw(user_data as *mut OhosCallbackData));
                return Err(anyhow!(
                    "OH_AudioStreamBuilder_GenerateRenderer failed: {result}"
                ));
            }
            let mut actual_sample_rate: i32 = 0;
            OH_AudioRenderer_GetSamplingRate(renderer, &mut actual_sample_rate);
            self.state.as_ref().unwrap().get().0.sample_rate = actual_sample_rate as u32;
            let start_result = OH_AudioRenderer_Start(renderer);
            if start_result != 0 {
                let _ = OH_AudioRenderer_Release(renderer);
                drop(Box::from_raw(user_data as *mut OhosCallbackData));
                return Err(anyhow!("OH_AudioRenderer_Start failed: {start_result}"));
            }
            self.stream = Some(renderer);
            self.callback_data = Some(user_data);
            Ok(())
        }
    }

    fn close(&mut self) -> Result<()> {
        if let Some(renderer) = self.stream.take() {
            unsafe {
                let _ = OH_AudioRenderer_Stop(renderer);
                OH_AudioRenderer_Release(renderer);
            }
        }
        if let Some(data) = self.callback_data.take() {
            unsafe {
                drop(Box::from_raw(data as *mut OhosCallbackData));
            }
        }
        Ok(())
    }

    fn consume_broken(&self) -> bool {
        self.broken.fetch_and(false, Ordering::Relaxed)
    }

    fn stream_info(&mut self) -> BackendStreamInfo {
        let mut info = OhosStreamInfo {
            settings: self.settings.clone(),
            actual_sample_rate: None,
            channel_count: None,
            sample_format: None,
            actual_latency_mode: None,
            frame_size_in_callback: None,
            stream_id: None,
            encoding_type: None,
            channel_layout: None,
            fast_status: None,
            underflow_count: None,
            overflow_count: None,
            frames_written: None,
            frames_read: None,
            stream_state: None,
        };
        if let Some(renderer) = self.stream {
            unsafe {
                let mut val: i32 = 0;
                if OH_AudioRenderer_GetSamplingRate(renderer, &mut val) == 0 { info.actual_sample_rate = Some(val as u32); }

                val = 0;
                if OH_AudioRenderer_GetChannelCount(renderer, &mut val) == 0 { info.channel_count = Some(val); }

                let mut uval: u32 = 0;
                if OH_AudioRenderer_GetSampleFormat(renderer, &mut uval) == 0 { info.sample_format = Some(uval); }

                uval = 0;
                if OH_AudioRenderer_GetLatencyMode(renderer, &mut uval) == 0 { info.actual_latency_mode = Some(uval); }

                val = 0;
                if OH_AudioRenderer_GetFrameSizeInCallback(renderer, &mut val) == 0 { info.frame_size_in_callback = Some(val); }

                uval = 0;
                if OH_AudioRenderer_GetStreamId(renderer, &mut uval) == 0 { info.stream_id = Some(uval); }

                uval = 0;
                if OH_AudioRenderer_GetEncodingType(renderer, &mut uval) == 0 { info.encoding_type = Some(uval); }

                let mut cval: u64 = 0;
                if OH_AudioRenderer_GetChannelLayout(renderer, &mut cval) == 0 { info.channel_layout = Some(cval); }

                let mut ucount: u32 = 0;
                if OH_AudioRenderer_GetUnderflowCount(renderer, &mut ucount) == 0 { info.underflow_count = Some(ucount); }

                let mut frames: i64 = 0;
                if OH_AudioRenderer_GetFramesWritten(renderer, &mut frames) == 0 { info.frames_written = Some(frames); }

                let mut status: u32 = 0;
                if OH_AudioRenderer_GetFastStatus(renderer, &mut status) == 0 { info.fast_status = Some(status == 1); }

                let mut state: i32 = 0;
                if OH_AudioRenderer_GetCurrentState(renderer, &mut state) == 0 { info.stream_state = Some(state); }
            }
        }
        BackendStreamInfo::Ohos(info)
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
    callback_data: Option<*mut c_void>,
}

impl OhosRecorderBackend {
    pub fn new(settings: OhosSettings) -> Self {
        Self {
            settings,
            state: None,
            broken: Arc::default(),
            stream: None,
            callback_data: None,
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
        if self.stream.is_some() {
            self.close()?;
        }
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
            let result = OH_AudioStreamBuilder_GenerateCapturer(builder, &mut capturer);
            OH_AudioStreamBuilder_Destroy(builder);
            if result != 0 || capturer.is_null() {
                drop(Box::from_raw(user_data as *mut OhosRecorderCallbackData));
                return Err(anyhow!(
                    "OH_AudioStreamBuilder_GenerateCapturer failed: {result}"
                ));
            }
            let mut actual_sample_rate: i32 = 0;
            OH_AudioCapturer_GetSamplingRate(capturer, &mut actual_sample_rate);
            self.state
                .as_ref()
                .unwrap()
                .get()
                .0
                .sample_rate = actual_sample_rate as u32;
            let start_result = OH_AudioCapturer_Start(capturer);
            if start_result != 0 {
                let _ = OH_AudioCapturer_Release(capturer);
                drop(Box::from_raw(user_data as *mut OhosRecorderCallbackData));
                return Err(anyhow!("OH_AudioCapturer_Start failed: {start_result}"));
            }
            self.stream = Some(capturer);
            self.callback_data = Some(user_data);
            Ok(())
        }
    }

    fn close(&mut self) -> Result<()> {
        if let Some(capturer) = self.stream.take() {
            unsafe {
                let _ = OH_AudioCapturer_Stop(capturer);
                OH_AudioCapturer_Release(capturer);
            }
        }
        if let Some(data) = self.callback_data.take() {
            unsafe {
                drop(Box::from_raw(data as *mut OhosRecorderCallbackData));
            }
        }
        Ok(())
    }

    fn consume_broken(&self) -> bool {
        self.broken.fetch_and(false, Ordering::Relaxed)
    }

    fn stream_info(&mut self) -> BackendStreamInfo {
        let mut info = OhosStreamInfo {
            settings: self.settings.clone(),
            actual_sample_rate: None,
            channel_count: None,
            sample_format: None,
            actual_latency_mode: None,
            frame_size_in_callback: None,
            stream_id: None,
            encoding_type: None,
            channel_layout: None,
            fast_status: None,
            underflow_count: None,
            overflow_count: None,
            frames_written: None,
            frames_read: None,
            stream_state: None,
        };
        if let Some(capturer) = self.stream {
            unsafe {
                let mut val: i32 = 0;
                if OH_AudioCapturer_GetSamplingRate(capturer, &mut val) == 0 { info.actual_sample_rate = Some(val as u32); }

                val = 0;
                if OH_AudioCapturer_GetChannelCount(capturer, &mut val) == 0 { info.channel_count = Some(val); }

                let mut uval: u32 = 0;
                if OH_AudioCapturer_GetSampleFormat(capturer, &mut uval) == 0 { info.sample_format = Some(uval); }

                uval = 0;
                if OH_AudioCapturer_GetLatencyMode(capturer, &mut uval) == 0 { info.actual_latency_mode = Some(uval); }

                val = 0;
                if OH_AudioCapturer_GetFrameSizeInCallback(capturer, &mut val) == 0 { info.frame_size_in_callback = Some(val); }

                uval = 0;
                if OH_AudioCapturer_GetStreamId(capturer, &mut uval) == 0 { info.stream_id = Some(uval); }

                uval = 0;
                if OH_AudioCapturer_GetEncodingType(capturer, &mut uval) == 0 { info.encoding_type = Some(uval); }

                let mut ucount: u32 = 0;
                if OH_AudioCapturer_GetOverflowCount(capturer, &mut ucount) == 0 { info.overflow_count = Some(ucount); }

                let mut frames: i64 = 0;
                if OH_AudioCapturer_GetFramesRead(capturer, &mut frames) == 0 { info.frames_read = Some(frames); }

                let mut status: u32 = 0;
                if OH_AudioCapturer_GetFastStatus(capturer, &mut status) == 0 { info.fast_status = Some(status == 1); }

                let mut state: i32 = 0;
                if OH_AudioCapturer_GetCurrentState(capturer, &mut state) == 0 { info.stream_state = Some(state); }
            }
        }
        BackendStreamInfo::Ohos(info)
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
