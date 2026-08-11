use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::Instant,
};

use anyhow::{Context, Result};
pub use wasapi::{ShareMode, StreamCategory, StreamOption};
use wasapi::{
    calculate_period_100ns, initialize_mta, AudioClient, AudioClientProperties, Device,
    DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat,
};

use super::{BackendSetup, BackendStreamInfo, RecorderBackendSetup, RecorderStateCell, StateCell};
use crate::{Backend, RecorderBackend};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleConversion {
    Float32,
    Int32,
    Int24,
    Int16,
}

impl SampleConversion {
    fn bytes_per_sample(self) -> usize {
        match self {
            SampleConversion::Float32 | SampleConversion::Int32 => 4,
            SampleConversion::Int24 => 3,
            SampleConversion::Int16 => 2,
        }
    }

    fn f32_to_bytes(self, src: &[f32], dst: &mut [u8]) {
        match self {
            SampleConversion::Float32 => {
                for (i, s) in src.iter().enumerate() {
                    dst[i * 4..(i + 1) * 4].copy_from_slice(&s.to_le_bytes());
                }
            }
            SampleConversion::Int32 => {
                for (i, s) in src.iter().enumerate() {
                    let clamped = s.clamp(-1.0, 1.0);
                    let sample = (clamped * 2147483647.0) as i32;
                    dst[i * 4..(i + 1) * 4].copy_from_slice(&sample.to_le_bytes());
                }
            }
            SampleConversion::Int24 => {
                for (i, s) in src.iter().enumerate() {
                    let clamped = s.clamp(-1.0, 1.0);
                    let sample = (clamped * 8388607.0) as i32;
                    let bytes = sample.to_le_bytes();
                    dst[i * 3..(i + 1) * 3].copy_from_slice(&bytes[..3]);
                }
            }
            SampleConversion::Int16 => {
                for (i, s) in src.iter().enumerate() {
                    let clamped = s.clamp(-1.0, 1.0);
                    let sample = (clamped * 32767.0) as i16;
                    dst[i * 2..(i + 1) * 2].copy_from_slice(&sample.to_le_bytes());
                }
            }
        }
    }

    fn bytes_to_f32(self, src: &[u8], dst: &mut [f32]) {
        match self {
            SampleConversion::Float32 => {
                let ptr = src.as_ptr() as *const f32;
                let f32_slice =
                    unsafe { std::slice::from_raw_parts(ptr, dst.len().min(src.len() / 4)) };
                dst[..f32_slice.len()].copy_from_slice(f32_slice);
            }
            SampleConversion::Int32 => {
                let count = dst.len().min(src.len() / 4);
                for i in 0..count {
                    let mut bytes = [0u8; 4];
                    bytes.copy_from_slice(&src[i * 4..(i + 1) * 4]);
                    let sample = i32::from_le_bytes(bytes);
                    dst[i] = sample as f32 / 2147483648.0;
                }
            }
            SampleConversion::Int24 => {
                let count = dst.len().min(src.len() / 3);
                for i in 0..count {
                    let mut bytes = [0u8; 4];
                    bytes[..3].copy_from_slice(&src[i * 3..(i + 1) * 3]);
                    if bytes[2] & 0x80 != 0 {
                        bytes[3] = 0xff;
                    }
                    let sample = i32::from_le_bytes(bytes);
                    dst[i] = sample as f32 / 8388608.0;
                }
            }
            SampleConversion::Int16 => {
                let count = dst.len().min(src.len() / 2);
                for i in 0..count {
                    let mut bytes = [0u8; 2];
                    bytes.copy_from_slice(&src[i * 2..(i + 1) * 2]);
                    let sample = i16::from_le_bytes(bytes);
                    dst[i] = sample as f32 / 32768.0;
                }
            }
        }
    }
}

fn mode_period_hns(mode: &StreamMode) -> u32 {
    match mode {
        StreamMode::EventsExclusive { period_hns } => *period_hns as u32,
        StreamMode::EventsShared { buffer_duration_hns, .. } => *buffer_duration_hns as u32,
        StreamMode::PollingExclusive { period_hns, .. } => *period_hns as u32,
        StreamMode::PollingShared { buffer_duration_hns, .. } => *buffer_duration_hns as u32,
    }
}

fn probe_exclusive_format(
    device: &Device,
    sample_rate: Option<u32>,
    desired_ch: usize,
    buffer_size: Option<u32>,
    direction: Direction,
    stream_category: StreamCategory,
    stream_option: Option<StreamOption>,
) -> Result<(AudioClient, WaveFormat, SampleConversion, StreamMode)> {
    let sample_rates: Vec<usize> = if let Some(sr) = sample_rate {
        vec![sr as usize]
    } else {
        vec![192000, 96000, 48000, 44100, 24000, 22050, 16000, 12000, 11025, 8000]
    };

    let format_candidates: [(usize, usize, SampleType, SampleConversion); 4] = [
        (32, 32, SampleType::Float, SampleConversion::Float32),
        (32, 32, SampleType::Int, SampleConversion::Int32),
        (24, 24, SampleType::Int, SampleConversion::Int24),
        (16, 16, SampleType::Int, SampleConversion::Int16),
    ];

    let mut last_err = String::new();
    for sr in &sample_rates {
        for (storebits, validbits, sample_type, conversion) in &format_candidates {
            let format = WaveFormat::new(
                *storebits,
                *validbits,
                sample_type,
                *sr,
                desired_ch,
                None,
            );

            let mut audio_client = match device.get_iaudioclient() {
                Ok(c) => c,
                Err(e) => {
                    last_err = format!("get_iaudioclient: {e}");
                    continue;
                }
            };

            let supported = match audio_client.is_supported_exclusive_with_quirks(&format) {
                Ok(f) => f,
                Err(e) => {
                    last_err = format!(
                        "{storebits}bit {:?} {}Hz: {e}",
                        sample_type, sr
                    );
                    continue;
                }
            };

            let (_def_period, min_period) = match audio_client.get_device_period() {
                Ok(p) => p,
                Err(e) => {
                    last_err = format!("get_device_period: {e}");
                    continue;
                }
            };

            let period_hns = if let Some(bs) = buffer_size {
                calculate_period_100ns(bs as i64, supported.get_samplespersec() as i64)
            } else {
                min_period
            };

            let desired_period = match audio_client
                .calculate_aligned_period_near(period_hns, Some(128), &supported)
            {
                Ok(p) => p,
                Err(e) => {
                    last_err = format!("calculate_aligned_period_near: {e}");
                    continue;
                }
            };

            let mode = StreamMode::EventsExclusive {
                period_hns: desired_period,
            };

            let props = {
                let mut p = AudioClientProperties::new().set_category(stream_category);
                if let Some(opt) = stream_option {
                    p = p.set_option(opt);
                }
                p
            };
            let _ = audio_client.set_properties(props);

            match audio_client.initialize_client(&supported, &direction, &mode) {
                Ok(()) => return Ok((audio_client, supported, *conversion, mode)),
                Err(e) => {
                    last_err = format!(
                        "{storebits}bit {:?} {}Hz init: {e}",
                        sample_type, sr
                    );
                }
            }
        }
    }
    Err(anyhow::anyhow!(
        "no exclusive format found for {desired_ch}ch. last: {last_err}",
    ))
}

#[derive(Debug, Clone)]
pub struct WasapiSettings {
    pub buffer_size: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub share_mode: ShareMode,
    pub stream_category: StreamCategory,
    pub stream_option: Option<StreamOption>,
}

impl Default for WasapiSettings {
    fn default() -> Self {
        Self {
            buffer_size: None,
            sample_rate: None,
            channels: None,
            share_mode: ShareMode::Shared,
            stream_category: StreamCategory::Other,
            stream_option: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WasapiStreamInfo {
    pub settings: WasapiSettings,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub device_name: Option<String>,
    pub actual_frames_per_callback: Option<u32>,
    pub default_period_hns: Option<u32>,
    pub min_period_hns: Option<u32>,
    pub min_aligned_period_hns: Option<u32>,
    pub actual_bits_per_sample: Option<u16>,
    pub actual_valid_bits_per_sample: Option<u16>,
    pub actual_sample_type: Option<String>,
    pub actual_period_hns: Option<u32>,
    pub buffer_size_frames: Option<u32>,
    pub channel_mask: Option<u32>,
    pub adapter_name: Option<String>,
    pub current_padding: Option<u32>,
    pub available_space: Option<u32>,
    pub clock_position: Option<u64>,
    pub clock_frequency: Option<u64>,
}

impl std::fmt::Display for WasapiStreamInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "settings.buffer_size: {:?}", self.settings.buffer_size)?;
        writeln!(f, "settings.sample_rate: {:?}", self.settings.sample_rate)?;
        writeln!(f, "settings.channels: {:?}", self.settings.channels)?;
        writeln!(f, "settings.exclusive: {:?}", self.settings.share_mode)?;
        writeln!(f, "sample_rate: {:?}", self.sample_rate)?;
        writeln!(f, "channels: {:?}", self.channels)?;
        writeln!(f, "device_name: {:?}", self.device_name)?;
        writeln!(f, "actual_frames_per_callback: {:?}", self.actual_frames_per_callback)?;
        writeln!(f, "default_period_hns: {:?}", self.default_period_hns)?;
        writeln!(f, "min_period_hns: {:?}", self.min_period_hns)?;
        writeln!(f, "min_aligned_period_hns: {:?}", self.min_aligned_period_hns)?;
        writeln!(f, "actual_bits_per_sample: {:?}", self.actual_bits_per_sample)?;
        writeln!(f, "actual_valid_bits_per_sample: {:?}", self.actual_valid_bits_per_sample)?;
        writeln!(f, "actual_sample_type: {:?}", self.actual_sample_type)?;
        writeln!(f, "actual_period_hns: {:?}", self.actual_period_hns)?;
        writeln!(f, "buffer_size_frames: {:?}", self.buffer_size_frames)?;
        writeln!(f, "channel_mask: 0x{:08X}", self.channel_mask.unwrap_or(0))?;
        writeln!(f, "adapter_name: {:?}", self.adapter_name)?;
        writeln!(f, "current_padding: {:?}", self.current_padding)?;
        writeln!(f, "available_space: {:?}", self.available_space)?;
        writeln!(f, "clock_position: {:?}", self.clock_position)?;
        writeln!(f, "clock_frequency: {:?}", self.clock_frequency)?;
        Ok(())
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
    default_period_hns: Arc<AtomicU32>,
    min_period_hns: Arc<AtomicU32>,
    min_aligned_period_hns: Arc<AtomicU32>,
    actual_bits: Arc<AtomicU32>,
    actual_valid_bits: Arc<AtomicU32>,
    actual_sample_type: Arc<Mutex<Option<String>>>,
    actual_period_hns: Arc<AtomicU32>,
    buffer_size_frames: Arc<AtomicU32>,
    channel_mask: Arc<AtomicU32>,
    current_padding: Arc<AtomicU32>,
    available_space: Arc<AtomicU32>,
    clock_position: Arc<AtomicU64>,
    clock_frequency: Arc<AtomicU64>,
    adapter_name: Arc<Mutex<Option<String>>>,
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
            default_period_hns: Arc::default(),
            min_period_hns: Arc::default(),
            min_aligned_period_hns: Arc::default(),
            actual_bits: Arc::default(),
            actual_valid_bits: Arc::default(),
            actual_sample_type: Arc::default(),
            actual_period_hns: Arc::default(),
            buffer_size_frames: Arc::default(),
            channel_mask: Arc::default(),
            current_padding: Arc::default(),
            available_space: Arc::default(),
            clock_position: Arc::default(),
            clock_frequency: Arc::default(),
            adapter_name: Arc::default(),
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
        default_period_hns: Arc<AtomicU32>,
        min_period_hns: Arc<AtomicU32>,
        min_aligned_period_hns: Arc<AtomicU32>,
        actual_bits: Arc<AtomicU32>,
        actual_valid_bits: Arc<AtomicU32>,
        actual_sample_type: Arc<Mutex<Option<String>>>,
        actual_period_hns: Arc<AtomicU32>,
        buffer_size_frames: Arc<AtomicU32>,
        channel_mask: Arc<AtomicU32>,
        current_padding: Arc<AtomicU32>,
        available_space: Arc<AtomicU32>,
        clock_position: Arc<AtomicU64>,
        clock_frequency: Arc<AtomicU64>,
        adapter_name: Arc<Mutex<Option<String>>>,
    ) -> Result<()> {
        let _ = initialize_mta().ok();

        let enumerator = DeviceEnumerator::new().context("create device enumerator")?;
        let device = enumerator
            .get_default_device(&Direction::Render)
            .context("get default output device")?;
        let dev_name = device.get_friendlyname().ok();
        *device_name.lock().unwrap() = dev_name;
        *adapter_name.lock().unwrap() = device.get_interface_friendlyname().ok();

        let mix_format = if settings.sample_rate.is_none() || settings.channels.is_none() {
            let client = device
                .get_iaudioclient()
                .context("get audio client")?;
            Some(client.get_mixformat().context("get mix format")?)
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

        let (audio_client, actual_format, conversion, mode) = if matches!(settings.share_mode, ShareMode::Exclusive) {
            probe_exclusive_format(
                &device,
                settings.sample_rate,
                desired_ch,
                settings.buffer_size,
                Direction::Render,
                settings.stream_category,
                settings.stream_option,
            )
            .context("exclusive format not supported")?
        } else {
            let mut client = device
                .get_iaudioclient()
                .context("get audio client")?;
            let (def_period, _min_period) = client
                .get_device_period()
                .context("get device period")?;
            let mode = StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: if let Some(bs) = settings.buffer_size {
                    calculate_period_100ns(bs as i64, desired_sr as i64)
                } else {
                    def_period
                },
            };
            let props = {
                let mut p = AudioClientProperties::new().set_category(settings.stream_category);
                if let Some(opt) = settings.stream_option {
                    p = p.set_option(opt);
                }
                p
            };
            let _ = client.set_properties(props);
            client
                .initialize_client(&desired_format, &Direction::Render, &mode)
                .context("initialize audio client")?;
            (
                client,
                desired_format,
                SampleConversion::Float32,
                mode,
            )
        };

        let actual_sr = actual_format.get_samplespersec();
        let actual_ch = actual_format.get_nchannels();
        let actual_ch_u32 = actual_ch as u32;

        if let Ok((def_per, min_per)) = audio_client.get_device_period() {
            default_period_hns.store(def_per as u32, Ordering::Relaxed);
            min_period_hns.store(min_per as u32, Ordering::Relaxed);
        }
        actual_period_hns.store(mode_period_hns(&mode), Ordering::Relaxed);
        if let Ok(aligned_min) =
            audio_client.calculate_aligned_period_near(0, Some(128), &actual_format)
        {
            min_aligned_period_hns.store(aligned_min as u32, Ordering::Relaxed);
        }
        actual_bits.store(actual_format.get_bitspersample() as u32, Ordering::Relaxed);
        actual_valid_bits.store(actual_format.get_validbitspersample() as u32, Ordering::Relaxed);
        channel_mask.store(actual_format.get_dwchannelmask(), Ordering::Relaxed);
        buffer_size_frames.store(audio_client.get_buffer_size().unwrap_or(0), Ordering::Relaxed);
        *actual_sample_type.lock().unwrap() = match actual_format.get_subformat() {
            Ok(SampleType::Float) => Some("Float".into()),
            Ok(SampleType::Int) => Some("Int".into()),
            Err(_) => None,
        };

        sample_rate.store(actual_sr, Ordering::Relaxed);
        channels.store(actual_ch_u32, Ordering::Relaxed);
        state.get().0.sample_rate = actual_sr;

        let h_event = audio_client
            .set_get_eventhandle()
            .context("get event handle")?;
        let render_client = audio_client
            .get_audiorenderclient()
            .context("get render client")?;
        let audio_clock = audio_client.get_audioclock().ok();
        if let Some(ref clock) = audio_clock {
            if let Ok(freq) = clock.get_frequency() {
                clock_frequency.store(freq, Ordering::Relaxed);
            }
        }

        audio_client.start_stream().context("start stream")?;

        let mut f32_buf = Vec::new();
        let mut byte_buf = Vec::new();
        let mut loop_result = Ok(());

        loop {
            if !running.load(Ordering::Relaxed) {
                let _ = audio_client.stop_stream();
                break;
            }

            let callback_instant = Instant::now();

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

            let n_bytes = n_samples * conversion.bytes_per_sample();
            byte_buf.resize(n_bytes, 0u8);
            conversion.f32_to_bytes(&f32_buf, &mut byte_buf);

            if let Err(e) = render_client.write_to_device(buffer_frames as usize, &byte_buf, None) {
                let _ = audio_client.stop_stream();
                broken.store(true, Ordering::Relaxed);
                loop_result = Err(anyhow::anyhow!(e));
                break;
            }

            let post_padding = audio_client.get_current_padding().unwrap_or(0);
            current_padding.store(post_padding, Ordering::Relaxed);
            available_space.store(buffer_frames, Ordering::Relaxed);
            if let Some(ref clock) = audio_clock {
                if let Ok((pos, _timer)) = clock.get_position() {
                    clock_position.store(pos, Ordering::Relaxed);
                }
            }

            let stream_delay_sec = if post_padding > 0 {
                post_padding as f64 / actual_sr as f64
            } else {
                buffer_frames as f64 / actual_sr as f64
            };
            let total_delay_sec = stream_delay_sec + callback_instant.elapsed().as_secs_f64();
            rec.push(total_delay_sec);

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

impl Backend for WasapiBackend {
    fn setup(&mut self, setup: BackendSetup) -> Result<()> {
        self.state = Some(Arc::new(setup.into()));
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        let settings = self.settings.clone();
        let state = Arc::clone(self.state.as_ref().context("not set up")?);
        let broken = Arc::clone(&self.broken);
        let handle_broken = Arc::clone(&self.broken);
        let running = Arc::clone(&self.running);
        let actual_frames = Arc::clone(&self.actual_frames);
        let sample_rate = Arc::clone(&self.sample_rate);
        let channels = Arc::clone(&self.channels);
        let device_name = Arc::clone(&self.device_name);
        let default_period_hns = Arc::clone(&self.default_period_hns);
        let min_period_hns = Arc::clone(&self.min_period_hns);
        let min_aligned_period_hns = Arc::clone(&self.min_aligned_period_hns);
        let actual_bits = Arc::clone(&self.actual_bits);
        let actual_sample_type = Arc::clone(&self.actual_sample_type);
        let actual_period_hns = Arc::clone(&self.actual_period_hns);
        let actual_valid_bits = Arc::clone(&self.actual_valid_bits);
        let buffer_size_frames = Arc::clone(&self.buffer_size_frames);
        let channel_mask = Arc::clone(&self.channel_mask);
        let current_padding = Arc::clone(&self.current_padding);
        let available_space = Arc::clone(&self.available_space);
        let clock_position = Arc::clone(&self.clock_position);
        let clock_frequency = Arc::clone(&self.clock_frequency);
        let adapter_name = Arc::clone(&self.adapter_name);

        running.store(true, Ordering::Relaxed);

        let join_handle = std::thread::Builder::new()
            .name("wasapi-playback".into())
            .spawn(move || {
                if WasapiBackend::run_playback(
                    settings,
                    state,
                    broken,
                    running,
                    actual_frames,
                    sample_rate,
                    channels,
                    device_name,
                    default_period_hns,
                    min_period_hns,
                    min_aligned_period_hns,
                    actual_bits,
                    actual_valid_bits,
                    actual_sample_type,
                    actual_period_hns,
                    buffer_size_frames,
                    channel_mask,
                    current_padding,
                    available_space,
                    clock_position,
                    clock_frequency,
                    adapter_name,
                ).is_err() {
                    handle_broken.store(true, Ordering::Relaxed);
                };
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
            actual_frames_per_callback: if frames > 0 { Some(frames) } else { None },
            default_period_hns: {
                let v = self.default_period_hns.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
            min_period_hns: {
                let v = self.min_period_hns.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
            min_aligned_period_hns: {
                let v = self.min_aligned_period_hns.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
            actual_bits_per_sample: {
                let v = self.actual_bits.load(Ordering::Relaxed);
                if v > 0 { Some(v as u16) } else { None }
            },
            actual_valid_bits_per_sample: {
                let v = self.actual_valid_bits.load(Ordering::Relaxed);
                if v > 0 { Some(v as u16) } else { None }
            },
            actual_sample_type: self.actual_sample_type.lock().unwrap().clone(),
            actual_period_hns: {
                let v = self.actual_period_hns.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
            buffer_size_frames: {
                let v = self.buffer_size_frames.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
            channel_mask: {
                let v = self.channel_mask.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
            adapter_name: self.adapter_name.lock().unwrap().clone(),
            current_padding: {
                let v = self.current_padding.load(Ordering::Relaxed);
                Some(v)
            },
            available_space: {
                let v = self.available_space.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
            clock_position: {
                let v = self.clock_position.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
            clock_frequency: {
                let v = self.clock_frequency.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
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
    default_period_hns: Arc<AtomicU32>,
    min_period_hns: Arc<AtomicU32>,
    min_aligned_period_hns: Arc<AtomicU32>,
    actual_bits: Arc<AtomicU32>,
    actual_valid_bits: Arc<AtomicU32>,
    actual_sample_type: Arc<Mutex<Option<String>>>,
    actual_period_hns: Arc<AtomicU32>,
    buffer_size_frames: Arc<AtomicU32>,
    channel_mask: Arc<AtomicU32>,
    current_padding: Arc<AtomicU32>,
    available_space: Arc<AtomicU32>,
    clock_position: Arc<AtomicU64>,
    clock_frequency: Arc<AtomicU64>,
    adapter_name: Arc<Mutex<Option<String>>>,
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
            default_period_hns: Arc::default(),
            min_period_hns: Arc::default(),
            min_aligned_period_hns: Arc::default(),
            actual_bits: Arc::default(),
            actual_valid_bits: Arc::default(),
            actual_sample_type: Arc::default(),
            actual_period_hns: Arc::default(),
            buffer_size_frames: Arc::default(),
            channel_mask: Arc::default(),
            current_padding: Arc::default(),
            available_space: Arc::default(),
            clock_position: Arc::default(),
            clock_frequency: Arc::default(),
            adapter_name: Arc::default(),
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
        default_period_hns: Arc<AtomicU32>,
        min_period_hns: Arc<AtomicU32>,
        min_aligned_period_hns: Arc<AtomicU32>,
        actual_bits: Arc<AtomicU32>,
        actual_valid_bits: Arc<AtomicU32>,
        actual_sample_type: Arc<Mutex<Option<String>>>,
        actual_period_hns: Arc<AtomicU32>,
        buffer_size_frames: Arc<AtomicU32>,
        channel_mask: Arc<AtomicU32>,
        current_padding: Arc<AtomicU32>,
        available_space: Arc<AtomicU32>,
        clock_position: Arc<AtomicU64>,
        clock_frequency: Arc<AtomicU64>,
        adapter_name: Arc<Mutex<Option<String>>>,
    ) -> Result<()> {
        let _ = initialize_mta().ok();

        let enumerator = DeviceEnumerator::new().context("create device enumerator")?;
        let device = enumerator
            .get_default_device(&Direction::Capture)
            .context("get default input device")?;
        let dev_name = device.get_friendlyname().ok();
        *device_name.lock().unwrap() = dev_name;
        *adapter_name.lock().unwrap() = device.get_interface_friendlyname().ok();

        let mix_format = if settings.sample_rate.is_none() || settings.channels.is_none() {
            let client = device
                .get_iaudioclient()
                .context("get audio client")?;
            Some(client.get_mixformat().context("get mix format")?)
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

        let (audio_client, actual_format, conversion, mode) = if matches!(settings.share_mode, ShareMode::Exclusive) {
            probe_exclusive_format(
                &device,
                settings.sample_rate,
                desired_ch,
                settings.buffer_size,
                Direction::Capture,
                settings.stream_category,
                settings.stream_option,
            )
            .context("exclusive capture format not supported")?
        } else {
            let desired_format =
                WaveFormat::new(32, 32, &SampleType::Float, desired_sr, desired_ch, None);
            let mut client = device
                .get_iaudioclient()
                .context("get audio client")?;
            let (def_period, _min_period) = client
                .get_device_period()
                .context("get device period")?;
            let mode = StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: if let Some(bs) = settings.buffer_size {
                    calculate_period_100ns(bs as i64, desired_sr as i64)
                } else {
                    def_period
                },
            };
            let props = {
                let mut p = AudioClientProperties::new().set_category(settings.stream_category);
                if let Some(opt) = settings.stream_option {
                    p = p.set_option(opt);
                }
                p
            };
            let _ = client.set_properties(props);
            client
                .initialize_client(&desired_format, &Direction::Capture, &mode)
                .context("initialize audio client")?;
            (
                client,
                desired_format,
                SampleConversion::Float32,
                mode,
            )
        };

        let actual_sr = actual_format.get_samplespersec();
        let actual_ch = actual_format.get_nchannels();

        if let Ok((def_per, min_per)) = audio_client.get_device_period() {
            default_period_hns.store(def_per as u32, Ordering::Relaxed);
            min_period_hns.store(min_per as u32, Ordering::Relaxed);
        }
        actual_period_hns.store(mode_period_hns(&mode), Ordering::Relaxed);
        if let Ok(aligned_min) =
            audio_client.calculate_aligned_period_near(0, Some(128), &actual_format)
        {
            min_aligned_period_hns.store(aligned_min as u32, Ordering::Relaxed);
        }
        actual_bits.store(actual_format.get_bitspersample() as u32, Ordering::Relaxed);
        actual_valid_bits.store(actual_format.get_validbitspersample() as u32, Ordering::Relaxed);
        channel_mask.store(actual_format.get_dwchannelmask(), Ordering::Relaxed);
        buffer_size_frames.store(audio_client.get_buffer_size().unwrap_or(0), Ordering::Relaxed);
        *actual_sample_type.lock().unwrap() = match actual_format.get_subformat() {
            Ok(SampleType::Float) => Some("Float".into()),
            Ok(SampleType::Int) => Some("Int".into()),
            Err(_) => None,
        };

        sample_rate.store(actual_sr, Ordering::Relaxed);
        channels.store(actual_ch as u32, Ordering::Relaxed);
        state.get().0.sample_rate = actual_sr;

        let h_event = audio_client
            .set_get_eventhandle()
            .context("get event handle")?;
        let capture_client = audio_client
            .get_audiocaptureclient()
            .context("get capture client")?;
        let audio_clock = audio_client.get_audioclock().ok();
        if let Some(ref clock) = audio_clock {
            if let Ok(freq) = clock.get_frequency() {
                clock_frequency.store(freq, Ordering::Relaxed);
            }
        }

        audio_client.start_stream().context("start stream")?;

        let buffer_size = audio_client.get_buffer_size().context("get buffer size")? as usize;
        let bytes_per_frame = actual_format.get_blockalign() as usize;
        let mut byte_buf: Vec<u8> = vec![0u8; bytes_per_frame * (buffer_size + 1024)];
        let mut f32_buf: Vec<f32> = Vec::new();
        let mut loop_result = Ok(());

        loop {
            if !running.load(Ordering::Relaxed) {
                let _ = audio_client.stop_stream();
                break;
            }

            let callback_instant = Instant::now();

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
            f32_buf.resize(n_samples, 0f32);
            conversion.bytes_to_f32(&byte_buf, &mut f32_buf);

            let (mixer, rec) = state.get();
            if actual_ch == 1 {
                mixer.record_mono(&f32_buf);
            } else {
                mixer.record_stereo(&f32_buf);
            }

            let post_padding = audio_client.get_current_padding().unwrap_or(0);
            current_padding.store(post_padding, Ordering::Relaxed);
            available_space.store(nbr_frames, Ordering::Relaxed);
            if let Some(ref clock) = audio_clock {
                if let Ok((pos, _timer)) = clock.get_position() {
                    clock_position.store(pos, Ordering::Relaxed);
                }
            }

            let stream_delay_sec = if post_padding > 0 {
                post_padding as f64 / actual_sr as f64
            } else {
                nbr_frames as f64 / actual_sr as f64
            };
            let total_delay_sec = stream_delay_sec + callback_instant.elapsed().as_secs_f64();
            rec.push(total_delay_sec);

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
        let handle_broken = Arc::clone(&broken);
        let running = Arc::clone(&self.running);
        let actual_frames = Arc::clone(&self.actual_frames);
        let sample_rate = Arc::clone(&self.sample_rate);
        let channels = Arc::clone(&self.channels);
        let device_name = Arc::clone(&self.device_name);
        let default_period_hns = Arc::clone(&self.default_period_hns);
        let min_period_hns = Arc::clone(&self.min_period_hns);
        let min_aligned_period_hns = Arc::clone(&self.min_aligned_period_hns);
        let actual_bits = Arc::clone(&self.actual_bits);
        let actual_sample_type = Arc::clone(&self.actual_sample_type);
        let actual_period_hns = Arc::clone(&self.actual_period_hns);
        let actual_valid_bits = Arc::clone(&self.actual_valid_bits);
        let buffer_size_frames = Arc::clone(&self.buffer_size_frames);
        let channel_mask = Arc::clone(&self.channel_mask);
        let current_padding = Arc::clone(&self.current_padding);
        let available_space = Arc::clone(&self.available_space);
        let clock_position = Arc::clone(&self.clock_position);
        let clock_frequency = Arc::clone(&self.clock_frequency);
        let adapter_name = Arc::clone(&self.adapter_name);

        running.store(true, Ordering::Relaxed);

        let join_handle = std::thread::Builder::new()
            .name("wasapi-capture".into())
            .spawn(move || {
                if WasapiRecorderBackend::run_capture(
                    settings,
                    state,
                    broken,
                    running,
                    actual_frames,
                    sample_rate,
                    channels,
                    device_name,
                    default_period_hns,
                    min_period_hns,
                    min_aligned_period_hns,
                    actual_bits,
                    actual_valid_bits,
                    actual_sample_type,
                    actual_period_hns,
                    buffer_size_frames,
                    channel_mask,
                    current_padding,
                    available_space,
                    clock_position,
                    clock_frequency,
                    adapter_name,
                ).is_err() {
                    handle_broken.store(true, Ordering::Relaxed);
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
            actual_frames_per_callback: if frames > 0 { Some(frames) } else { None },
            default_period_hns: {
                let v = self.default_period_hns.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
            min_period_hns: {
                let v = self.min_period_hns.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
            min_aligned_period_hns: {
                let v = self.min_aligned_period_hns.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
            actual_bits_per_sample: {
                let v = self.actual_bits.load(Ordering::Relaxed);
                if v > 0 { Some(v as u16) } else { None }
            },
            actual_valid_bits_per_sample: {
                let v = self.actual_valid_bits.load(Ordering::Relaxed);
                if v > 0 { Some(v as u16) } else { None }
            },
            actual_sample_type: self.actual_sample_type.lock().unwrap().clone(),
            actual_period_hns: {
                let v = self.actual_period_hns.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
            buffer_size_frames: {
                let v = self.buffer_size_frames.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
            channel_mask: {
                let v = self.channel_mask.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
            adapter_name: self.adapter_name.lock().unwrap().clone(),
            current_padding: {
                let v = self.current_padding.load(Ordering::Relaxed);
                Some(v)
            },
            available_space: {
                let v = self.available_space.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
            clock_position: {
                let v = self.clock_position.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
            clock_frequency: {
                let v = self.clock_frequency.load(Ordering::Relaxed);
                if v > 0 { Some(v) } else { None }
            },
        })
    }
}

impl Drop for WasapiRecorderBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
