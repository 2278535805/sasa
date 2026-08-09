use crate::{Backend, RecorderBackend};
use anyhow::{Context, Result};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    BufferSize, InputCallbackInfo, OutputCallbackInfo, Stream, StreamError,
};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};

use super::{BackendSetup, BackendStreamInfo, RecorderBackendSetup, RecorderStateCell, StateCell};

#[derive(Debug, Clone, Default)]
pub struct CpalSettings {
    pub buffer_size: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct CpalStreamInfo {
    pub settings: CpalSettings,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub device_name: Option<String>,
    pub actual_frames_per_callback: Option<u32>,
}

impl std::fmt::Display for CpalStreamInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "settings.buffer_size: {:?}", self.settings.buffer_size)?;
        writeln!(f, "sample_rate: {:?}", self.sample_rate)?;
        writeln!(f, "channels: {:?}", self.channels)?;
        writeln!(f, "device_name: {:?}", self.device_name)?;
        writeln!(f, "actual_frames_per_callback: {:?}", self.actual_frames_per_callback)
    }
}

pub struct CpalBackend {
    settings: CpalSettings,
    stream: Option<Stream>,
    broken: Arc<AtomicBool>,
    state: Option<Arc<StateCell>>,
    sample_rate: Option<u32>,
    channels: Option<u16>,
    device_name: Option<String>,
    actual_frames: Arc<AtomicU32>,
}

impl CpalBackend {
    pub fn new(settings: CpalSettings) -> Self {
        Self {
            settings,
            stream: None,
            broken: Arc::default(),
            state: None,
            sample_rate: None,
            channels: None,
            device_name: None,
            actual_frames: Arc::default(),
        }
    }
}

impl Backend for CpalBackend {
    fn setup(&mut self, setup: BackendSetup) -> Result<()> {
        self.state = Some(Arc::new(setup.into()));
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        let host = cpal::default_host();
        let device = match host.default_output_device() { 
            Some(device) => device, 
            None => { 
                eprintln!("no default output device is found"); 
                return Ok(());
            },
        };
        let device_name = device.name().ok().unwrap_or_default();
        let config = device
            .default_output_config()
            .context("cannot get output config")?
            .config();
        let channels = config.channels;
        let sample_rate = config.sample_rate.0;
        let mut config_with_buffer = config.clone();
        config_with_buffer.buffer_size = self
            .settings
            .buffer_size
            .map_or(BufferSize::Default, |it| BufferSize::Fixed(it));

        let broken = Arc::clone(&self.broken);
        let actual_frames = Arc::clone(&self.actual_frames);
        let error_callback = move |err| {
            eprintln!("audio error: {err:?}");
            if matches!(err, StreamError::DeviceNotAvailable) {
                broken.store(true, Ordering::Relaxed);
            }
        };
        let state = Arc::clone(self.state.as_ref().unwrap());
        state.get().0.sample_rate = sample_rate;
        let stream = (if channels == 1 {
            device.build_output_stream(
                &config_with_buffer,
                move |data: &mut [f32], info: &OutputCallbackInfo| {
                    let (mixer, rec) = state.get();
                    mixer.render_mono(data);
                    actual_frames.store(data.len() as u32, Ordering::Relaxed);
                    let ts = info.timestamp();
                    if let Some(delay) = ts.playback.duration_since(&ts.callback) {
                        rec.push(delay.as_secs_f64());
                    }
                },
                error_callback,
                None,
            )
        } else {
            device.build_output_stream(
                &config_with_buffer,
                move |data: &mut [f32], info: &OutputCallbackInfo| {
                    let (mixer, rec) = state.get();
                    mixer.render_stereo(data);
                    actual_frames.store((data.len() / 2) as u32, Ordering::Relaxed);
                    let ts = info.timestamp();
                    if let Some(delay) = ts.playback.duration_since(&ts.callback) {
                        rec.push(delay.as_secs_f64());
                    }
                },
                error_callback,
                None
            )
        })
        .context("failed to build stream")?;
        stream.play()?;
        self.sample_rate = Some(sample_rate);
        self.channels = Some(channels);
        self.device_name = Some(device_name);
        self.stream = Some(stream);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.stream = None;
        Ok(())
    }

    fn consume_broken(&self) -> bool {
        self.broken.fetch_and(false, Ordering::Relaxed)
    }

    fn stream_info(&mut self) -> BackendStreamInfo {
        let frames = self.actual_frames.load(Ordering::Relaxed);
        BackendStreamInfo::Cpal(CpalStreamInfo {
            settings: self.settings.clone(),
            sample_rate: self.sample_rate,
            channels: self.channels,
            device_name: self.device_name.clone(),
            actual_frames_per_callback: if frames > 0 { Some(frames) } else { None },
        })
    }
}

impl Drop for CpalBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

pub struct CpalRecorderBackend {
    settings: CpalSettings,
    stream: Option<Stream>,
    broken: Arc<AtomicBool>,
    state: Option<Arc<RecorderStateCell>>,
    sample_rate: Option<u32>,
    channels: Option<u16>,
    device_name: Option<String>,
    actual_frames: Arc<AtomicU32>,
}

impl CpalRecorderBackend {
    pub fn new(settings: CpalSettings) -> Self {
        Self {
            settings,
            stream: None,
            broken: Arc::default(),
            state: None,
            sample_rate: None,
            channels: None,
            device_name: None,
            actual_frames: Arc::default(),
        }
    }
}

impl RecorderBackend for CpalRecorderBackend {
    fn setup(&mut self, setup: RecorderBackendSetup) -> Result<()> {
        self.state = Some(Arc::new(setup.into()));
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        let host = cpal::default_host();
        let device = match host.default_input_device() {
            Some(device) => device,
            None => {
                eprintln!("no default input device is found");
                return Ok(());
            }
        };
        let device_name = device.name().ok().unwrap_or_default();
        let config = device
            .default_input_config()
            .context("cannot get input config")?
            .config();
        let channels = config.channels;
        let sample_rate = config.sample_rate.0;
        let mut config_with_buffer = config.clone();
        config_with_buffer.buffer_size = self
            .settings
            .buffer_size
            .map_or(BufferSize::Default, |it| BufferSize::Fixed(it));

        let broken = Arc::clone(&self.broken);
        let actual_frames = Arc::clone(&self.actual_frames);
        let error_callback = move |err| {
            eprintln!("audio input error: {err:?}");
            if matches!(err, StreamError::DeviceNotAvailable) {
                broken.store(true, Ordering::Relaxed);
            }
        };
        let state = Arc::clone(self.state.as_ref().unwrap());
        state.get().0.sample_rate = sample_rate;
        let stream = (if channels == 1 {
            device.build_input_stream(
                &config_with_buffer,
                move |data: &[f32], info: &InputCallbackInfo| {
                    let (mixer, rec) = state.get();
                    mixer.record_mono(data);
                    actual_frames.store(data.len() as u32, Ordering::Relaxed);
                    let ts = info.timestamp();
                    if let Some(delay) = ts.capture.duration_since(&ts.callback) {
                        rec.push(delay.as_secs_f64());
                    }
                },
                error_callback,
                None,
            )
        } else {
            device.build_input_stream(
                &config_with_buffer,
                move |data: &[f32], info: &InputCallbackInfo| {
                    let (mixer, rec) = state.get();
                    mixer.record_stereo(data);
                    actual_frames.store((data.len() / 2) as u32, Ordering::Relaxed);
                    let ts = info.timestamp();
                    if let Some(delay) = ts.capture.duration_since(&ts.callback) {
                        rec.push(delay.as_secs_f64());
                    }
                },
                error_callback,
                None,
            )
        })
        .context("failed to build input stream")?;
        stream.play()?;
        self.sample_rate = Some(sample_rate);
        self.channels = Some(channels);
        self.device_name = Some(device_name);
        self.stream = Some(stream);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.stream = None;
        Ok(())
    }

    fn consume_broken(&self) -> bool {
        self.broken.fetch_and(false, Ordering::Relaxed)
    }

    fn stream_info(&mut self) -> BackendStreamInfo {
        let frames = self.actual_frames.load(Ordering::Relaxed);
        BackendStreamInfo::Cpal(CpalStreamInfo {
            settings: self.settings.clone(),
            sample_rate: self.sample_rate,
            channels: self.channels,
            device_name: self.device_name.clone(),
            actual_frames_per_callback: if frames > 0 { Some(frames) } else { None },
        })
    }
}

impl Drop for CpalRecorderBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
