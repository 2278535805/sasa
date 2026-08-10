use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
};

use anyhow::{Context, Result};
pub use wasapi::ShareMode;
use wasapi::{
    calculate_period_100ns, initialize_mta, DeviceEnumerator,
    Direction, SampleType, StreamMode, WaveFormat,
};

use super::{BackendSetup, BackendStreamInfo, RecorderBackendSetup, RecorderStateCell, StateCell};
use crate::{Backend, RecorderBackend};

#[derive(Debug, Clone)]
pub struct WasapiSettings {
    pub buffer_size: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub exclusive: bool,
}

impl Default for WasapiSettings {
    fn default() -> Self {
        Self {
            buffer_size: None,
            sample_rate: None,
            channels: None,
            exclusive: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WasapiStreamInfo {
    pub settings: WasapiSettings,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub device_name: Option<String>,
    pub share_mode: Option<ShareMode>,
    pub actual_frames_per_callback: Option<u32>,
}

impl std::fmt::Display for WasapiStreamInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "settings.buffer_size: {:?}", self.settings.buffer_size)?;
        writeln!(f, "settings.sample_rate: {:?}", self.settings.sample_rate)?;
        writeln!(f, "settings.channels: {:?}", self.settings.channels)?;
        writeln!(f, "settings.exclusive: {:?}", self.settings.exclusive)?;
        writeln!(f, "sample_rate: {:?}", self.sample_rate)?;
        writeln!(f, "channels: {:?}", self.channels)?;
        writeln!(f, "device_name: {:?}", self.device_name)?;
        writeln!(f, "share_mode: {:?}", self.share_mode)?;
        writeln!(f,"actual_frames_per_callback: {:?}", self.actual_frames_per_callback)
    }
}

pub struct WasapiBackend {
    settings: WasapiSettings,
    state: Option<Arc<StateCell>>,
    broken: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    join_handle: Option<JoinHandle<()>>,
    sample_rate: Arc<AtomicU32>,
    channels: Arc<AtomicU32>,
    actual_frames: Arc<AtomicU32>,
    device_name: Arc<Mutex<Option<String>>>,
    share_mode: Arc<Mutex<Option<ShareMode>>>,
}

impl WasapiBackend {
    pub fn new(settings: WasapiSettings) -> Self {
        Self {
            settings,
            state: None,
            broken: Arc::default(),
            running: Arc::default(),
            join_handle: None,
            sample_rate: Arc::default(),
            channels: Arc::default(),
            actual_frames: Arc::default(),
            device_name: Arc::default(),
            share_mode: Arc::default(),
        }
    }

    fn run_playback(
        settings: WasapiSettings,
        state: Arc<StateCell>,
        broken: Arc<AtomicBool>,
        running: Arc<AtomicBool>,
        actual_frames: Arc<AtomicU32>,
        sample_rate: Arc<AtomicU32>,
        channels: Arc<AtomicU32>,
        device_name: Arc<Mutex<Option<String>>>,
        share_mode: Arc<Mutex<Option<ShareMode>>>,
    ) -> Result<()> {
        let _ = initialize_mta().ok();

        let enumerator = DeviceEnumerator::new().context("create device enumerator")?;
        let device = enumerator
            .get_default_device(&Direction::Render)
            .context("get default output device")?;
        let dev_name = device.get_friendlyname().ok();
        *device_name.lock().unwrap() = dev_name;

        let mut audio_client = device
            .get_iaudioclient()
            .context("get audio client")?;

        let mix_format = if settings.sample_rate.is_none() || settings.channels.is_none() {
            Some(audio_client.get_mixformat().context("get mix format")?)
        } else {
            None
        };

        let desired_sr = if let Some(sr) = settings.sample_rate {
            sr as usize
        } else {
            mix_format.as_ref().unwrap().get_samplespersec() as usize
        };
        let desired_ch = if let Some(ch) = settings.channels {
            ch as usize
        } else {
            mix_format.as_ref().unwrap().get_nchannels() as usize
        };
        let desired_format =
            WaveFormat::new(32, 32, &SampleType::Float, desired_sr, desired_ch, None);

        let (_blockalign, actual_format, mode) = if settings.exclusive {
            let format = audio_client
                .is_supported_exclusive_with_quirks(&desired_format)
                .context("exclusive format not supported")?;
            let blockalign = format.get_blockalign();
            let (_def_period, min_period) = audio_client
                .get_device_period()
                .context("get device period")?;
            let desired_period = audio_client
                .calculate_aligned_period_near(
                    if let Some(bs) = settings.buffer_size {
                        calculate_period_100ns(bs as i64, format.get_samplespersec() as i64)
                    } else {
                        min_period
                    },
                    Some(128),
                    &format,
                )
                .context("calculate aligned period")?;
            (
                blockalign,
                format,
                StreamMode::EventsExclusive {
                    period_hns: desired_period,
                },
            )
        } else {
            let blockalign = desired_format.get_blockalign();
            let (def_period, _min_period) = audio_client
                .get_device_period()
                .context("get device period")?;
            (
                blockalign,
                desired_format,
                StreamMode::EventsShared {
                    autoconvert: true,
                    buffer_duration_hns: if let Some(bs) = settings.buffer_size {
                        calculate_period_100ns(bs as i64, desired_sr as i64)
                    } else {
                        def_period
                    },
                },
            )
        };

        audio_client
            .initialize_client(&actual_format, &Direction::Render, &mode)
            .context("initialize audio client")?;

        let actual_sr = actual_format.get_samplespersec();
        let actual_ch = actual_format.get_nchannels();
        let actual_ch_u32 = actual_ch as u32;

        sample_rate.store(actual_sr, Ordering::Relaxed);
        channels.store(actual_ch_u32, Ordering::Relaxed);
        state.get().0.sample_rate = actual_sr;
        *share_mode.lock().unwrap() = if settings.exclusive {
            Some(ShareMode::Exclusive)
        } else {
            Some(ShareMode::Shared)
        };

        let h_event = audio_client
            .set_get_eventhandle()
            .context("get event handle")?;
        let render_client = audio_client
            .get_audiorenderclient()
            .context("get render client")?;

        audio_client.start_stream().context("start stream")?;

        let mut f32_buf = Vec::new();
        let mut byte_buf = Vec::new();
        let mut loop_result = Ok(());

        loop {
            if !running.load(Ordering::Relaxed) {
                let _ = audio_client.stop_stream();
                break;
            }

            let buffer_frames = match audio_client.get_available_space_in_frames() {
                Ok(f) => f,
                Err(e) => {
                    let _ = audio_client.stop_stream();
                    broken.store(true, Ordering::Relaxed);
                    loop_result = Err(anyhow::anyhow!(e));
                    break;
                }
            };

            if buffer_frames == 0 {
                if h_event.wait_for_event(100).is_err() {
                    let _ = audio_client.stop_stream();
                    broken.store(true, Ordering::Relaxed);
                    loop_result = Err(anyhow::anyhow!("event wait timeout"));
                    break;
                }
                continue;
            }

            actual_frames.store(buffer_frames, Ordering::Relaxed);

            let n_samples = buffer_frames as usize * actual_ch as usize;
            f32_buf.resize(n_samples, 0f32);

            let (mixer, rec) = state.get();
            if actual_ch == 1 {
                mixer.render_mono(&mut f32_buf);
            } else {
                mixer.render_stereo(&mut f32_buf);
            }

            let n_bytes = n_samples * 4;
            byte_buf.resize(n_bytes, 0u8);
            for (i, sample) in f32_buf.iter().enumerate() {
                let bytes = sample.to_le_bytes();
                byte_buf[i * 4..(i + 1) * 4].copy_from_slice(&bytes);
            }

            if let Err(e) = render_client.write_to_device(buffer_frames as usize, &byte_buf, None) {
                let _ = audio_client.stop_stream();
                broken.store(true, Ordering::Relaxed);
                loop_result = Err(anyhow::anyhow!(e));
                break;
            }

            let latency_sec = buffer_frames as f64 / actual_sr as f64;
            rec.push(latency_sec);

            if h_event.wait_for_event(1000).is_err() {
                let _ = audio_client.stop_stream();
                broken.store(true, Ordering::Relaxed);
                loop_result = Err(anyhow::anyhow!("event wait timeout"));
                break;
            }
        }

        loop_result
    }
}

// —— WasapiRecorderBackend ——

impl Backend for WasapiBackend {
    fn setup(&mut self, setup: BackendSetup) -> Result<()> {
        self.state = Some(Arc::new(setup.into()));
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        let settings = self.settings.clone();
        let state = Arc::clone(self.state.as_ref().context("not set up")?);
        let broken = Arc::clone(&self.broken);
        let running = Arc::clone(&self.running);
        let actual_frames = Arc::clone(&self.actual_frames);
        let sample_rate = Arc::clone(&self.sample_rate);
        let channels = Arc::clone(&self.channels);
        let device_name = Arc::clone(&self.device_name);
        let share_mode = Arc::clone(&self.share_mode);

        running.store(true, Ordering::Relaxed);

        let join_handle = std::thread::Builder::new()
            .name("wasapi-playback".into())
            .spawn(move || {
                if let Err(e) = WasapiBackend::run_playback(
                    settings,
                    state,
                    broken,
                    running,
                    actual_frames,
                    sample_rate,
                    channels,
                    device_name,
                    share_mode,
                ) {
                    eprintln!("wasapi playback error: {e}");
                }
            })
            .context("spawn playback thread")?;

        self.join_handle = Some(join_handle);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
        Ok(())
    }

    fn consume_broken(&self) -> bool {
        self.broken.fetch_and(false, Ordering::Relaxed)
    }

    fn stream_info(&mut self) -> BackendStreamInfo {
        let frames = self.actual_frames.load(Ordering::Relaxed);
        BackendStreamInfo::Wasapi(WasapiStreamInfo {
            settings: self.settings.clone(),
            sample_rate: {
                let sr = self.sample_rate.load(Ordering::Relaxed);
                if sr > 0 { Some(sr) } else { None }
            },
            channels: {
                let ch = self.channels.load(Ordering::Relaxed);
                if ch > 0 { Some(ch as u16) } else { None }
            },
            device_name: self.device_name.lock().unwrap().clone(),
            share_mode: *self.share_mode.lock().unwrap(),
            actual_frames_per_callback: if frames > 0 { Some(frames) } else { None },
        })
    }
}

impl Drop for WasapiBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

pub struct WasapiRecorderBackend {
    settings: WasapiSettings,
    state: Option<Arc<RecorderStateCell>>,
    broken: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    join_handle: Option<JoinHandle<()>>,
    sample_rate: Arc<AtomicU32>,
    channels: Arc<AtomicU32>,
    actual_frames: Arc<AtomicU32>,
    device_name: Arc<Mutex<Option<String>>>,
    share_mode: Arc<Mutex<Option<ShareMode>>>,
}

impl WasapiRecorderBackend {
    pub fn new(settings: WasapiSettings) -> Self {
        Self {
            settings,
            state: None,
            broken: Arc::default(),
            running: Arc::default(),
            join_handle: None,
            sample_rate: Arc::default(),
            channels: Arc::default(),
            actual_frames: Arc::default(),
            device_name: Arc::default(),
            share_mode: Arc::default(),
        }
    }

    fn run_capture(
        settings: WasapiSettings,
        state: Arc<RecorderStateCell>,
        broken: Arc<AtomicBool>,
        running: Arc<AtomicBool>,
        actual_frames: Arc<AtomicU32>,
        sample_rate: Arc<AtomicU32>,
        channels: Arc<AtomicU32>,
        device_name: Arc<Mutex<Option<String>>>,
        share_mode: Arc<Mutex<Option<ShareMode>>>,
    ) -> Result<()> {
        let _ = initialize_mta().ok();

        let enumerator = DeviceEnumerator::new().context("create device enumerator")?;
        let device = enumerator
            .get_default_device(&Direction::Capture)
            .context("get default input device")?;
        let dev_name = device.get_friendlyname().ok();
        *device_name.lock().unwrap() = dev_name;

        let mut audio_client = device
            .get_iaudioclient()
            .context("get audio client")?;

        let mix_format = if settings.sample_rate.is_none() || settings.channels.is_none() {
            Some(audio_client.get_mixformat().context("get mix format")?)
        } else {
            None
        };

        let desired_sr = if let Some(sr) = settings.sample_rate {
            sr as usize
        } else {
            mix_format.as_ref().unwrap().get_samplespersec() as usize
        };
        let desired_ch = if let Some(ch) = settings.channels {
            ch as usize
        } else {
            mix_format.as_ref().unwrap().get_nchannels() as usize
        };
        let desired_format =
            WaveFormat::new(32, 32, &SampleType::Float, desired_sr, desired_ch, None);

        let (actual_format, mode) = if settings.exclusive {
            let format = audio_client
                .is_supported_exclusive_with_quirks(&desired_format)
                .context("exclusive capture format not supported")?;
            let (_def_period, min_period) = audio_client
                .get_device_period()
                .context("get device period")?;
            let desired_period = audio_client
                .calculate_aligned_period_near(
                    if let Some(bs) = settings.buffer_size {
                        calculate_period_100ns(bs as i64, format.get_samplespersec() as i64)
                    } else {
                        min_period
                    },
                    Some(128),
                    &format,
                )
                .context("calculate aligned period")?;
            (
                format,
                StreamMode::EventsExclusive {
                    period_hns: desired_period,
                },
            )
        } else {
            let (def_period, _min_period) = audio_client
                .get_device_period()
                .context("get device period")?;
            (
                desired_format,
                StreamMode::EventsShared {
                    autoconvert: true,
                    buffer_duration_hns: if let Some(bs) = settings.buffer_size {
                        calculate_period_100ns(bs as i64, desired_sr as i64)
                    } else {
                        def_period
                    },
                },
            )
        };

        audio_client
            .initialize_client(&actual_format, &Direction::Capture, &mode)
            .context("initialize audio client")?;

        let actual_sr = actual_format.get_samplespersec();
        let actual_ch = actual_format.get_nchannels();

        sample_rate.store(actual_sr, Ordering::Relaxed);
        channels.store(actual_ch as u32, Ordering::Relaxed);
        state.get().0.sample_rate = actual_sr;
        *share_mode.lock().unwrap() = if settings.exclusive {
            Some(ShareMode::Exclusive)
        } else {
            Some(ShareMode::Shared)
        };

        let h_event = audio_client
            .set_get_eventhandle()
            .context("get event handle")?;
        let capture_client = audio_client
            .get_audiocaptureclient()
            .context("get capture client")?;

        audio_client.start_stream().context("start stream")?;

        let buffer_size = audio_client.get_buffer_size().context("get buffer size")? as usize;
        let bytes_per_frame = actual_format.get_blockalign() as usize;
        let mut byte_buf: Vec<u8> = vec![0u8; bytes_per_frame * (buffer_size + 1024)];
        let mut loop_result = Ok(());

        loop {
            if !running.load(Ordering::Relaxed) {
                let _ = audio_client.stop_stream();
                break;
            }

            let (nbr_frames, _info) = match capture_client.read_from_device(&mut byte_buf) {
                Ok(v) => v,
                Err(e) => {
                    let _ = audio_client.stop_stream();
                    broken.store(true, Ordering::Relaxed);
                    loop_result = Err(anyhow::anyhow!(e));
                    break;
                }
            };

            if nbr_frames == 0 {
                if h_event.wait_for_event(100).is_err() {
                    let _ = audio_client.stop_stream();
                    broken.store(true, Ordering::Relaxed);
                    loop_result = Err(anyhow::anyhow!("event wait timeout"));
                    break;
                }
                continue;
            }

            actual_frames.store(nbr_frames, Ordering::Relaxed);

            let n_samples = nbr_frames as usize * actual_ch as usize;
            let expected_bytes = n_samples * 4;

            let f32_slice: &[f32] = if byte_buf.len() >= expected_bytes {
                let ptr = byte_buf.as_ptr() as *const f32;
                unsafe { std::slice::from_raw_parts(ptr, n_samples) }
            } else {
                byte_buf.resize(expected_bytes, 0u8);
                let ptr = byte_buf.as_ptr() as *const f32;
                unsafe { std::slice::from_raw_parts(ptr, n_samples) }
            };

            let captured_slice = &f32_slice[..n_samples];

            let (mixer, rec) = state.get();
            if actual_ch == 1 {
                mixer.record_mono(captured_slice);
            } else {
                mixer.record_stereo(captured_slice);
            }

            let latency_sec = nbr_frames as f64 / actual_sr as f64;
            rec.push(latency_sec);

            if h_event.wait_for_event(1000).is_err() {
                let _ = audio_client.stop_stream();
                broken.store(true, Ordering::Relaxed);
                loop_result = Err(anyhow::anyhow!("event wait timeout"));
                break;
            }
        }

        loop_result
    }
}

impl RecorderBackend for WasapiRecorderBackend {
    fn setup(&mut self, setup: RecorderBackendSetup) -> Result<()> {
        self.state = Some(Arc::new(setup.into()));
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        let settings = self.settings.clone();
        let state = Arc::clone(self.state.as_ref().context("not set up")?);
        let broken = Arc::clone(&self.broken);
        let running = Arc::clone(&self.running);
        let actual_frames = Arc::clone(&self.actual_frames);
        let sample_rate = Arc::clone(&self.sample_rate);
        let channels = Arc::clone(&self.channels);
        let device_name = Arc::clone(&self.device_name);
        let share_mode = Arc::clone(&self.share_mode);

        running.store(true, Ordering::Relaxed);

        let join_handle = std::thread::Builder::new()
            .name("wasapi-capture".into())
            .spawn(move || {
                if let Err(e) = WasapiRecorderBackend::run_capture(
                    settings,
                    state,
                    broken,
                    running,
                    actual_frames,
                    sample_rate,
                    channels,
                    device_name,
                    share_mode,
                ) {
                    eprintln!("wasapi capture error: {e}");
                }
            })
            .context("spawn capture thread")?;

        self.join_handle = Some(join_handle);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
        Ok(())
    }

    fn consume_broken(&self) -> bool {
        self.broken.fetch_and(false, Ordering::Relaxed)
    }

    fn stream_info(&mut self) -> BackendStreamInfo {
        let frames = self.actual_frames.load(Ordering::Relaxed);
        BackendStreamInfo::Wasapi(WasapiStreamInfo {
            settings: self.settings.clone(),
            sample_rate: {
                let sr = self.sample_rate.load(Ordering::Relaxed);
                if sr > 0 { Some(sr) } else { None }
            },
            channels: {
                let ch = self.channels.load(Ordering::Relaxed);
                if ch > 0 { Some(ch as u16) } else { None }
            },
            device_name: self.device_name.lock().unwrap().clone(),
            share_mode: *self.share_mode.lock().unwrap(),
            actual_frames_per_callback: if frames > 0 { Some(frames) } else { None },
        })
    }
}

impl Drop for WasapiRecorderBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
