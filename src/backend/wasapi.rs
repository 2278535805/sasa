use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
pub use wasapi::{calculate_period_100ns, ShareMode, StreamCategory, StreamOption};
use wasapi::{
    deinitialize, initialize_mta, AudioClient, AudioClientProperties, AudioClock, Device,
    DeviceEnumerator, Direction, Handle, SampleType, StreamMode, WasapiError, WaveFormat,
};

use super::{BackendSetup, BackendStreamInfo, RecorderBackendSetup, RecorderStateCell, StateCell};
use crate::{Backend, RecorderBackend};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleConversion {
    Float32,
    Int32,
    Int24In32,
    Int24,
    Int16,
}

impl SampleConversion {
    fn bytes_per_sample(self) -> usize {
        match self {
            SampleConversion::Float32 | SampleConversion::Int32 | SampleConversion::Int24In32 => 4,
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
            SampleConversion::Int24In32 => {
                for (i, s) in src.iter().enumerate() {
                    let clamped = s.clamp(-1.0, 1.0);
                    let sample = ((clamped * 8388607.0) as i32) << 8;
                    dst[i * 4..(i + 1) * 4].copy_from_slice(&sample.to_le_bytes());
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
                let count = dst.len().min(src.len() / 4);
                for i in 0..count {
                    let mut bytes = [0u8; 4];
                    bytes.copy_from_slice(&src[i * 4..(i + 1) * 4]);
                    dst[i] = f32::from_le_bytes(bytes);
                }
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
            SampleConversion::Int24In32 => {
                let count = dst.len().min(src.len() / 4);
                for i in 0..count {
                    let mut bytes = [0u8; 4];
                    bytes.copy_from_slice(&src[i * 4..(i + 1) * 4]);
                    let sample = i32::from_le_bytes(bytes) >> 8;
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

fn sample_conversion(
    sample_type: &SampleType,
    storebits: u16,
    validbits: u16,
) -> Option<SampleConversion> {
    match sample_type {
        SampleType::Float => Some(SampleConversion::Float32),
        SampleType::Int => match (storebits, validbits) {
            (16, _) => Some(SampleConversion::Int16),
            (24, _) => Some(SampleConversion::Int24),
            (32, 24) => Some(SampleConversion::Int24In32),
            (32, _) => Some(SampleConversion::Int32),
            _ => None,
        },
    }
}

fn mode_period_hns(mode: &StreamMode) -> u32 {
    match mode {
        StreamMode::EventsExclusive { period_hns } => *period_hns as u32,
        StreamMode::EventsShared {
            buffer_duration_hns,
            ..
        } => *buffer_duration_hns as u32,
        StreamMode::PollingExclusive { period_hns, .. } => *period_hns as u32,
        StreamMode::PollingShared {
            buffer_duration_hns,
            ..
        } => *buffer_duration_hns as u32,
    }
}

const AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED: i32 = 0x88890019u32 as i32;

fn is_buffer_size_not_aligned(err: &WasapiError) -> bool {
    matches!(err, WasapiError::Windows(e) if e.code().0 == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED)
}

fn audio_client_properties(
    stream_category: StreamCategory,
    stream_option: Option<StreamOption>,
) -> AudioClientProperties {
    let mut props = AudioClientProperties::new().set_category(stream_category);
    if let Some(option) = stream_option {
        props = props.set_option(option);
    }
    props
}

fn apply_audio_client_properties(client: &AudioClient, settings: &WasapiSettings) {
    let properties = audio_client_properties(settings.stream_category, settings.stream_option);
    if let Err(e) = client.set_properties(properties) {
        eprintln!("wasapi: failed to set audio client properties: {e}");
    }
}

const POLLING_BUFFER_PERIODS: i64 = 1;

fn exclusive_mode(timing: Timing, period_hns: i64) -> StreamMode {
    match timing {
        Timing::Events => StreamMode::EventsExclusive { period_hns },
        Timing::Polling => StreamMode::PollingExclusive {
            period_hns,
            buffer_duration_hns: period_hns * POLLING_BUFFER_PERIODS,
        },
    }
}

#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(uperiod: u32) -> u32;
    fn timeEndPeriod(uperiod: u32) -> u32;
}

struct PollTimer;

impl PollTimer {
    fn new() -> Self {
        let result = unsafe { timeBeginPeriod(1) };
        if result != 0 {
            eprintln!("wasapi: timeBeginPeriod(1) failed with error {result}");
        }
        Self
    }
}

impl Drop for PollTimer {
    fn drop(&mut self) {
        unsafe {
            timeEndPeriod(1);
        }
    }
}

const AVRT_PRIORITY_CRITICAL: i32 = 2;

#[link(name = "avrt")]
extern "system" {
    fn AvSetMmThreadCharacteristicsW(task_name: *const u16, task_index: *mut u32) -> *mut c_void;
    fn AvRevertMmThreadCharacteristics(handle: *mut c_void) -> i32;
    fn AvSetMmThreadPriority(handle: *mut c_void, priority: i32) -> i32;
}

struct MmcssGuard(*mut c_void);

impl MmcssGuard {
    fn new() -> Self {
        let task_name: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
        let mut task_index = 0u32;
        let handle = unsafe { AvSetMmThreadCharacteristicsW(task_name.as_ptr(), &mut task_index) };
        if handle.is_null() {
            eprintln!("wasapi: failed to register thread with MMCSS Pro Audio");
        } else if unsafe { AvSetMmThreadPriority(handle, AVRT_PRIORITY_CRITICAL) } == 0 {
            eprintln!("wasapi: failed to set MMCSS thread priority to critical");
        }
        Self(handle)
    }
}

impl Drop for MmcssGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                AvRevertMmThreadCharacteristics(self.0);
            }
        }
    }
}

struct ComGuard;

impl ComGuard {
    fn new() -> Result<Self> {
        let result = initialize_mta();
        if result.is_err() {
            anyhow::bail!("initialize MTA failed: {result:?}");
        }
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        deinitialize();
    }
}

fn probe_exclusive_format(
    device: &Device,
    settings: &WasapiSettings,
    desired_ch: usize,
    direction: Direction,
) -> Result<(AudioClient, WaveFormat, SampleConversion, StreamMode)> {
    let sample_rates: Vec<usize> = if let Some(sr) = settings.sample_rate {
        vec![sr as usize]
    } else {
        vec![
            192000, 96000, 48000, 44100, 24000, 22050, 16000, 12000, 11025, 8000,
        ]
    };

    let format_candidates: [(usize, usize, SampleType); 5] = [
        (32, 32, SampleType::Float),
        (32, 32, SampleType::Int),
        (24, 24, SampleType::Int),
        (32, 24, SampleType::Int),
        (16, 16, SampleType::Int),
    ];

    let mut last_err = String::new();
    for sr in &sample_rates {
        for (storebits, validbits, sample_type) in &format_candidates {
            let format =
                WaveFormat::new(*storebits, *validbits, sample_type, *sr, desired_ch, None);

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
                        "{storebits}bit/{validbits}valid {:?} {}Hz: {e}",
                        sample_type, sr
                    );
                    continue;
                }
            };

            if supported.get_subformat().is_err() && *storebits == 24 {
                last_err = format!(
                    "{storebits}bit/{validbits}valid {:?} {}Hz: ambiguous WAVEFORMATEX fallback for packed 24-bit",
                    sample_type, sr
                );
                continue;
            }

            let conversion = match sample_conversion(
                sample_type,
                supported.get_bitspersample(),
                supported.get_validbitspersample(),
            ) {
                Some(conversion) => conversion,
                None => {
                    last_err = format!(
                        "{storebits}bit/{validbits}valid {:?} {}Hz: unsupported sample format",
                        sample_type, sr
                    );
                    continue;
                }
            };

            let period_hns = match settings.buffer_size {
                Some(bs) => calculate_period_100ns(bs as i64, supported.get_samplespersec() as i64),
                None => audio_client
                    .get_device_period()
                    .map(|(default_period, _)| default_period)
                    .unwrap_or(0),
            };

            let desired_period =
                match audio_client.calculate_aligned_period_near(period_hns, Some(128), &supported)
                {
                    Ok(p) => p,
                    Err(e) => {
                        last_err = format!("calculate_aligned_period_near: {e}");
                        continue;
                    }
                };

            let mut mode = exclusive_mode(settings.timing, desired_period);

            apply_audio_client_properties(&audio_client, settings);

            match audio_client.initialize_client(&supported, &direction, &mode) {
                Ok(()) => return Ok((audio_client, supported, conversion, mode)),
                Err(e) if is_buffer_size_not_aligned(&e) => {
                    let aligned_frames = match audio_client.get_buffer_size() {
                        Ok(frames) => frames,
                        Err(e) => {
                            last_err = format!(
                                "{storebits}bit/{validbits}valid {:?} {}Hz get_buffer_size after unaligned: {e}",
                                sample_type, sr
                            );
                            continue;
                        }
                    };
                    let aligned_duration = calculate_period_100ns(
                        aligned_frames as i64,
                        supported.get_samplespersec() as i64,
                    );
                    mode = match settings.timing {
                        Timing::Events => StreamMode::EventsExclusive {
                            period_hns: aligned_duration,
                        },
                        Timing::Polling => StreamMode::PollingExclusive {
                            period_hns: aligned_duration,
                            buffer_duration_hns: aligned_duration * POLLING_BUFFER_PERIODS,
                        },
                    };
                    drop(audio_client);
                    let mut aligned_client = match device.get_iaudioclient() {
                        Ok(client) => client,
                        Err(e) => {
                            last_err = format!(
                                "{storebits}bit/{validbits}valid {:?} {}Hz get_iaudioclient after unaligned: {e}",
                                sample_type, sr
                            );
                            continue;
                        }
                    };
                    apply_audio_client_properties(&aligned_client, settings);
                    match aligned_client.initialize_client(&supported, &direction, &mode) {
                        Ok(()) => return Ok((aligned_client, supported, conversion, mode)),
                        Err(e) => {
                            last_err = format!(
                                "{storebits}bit/{validbits}valid {:?} {}Hz init with aligned {aligned_frames} frames: {e}",
                                sample_type, sr
                            );
                        }
                    }
                }
                Err(e) => {
                    last_err = format!(
                        "{storebits}bit/{validbits}valid {:?} {}Hz init: {e}",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timing {
    Events,
    Polling,
}

#[derive(Debug, Clone)]
pub struct WasapiSettings {
    pub buffer_size: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub share_mode: ShareMode,
    pub stream_category: StreamCategory,
    pub stream_option: Option<StreamOption>,
    pub timing: Timing,
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
            timing: Timing::Events,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WasapiStreamInfo {
    pub settings: WasapiSettings,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    pub device_name: Option<String>,
    pub frames_per_callback: Option<u32>,
    pub default_period_hns: Option<u32>,
    pub min_period_hns: Option<u32>,
    pub min_aligned_period_hns: Option<u32>,
    pub bits_per_sample: Option<u16>,
    pub valid_bits_per_sample: Option<u16>,
    pub sample_type: Option<String>,
    pub period_hns: Option<u32>,
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
        writeln!(f, "settings.share_mode: {:?}", self.settings.share_mode)?;
        writeln!(f, "settings.timing: {:?}", self.settings.timing)?;
        writeln!(f, "sample_rate: {:?}", self.sample_rate)?;
        writeln!(f, "channels: {:?}", self.channels)?;
        writeln!(f, "device_name: {:?}", self.device_name)?;
        writeln!(f, "frames_per_callback: {:?}", self.frames_per_callback)?;
        writeln!(f, "default_period_hns: {:?}", self.default_period_hns)?;
        writeln!(f, "min_period_hns: {:?}", self.min_period_hns)?;
        writeln!(
            f,
            "min_aligned_period_hns: {:?}",
            self.min_aligned_period_hns
        )?;
        writeln!(f, "bits_per_sample: {:?}", self.bits_per_sample)?;
        writeln!(f, "valid_bits_per_sample: {:?}", self.valid_bits_per_sample)?;
        writeln!(f, "sample_type: {:?}", self.sample_type)?;
        writeln!(f, "period_hns: {:?}", self.period_hns)?;
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

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl WasapiSharedState {
    fn reset(&self) {
        self.broken.store(false, Ordering::Relaxed);
        self.stalled.store(false, Ordering::Relaxed);
        self.sample_rate.store(0, Ordering::Relaxed);
        self.channels.store(0, Ordering::Relaxed);
        self.frames_per_callback.store(0, Ordering::Relaxed);
        *lock(&self.device_name) = None;
        self.default_period_hns.store(0, Ordering::Relaxed);
        self.min_period_hns.store(0, Ordering::Relaxed);
        self.min_aligned_period_hns.store(0, Ordering::Relaxed);
        self.bits_per_sample.store(0, Ordering::Relaxed);
        self.valid_bits_per_sample.store(0, Ordering::Relaxed);
        *lock(&self.sample_type) = None;
        self.period_hns.store(0, Ordering::Relaxed);
        self.buffer_size_frames.store(0, Ordering::Relaxed);
        self.channel_mask.store(0, Ordering::Relaxed);
        self.current_padding.store(0, Ordering::Relaxed);
        self.available_space.store(0, Ordering::Relaxed);
        self.clock_position.store(0, Ordering::Relaxed);
        self.clock_frequency.store(0, Ordering::Relaxed);
        *lock(&self.adapter_name) = None;
    }
}

#[derive(Default)]
struct WasapiSharedState {
    broken: AtomicBool,
    stalled: AtomicBool,
    running: AtomicBool,
    sample_rate: AtomicU32,
    channels: AtomicU32,
    frames_per_callback: AtomicU32,
    device_name: Mutex<Option<String>>,
    default_period_hns: AtomicU32,
    min_period_hns: AtomicU32,
    min_aligned_period_hns: AtomicU32,
    bits_per_sample: AtomicU32,
    valid_bits_per_sample: AtomicU32,
    sample_type: Mutex<Option<String>>,
    period_hns: AtomicU32,
    buffer_size_frames: AtomicU32,
    channel_mask: AtomicU32,
    current_padding: AtomicU32,
    available_space: AtomicU32,
    clock_position: AtomicU64,
    clock_frequency: AtomicU64,
    adapter_name: Mutex<Option<String>>,
}

fn stream_info_from_shared(
    settings: &WasapiSettings,
    shared: &WasapiSharedState,
) -> WasapiStreamInfo {
    let frames = shared.frames_per_callback.load(Ordering::Relaxed);
    WasapiStreamInfo {
        settings: settings.clone(),
        sample_rate: {
            let v = shared.sample_rate.load(Ordering::Relaxed);
            (v > 0).then_some(v)
        },
        channels: {
            let v = shared.channels.load(Ordering::Relaxed);
            (v > 0).then_some(v as u16)
        },
        device_name: lock(&shared.device_name).clone(),
        frames_per_callback: (frames > 0).then_some(frames),
        default_period_hns: {
            let v = shared.default_period_hns.load(Ordering::Relaxed);
            (v > 0).then_some(v)
        },
        min_period_hns: {
            let v = shared.min_period_hns.load(Ordering::Relaxed);
            (v > 0).then_some(v)
        },
        min_aligned_period_hns: {
            let v = shared.min_aligned_period_hns.load(Ordering::Relaxed);
            (v > 0).then_some(v)
        },
        bits_per_sample: {
            let v = shared.bits_per_sample.load(Ordering::Relaxed);
            (v > 0).then_some(v as u16)
        },
        valid_bits_per_sample: {
            let v = shared.valid_bits_per_sample.load(Ordering::Relaxed);
            (v > 0).then_some(v as u16)
        },
        sample_type: lock(&shared.sample_type).clone(),
        period_hns: {
            let v = shared.period_hns.load(Ordering::Relaxed);
            (v > 0).then_some(v)
        },
        buffer_size_frames: {
            let v = shared.buffer_size_frames.load(Ordering::Relaxed);
            (v > 0).then_some(v)
        },
        channel_mask: {
            let v = shared.channel_mask.load(Ordering::Relaxed);
            (v > 0).then_some(v)
        },
        adapter_name: lock(&shared.adapter_name).clone(),
        current_padding: Some(shared.current_padding.load(Ordering::Relaxed)),
        available_space: {
            let v = shared.available_space.load(Ordering::Relaxed);
            (v > 0).then_some(v)
        },
        clock_position: {
            let v = shared.clock_position.load(Ordering::Relaxed);
            (v > 0).then_some(v)
        },
        clock_frequency: {
            let v = shared.clock_frequency.load(Ordering::Relaxed);
            (v > 0).then_some(v)
        },
    }
}

struct WasapiSession {
    audio_client: AudioClient,
    format: WaveFormat,
    conversion: SampleConversion,
    polling: bool,
    h_event: Option<Handle>,
    audio_clock: Option<AudioClock>,
    sample_rate: u32,
    channels: u16,
}

struct CachedSetup {
    format: WaveFormat,
    conversion: SampleConversion,
    mode: StreamMode,
}

fn setup_session(
    settings: &WasapiSettings,
    direction: Direction,
    shared: &WasapiSharedState,
    cache: &mut Option<CachedSetup>,
) -> Result<WasapiSession> {
    let enumerator = DeviceEnumerator::new().context("create device enumerator")?;
    let device = enumerator
        .get_default_device(&direction)
        .with_context(|| format!("get default {direction} device"))?;
    *lock(&shared.device_name) = device.get_friendlyname().ok();
    *lock(&shared.adapter_name) = device.get_interface_friendlyname().ok();

    let mix_format = if settings.sample_rate.is_none() || settings.channels.is_none() {
        let client = device.get_iaudioclient().context("get audio client")?;
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

    let (audio_client, format, conversion, mode) =
        if matches!(settings.share_mode, ShareMode::Exclusive) {
            let mut reused: Option<(AudioClient, WaveFormat, SampleConversion, StreamMode)> = None;
            if let Some(cached) = cache.as_ref() {
                match device.get_iaudioclient() {
                    Ok(mut client) => {
                        apply_audio_client_properties(&client, settings);
                        match client.initialize_client(&cached.format, &direction, &cached.mode) {
                            Ok(()) => {
                                reused = Some((
                                    client,
                                    cached.format.clone(),
                                    cached.conversion,
                                    cached.mode,
                                ));
                            }
                            Err(e) => eprintln!(
                                "wasapi: reusing cached exclusive format failed: {e}, re-probing"
                            ),
                        }
                    }
                    Err(e) => eprintln!(
                        "wasapi: get audio client for cached format failed: {e}, re-probing"
                    ),
                }
            }
            let built = match reused {
                Some(built) => built,
                None => probe_exclusive_format(&device, settings, desired_ch, direction)
                    .context("exclusive format not supported")?,
            };
            *cache = Some(CachedSetup {
                format: built.1.clone(),
                conversion: built.2,
                mode: built.3,
            });
            built
        } else {
            let desired_format =
                WaveFormat::new(32, 32, &SampleType::Float, desired_sr, desired_ch, None);
            let mut client = device.get_iaudioclient().context("get audio client")?;
            let (def_period, _min_period) =
                client.get_device_period().context("get device period")?;
            let buffer_duration_hns = if let Some(bs) = settings.buffer_size {
                calculate_period_100ns(bs as i64, desired_sr as i64)
            } else {
                def_period
            };
            let mode = match settings.timing {
                Timing::Events => StreamMode::EventsShared {
                    autoconvert: true,
                    buffer_duration_hns,
                },
                Timing::Polling => StreamMode::PollingShared {
                    autoconvert: true,
                    buffer_duration_hns,
                },
            };
            apply_audio_client_properties(&client, settings);
            client
                .initialize_client(&desired_format, &direction, &mode)
                .context("initialize audio client")?;
            (client, desired_format, SampleConversion::Float32, mode)
        };

    let sample_rate = format.get_samplespersec();
    let channels = format.get_nchannels();

    if let Ok((def_per, min_per)) = audio_client.get_device_period() {
        shared
            .default_period_hns
            .store(def_per as u32, Ordering::Relaxed);
        shared
            .min_period_hns
            .store(min_per as u32, Ordering::Relaxed);
    }
    shared
        .period_hns
        .store(mode_period_hns(&mode), Ordering::Relaxed);
    if let Ok(aligned_min) = audio_client.calculate_aligned_period_near(0, Some(128), &format) {
        shared
            .min_aligned_period_hns
            .store(aligned_min as u32, Ordering::Relaxed);
    }
    shared
        .bits_per_sample
        .store(format.get_bitspersample() as u32, Ordering::Relaxed);
    shared
        .valid_bits_per_sample
        .store(format.get_validbitspersample() as u32, Ordering::Relaxed);
    shared
        .channel_mask
        .store(format.get_dwchannelmask(), Ordering::Relaxed);
    shared.buffer_size_frames.store(
        audio_client.get_buffer_size().unwrap_or(0),
        Ordering::Relaxed,
    );
    *lock(&shared.sample_type) = match format.get_subformat() {
        Ok(SampleType::Float) => Some("Float".into()),
        Ok(SampleType::Int) => Some("Int".into()),
        Err(_) => None,
    };
    shared.sample_rate.store(sample_rate, Ordering::Relaxed);
    shared.channels.store(channels as u32, Ordering::Relaxed);

    let polling = matches!(
        mode,
        StreamMode::PollingExclusive { .. } | StreamMode::PollingShared { .. }
    );
    let h_event = if polling {
        None
    } else {
        Some(
            audio_client
                .set_get_eventhandle()
                .context("get event handle")?,
        )
    };
    let audio_clock = audio_client.get_audioclock().ok();
    if let Some(ref clock) = audio_clock {
        if let Ok(freq) = clock.get_frequency() {
            shared.clock_frequency.store(freq, Ordering::Relaxed);
        }
    }

    Ok(WasapiSession {
        audio_client,
        format,
        conversion,
        polling,
        h_event,
        audio_clock,
        sample_rate,
        channels,
    })
}

pub struct WasapiBackend {
    settings: WasapiSettings,
    state: Option<Arc<StateCell>>,
    shared: Arc<WasapiSharedState>,
    join_handle: Option<JoinHandle<()>>,
}
impl WasapiBackend {
    pub fn new(settings: WasapiSettings) -> Self {
        Self {
            settings,
            state: None,
            shared: Arc::new(WasapiSharedState::default()),
            join_handle: None,
        }
    }

    fn run_playback(
        settings: WasapiSettings,
        state: Arc<StateCell>,
        shared: Arc<WasapiSharedState>,
    ) -> Result<()> {
        let _com = ComGuard::new()?;
        let _mmcss = MmcssGuard::new();

        let mut setup_cache: Option<CachedSetup> = None;

        if matches!(settings.share_mode, ShareMode::Exclusive)
            && matches!(settings.timing, Timing::Polling)
        {
            let result = Self::run_playback_session(&settings, &state, &shared, &mut setup_cache);
            if result.is_err() {
                shared.broken.store(true, Ordering::Relaxed);
            }
            return result;
        }

        loop {
            if !shared.running.load(Ordering::Relaxed) {
                return Ok(());
            }
            shared.stalled.store(false, Ordering::Relaxed);
            let result = Self::run_playback_session(&settings, &state, &shared, &mut setup_cache);
            let stalled = shared.stalled.load(Ordering::Relaxed);
            match result {
                Ok(()) if !stalled => return Ok(()),
                Err(e) if !stalled => {
                    shared.broken.store(true, Ordering::Relaxed);
                    return Err(e);
                }
                Ok(()) => eprintln!("wasapi playback stalled, rebuilding stream"),
                Err(e) => eprintln!("wasapi playback stalled, rebuilding stream: {e}"),
            }
        }
    }

    fn run_playback_session(
        settings: &WasapiSettings,
        state: &Arc<StateCell>,
        shared: &WasapiSharedState,
        cache: &mut Option<CachedSetup>,
    ) -> Result<()> {
        let WasapiSession {
            audio_client,
            conversion,
            polling,
            h_event,
            audio_clock,
            sample_rate: actual_sr,
            channels: actual_ch,
            ..
        } = setup_session(settings, Direction::Render, shared, cache)?;

        state.get().0.sample_rate = actual_sr;

        let render_client = audio_client
            .get_audiorenderclient()
            .context("get render client")?;

        let buffer_frames_total = audio_client.get_buffer_size().context("get buffer size")?;
        let target_frames = (buffer_frames_total / 2).max(actual_sr / 500);
        let poll_interval = Duration::from_millis(1);
        let _poll_timer = polling.then(PollTimer::new);

        let clock_frequency = audio_clock
            .as_ref()
            .and_then(|clock| clock.get_frequency().ok());
        let mut frames_written: u64 = 0;
        let mut f32_buf = Vec::new();
        let mut byte_buf = Vec::new();
        let mut write_block =
            |frames: u32, available: u32, callback_instant: Instant| -> Result<()> {
                shared.frames_per_callback.store(frames, Ordering::Relaxed);
                shared.available_space.store(available, Ordering::Relaxed);

                let n_samples = frames as usize * actual_ch as usize;
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

                render_client
                    .write_to_device(frames as usize, &byte_buf, None)
                    .map_err(|e| anyhow::anyhow!(e))?;

                frames_written += frames as u64;

                let post_padding = audio_client.get_current_padding().unwrap_or(0);
                shared
                    .current_padding
                    .store(post_padding, Ordering::Relaxed);

                let device_position = audio_clock
                    .as_ref()
                    .and_then(|clock| clock.get_position().ok().map(|(position, _timer)| position));
                if let Some(position) = device_position {
                    shared.clock_position.store(position, Ordering::Relaxed);
                }

                let padding_delay_sec = if post_padding > 0 {
                    post_padding as f64 / actual_sr as f64
                } else {
                    frames as f64 / actual_sr as f64
                };
                let clock_delay_sec = match (device_position, clock_frequency) {
                    (Some(position), Some(frequency)) if frequency > 0 => {
                        let written_sec = frames_written as f64 / actual_sr as f64;
                        let played_sec = position as f64 / frequency as f64;
                        (written_sec - played_sec).max(0.0)
                    }
                    _ => 0.0,
                };
                let stream_delay_sec = clock_delay_sec.max(padding_delay_sec);
                rec.push(stream_delay_sec + callback_instant.elapsed().as_secs_f64());
                Ok(())
            };

        if !polling {
            let prefill = buffer_frames_total;
            if let Err(e) = write_block(prefill, prefill, Instant::now()) {
                shared.broken.store(true, Ordering::Relaxed);
                return Err(e).context("prefill render buffer");
            }
        }

        audio_client.start_stream().context("start stream")?;

        let exclusive = matches!(settings.share_mode, ShareMode::Exclusive);
        let mut stall_tracker = (exclusive && !polling).then(|| (Instant::now(), 0u32));
        let mut loop_result = Ok(());

        loop {
            if !shared.running.load(Ordering::Relaxed) {
                let _ = audio_client.stop_stream();
                break;
            }

            if !polling && h_event.as_ref().unwrap().wait_for_event(1000).is_err() {
                let _ = audio_client.stop_stream();
                shared.broken.store(true, Ordering::Relaxed);
                loop_result = Err(anyhow::anyhow!("event wait timeout"));
                break;
            }

            let callback_instant = Instant::now();

            let available = match audio_client.get_available_space_in_frames() {
                Ok(f) => f,
                Err(e) => {
                    let _ = audio_client.stop_stream();
                    shared.broken.store(true, Ordering::Relaxed);
                    loop_result = Err(anyhow::anyhow!(e));
                    break;
                }
            };

            let buffer_frames = if polling {
                let padding = buffer_frames_total.saturating_sub(available);
                if available == 0 || padding >= target_frames {
                    std::thread::sleep(poll_interval);
                    continue;
                }
                (target_frames - padding).min(available)
            } else {
                if available == 0 {
                    continue;
                }

                if let Some((last_callback_instant, interval_strikes)) = stall_tracker.as_mut() {
                    let interval_secs = callback_instant
                        .duration_since(*last_callback_instant)
                        .as_secs_f64();
                    *last_callback_instant = callback_instant;
                    let expected_interval = available as f64 / actual_sr as f64;
                    if interval_secs > expected_interval * 1.5 {
                        *interval_strikes += 1;
                    } else {
                        *interval_strikes = 0;
                    }
                    if *interval_strikes >= 3 {
                        let _ = audio_client.stop_stream();
                        shared.stalled.store(true, Ordering::Relaxed);
                        eprintln!(
                            "wasapi playback stalled: callback interval {:.2}ms vs expected {:.2}ms",
                            interval_secs * 1000.0,
                            expected_interval * 1000.0
                        );
                        break;
                    }
                }

                available
            };

            if let Err(e) = write_block(buffer_frames, available, callback_instant) {
                let _ = audio_client.stop_stream();
                shared.broken.store(true, Ordering::Relaxed);
                loop_result = Err(e);
                break;
            }

            if polling {
                std::thread::sleep(poll_interval);
            }
        }

        loop_result
    }
}

impl Backend for WasapiBackend {
    fn setup(&mut self, setup: BackendSetup) {
        self.state = Some(Arc::new(setup.into()));
    }

    fn start(&mut self) -> Result<()> {
        let state = Arc::clone(self.state.as_ref().context("not set up")?);
        self.close()?;

        let settings = self.settings.clone();
        let shared = Arc::clone(&self.shared);

        shared.reset();
        shared.running.store(true, Ordering::Relaxed);

        let thread_shared = Arc::clone(&shared);
        let spawn_result = std::thread::Builder::new()
            .name("wasapi-playback".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    WasapiBackend::run_playback(settings, state, Arc::clone(&thread_shared))
                }));
                if !matches!(result, Ok(Ok(()))) {
                    thread_shared.broken.store(true, Ordering::Relaxed);
                }
            });

        let join_handle = match spawn_result {
            Ok(handle) => handle,
            Err(e) => {
                shared.running.store(false, Ordering::Relaxed);
                return Err(e).context("spawn playback thread");
            }
        };

        self.join_handle = Some(join_handle);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.shared.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
        Ok(())
    }

    fn consume_broken(&self) -> bool {
        self.shared.broken.fetch_and(false, Ordering::Relaxed)
    }

    fn stream_info(&mut self) -> BackendStreamInfo {
        BackendStreamInfo::Wasapi(stream_info_from_shared(&self.settings, &self.shared))
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
    shared: Arc<WasapiSharedState>,
    join_handle: Option<JoinHandle<()>>,
}

impl WasapiRecorderBackend {
    pub fn new(settings: WasapiSettings) -> Self {
        Self {
            settings,
            state: None,
            shared: Arc::new(WasapiSharedState::default()),
            join_handle: None,
        }
    }

    fn run_capture(
        settings: WasapiSettings,
        state: Arc<RecorderStateCell>,
        shared: Arc<WasapiSharedState>,
    ) -> Result<()> {
        let _com = ComGuard::new()?;
        let _mmcss = MmcssGuard::new();

        let mut setup_cache = None;

        let WasapiSession {
            audio_client,
            format,
            conversion,
            polling,
            h_event,
            audio_clock,
            sample_rate: actual_sr,
            channels: actual_ch,
        } = setup_session(&settings, Direction::Capture, &shared, &mut setup_cache)?;

        state.get().0.sample_rate = actual_sr;

        let capture_client = audio_client
            .get_audiocaptureclient()
            .context("get capture client")?;

        audio_client.start_stream().context("start stream")?;

        let poll_interval = Duration::from_millis(1);
        let _poll_timer = polling.then(PollTimer::new);

        let buffer_size = audio_client.get_buffer_size().context("get buffer size")? as usize;
        let bytes_per_frame = format.get_blockalign() as usize;
        let mut byte_buf: Vec<u8> = vec![0u8; bytes_per_frame * (buffer_size + 1024)];
        let mut f32_buf: Vec<f32> = Vec::new();
        let mut loop_result = Ok(());

        loop {
            if !shared.running.load(Ordering::Relaxed) {
                let _ = audio_client.stop_stream();
                break;
            }

            let callback_instant = Instant::now();

            let (nbr_frames, info) = match capture_client.read_from_device(&mut byte_buf) {
                Ok(v) => v,
                Err(e) => {
                    let _ = audio_client.stop_stream();
                    shared.broken.store(true, Ordering::Relaxed);
                    loop_result = Err(anyhow::anyhow!(e));
                    break;
                }
            };

            if nbr_frames == 0 {
                if polling {
                    std::thread::sleep(poll_interval);
                    continue;
                }
                if h_event.as_ref().unwrap().wait_for_event(1000).is_err() {
                    let _ = audio_client.stop_stream();
                    shared.broken.store(true, Ordering::Relaxed);
                    loop_result = Err(anyhow::anyhow!("event wait timeout"));
                    break;
                }
                continue;
            }

            shared
                .frames_per_callback
                .store(nbr_frames, Ordering::Relaxed);

            let n_samples = nbr_frames as usize * actual_ch as usize;
            f32_buf.resize(n_samples, 0f32);
            if info.flags.silent {
                f32_buf.fill(0.0);
            } else {
                conversion.bytes_to_f32(&byte_buf, &mut f32_buf);
            }

            let (mixer, rec) = state.get();
            if actual_ch == 1 {
                mixer.record_mono(&f32_buf);
            } else {
                mixer.record_stereo(&f32_buf);
            }

            let post_padding = audio_client.get_current_padding().unwrap_or(0);
            shared
                .current_padding
                .store(post_padding, Ordering::Relaxed);
            shared.available_space.store(nbr_frames, Ordering::Relaxed);

            let mut clock_position = None;
            let mut qpc_now = None;
            if let Some(ref clock) = audio_clock {
                if let Ok((position, timer)) = clock.get_position() {
                    clock_position = Some(position);
                    qpc_now = Some(timer);
                }
            }
            if let Some(position) = clock_position {
                shared.clock_position.store(position, Ordering::Relaxed);
            }

            let padding_delay_sec = if post_padding > 0 {
                post_padding as f64 / actual_sr as f64
            } else {
                nbr_frames as f64 / actual_sr as f64
            };
            let stream_delay_sec = match qpc_now {
                Some(now)
                    if !info.flags.timestamp_error
                        && info.timestamp > 0
                        && now >= info.timestamp =>
                {
                    ((now - info.timestamp) as f64 / 10_000_000.0).max(padding_delay_sec)
                }
                _ => padding_delay_sec + callback_instant.elapsed().as_secs_f64(),
            };
            rec.push(stream_delay_sec);

            if polling {
                std::thread::sleep(poll_interval);
            } else if h_event.as_ref().unwrap().wait_for_event(1000).is_err() {
                let _ = audio_client.stop_stream();
                shared.broken.store(true, Ordering::Relaxed);
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
        let state = Arc::clone(self.state.as_ref().context("not set up")?);
        self.close()?;

        let settings = self.settings.clone();
        let shared = Arc::clone(&self.shared);

        shared.reset();
        shared.running.store(true, Ordering::Relaxed);

        let thread_shared = Arc::clone(&shared);
        let spawn_result = std::thread::Builder::new()
            .name("wasapi-capture".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    WasapiRecorderBackend::run_capture(settings, state, Arc::clone(&thread_shared))
                }));
                if !matches!(result, Ok(Ok(()))) {
                    thread_shared.broken.store(true, Ordering::Relaxed);
                }
            });

        let join_handle = match spawn_result {
            Ok(handle) => handle,
            Err(e) => {
                shared.running.store(false, Ordering::Relaxed);
                return Err(e).context("spawn capture thread");
            }
        };

        self.join_handle = Some(join_handle);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.shared.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
        Ok(())
    }

    fn consume_broken(&self) -> bool {
        self.shared.broken.fetch_and(false, Ordering::Relaxed)
    }

    fn stream_info(&mut self) -> BackendStreamInfo {
        BackendStreamInfo::Wasapi(stream_info_from_shared(&self.settings, &self.shared))
    }
}

impl Drop for WasapiRecorderBackend {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
