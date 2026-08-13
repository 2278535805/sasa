/// Simple And Stupid Audio for Rust, optimized for low latency.
pub mod backend;
use atomic_float::AtomicF64;
pub use backend::{Backend, BackendStreamInfo, RecorderBackend};

mod clip;
pub use clip::AudioClip;

mod mixer;

mod renderer;
pub use renderer::{Music, MusicClock, MusicParams, PlaySfxParams, Renderer, Sfx};

pub mod recorder;
pub use recorder::{Recorder, Record};

use crate::{
    backend::{BackendSetup, RecorderBackendSetup},
    mixer::RecorderMixerCommand,
    mixer::RenderMixerCommand,
};
use anyhow::{anyhow, Context, Result};
use ringbuf::{HeapProducer, HeapRb};
use std::{
    ops::{Add, Mul},
    sync::{
        atomic::Ordering,
        Arc,
    },
};

fn buffer_is_full<E>(_: E) -> anyhow::Error {
    anyhow!("buffer is full")
}

#[derive(Clone, Copy, Default)]
pub struct Frame(pub f32, pub f32);
impl Frame {
    pub fn avg(&self) -> f32 {
        (self.0 + self.1) / 2.
    }

    pub fn interpolate(&self, other: &Self, f: f32) -> Self {
        Self(
            self.0 + (other.0 - self.0) * f,
            self.1 + (other.1 - self.1) * f,
        )
    }
}
impl Add for Frame {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0, self.1 + rhs.1)
    }
}
impl Mul<f32> for Frame {
    type Output = Self;

    fn mul(self, rhs: f32) -> Self::Output {
        Self(self.0 * rhs, self.1 * rhs)
    }
}

const LATENCY_RECORD_NUM: usize = 640;

pub struct LatencyRecorder {
    records: [f64; LATENCY_RECORD_NUM],
    head: usize,
    sum: f64,
    full: bool,
    result: Arc<AtomicF64>,
}

impl LatencyRecorder {
    pub fn new(result: Arc<AtomicF64>) -> Self {
        Self {
            records: [0.; LATENCY_RECORD_NUM],
            head: 0,
            sum: 0.,
            full: false,
            result,
        }
    }

    pub fn push(&mut self, record: f64) {
        let place = &mut self.records[self.head];
        self.sum += record - *place;
        *place = record;
        self.head += 1;
        if self.head == LATENCY_RECORD_NUM {
            self.full = true;
            self.head = 0;
        }
        self.result.store(
            self.sum
                / (if self.full {
                    LATENCY_RECORD_NUM
                } else {
                    self.head.max(1)
                }) as f64,
            Ordering::SeqCst,
        );
    }
}

pub struct AudioManager {
    backend: Box<dyn Backend>,
    latency: Arc<AtomicF64>,
    prod: HeapProducer<RenderMixerCommand>,
}

impl AudioManager {
    pub fn new(backend: impl Backend + 'static) -> Result<Self> {
        Self::new_box(Box::new(backend))
    }

    pub fn new_box(mut backend: Box<dyn Backend>) -> Result<Self> {
        let (prod, cons) = HeapRb::new(16).split();
        let latency: Arc<AtomicF64> = Arc::default();
        let latency_rec = LatencyRecorder::new(Arc::clone(&latency));
        backend.setup(BackendSetup {
            mixer_cons: cons,
            latency_rec,
        })?;
        Ok(Self {
            backend,
            latency,
            prod,
        })
    }

    pub fn create_sfx(&mut self, clip: AudioClip, buffer_size: Option<usize>) -> Result<Sfx> {
        let (sfx, sfx_renderer) = Sfx::new(clip, buffer_size);
        self.prod
            .push(RenderMixerCommand::AddSfxRenderer(Box::new(sfx_renderer)))
            .map_err(buffer_is_full)
            .context("add sfx renderer")?;
        Ok(sfx)
    }

    pub fn create_music(&mut self, clip: AudioClip, settings: MusicParams) -> Result<Music> {
        let (music, music_renderer) = Music::new(clip, settings);
        self.prod
            .push(RenderMixerCommand::AddMusicRenderer(Box::new(music_renderer)))
            .map_err(buffer_is_full)
            .context("add music renderer")?;
        Ok(music)
    }

    pub fn add_renderer(&mut self, renderer: impl Renderer + 'static) -> Result<()> {
        self.prod
            .push(RenderMixerCommand::AddRenderer(Box::new(renderer)))
            .map_err(buffer_is_full)
            .context("add renderer")?;
        Ok(())
    }

    pub fn estimate_latency(&self) -> f64 {
        self.latency.load(Ordering::SeqCst)
    }

    pub fn stream_info(&mut self) -> BackendStreamInfo {
        self.backend.stream_info()
    }

    #[inline(always)]
    pub fn consume_broken(&self) -> bool {
        self.backend.consume_broken()
    }

    #[inline(always)]
    pub fn start(&mut self) -> Result<()> {
        self.backend.start()
    }

    pub fn close(&mut self) -> Result<()> {
        self.backend.close()
    }

    pub fn recover_if_needed(&mut self) -> Result<()> {
        if self.consume_broken() {
            self.start()
        } else {
            Ok(())
        }
    }
}

pub struct AudioRecorder {
    backend: Box<dyn RecorderBackend>,
    latency: Arc<AtomicF64>,
    prod: HeapProducer<RecorderMixerCommand>,
}

impl AudioRecorder {
    pub fn new(backend: impl RecorderBackend + 'static) -> Result<Self> {
        Self::new_box(Box::new(backend))
    }

    pub fn new_box(mut backend: Box<dyn RecorderBackend>) -> Result<Self> {
        let (prod, cons) = HeapRb::new(16).split();
        let latency: Arc<AtomicF64> = Arc::default();
        let latency_rec = LatencyRecorder::new(Arc::clone(&latency));
        backend.setup(RecorderBackendSetup {
            mixer_cons: cons,
            latency_rec,
        })?;
        backend.start()?;
        Ok(Self {
            backend,
            latency,
            prod,
        })
    }

    pub fn create(&mut self, buffer_size: Option<usize>) -> Result<Record> {
        let (record, record_recorder) = Record::new(buffer_size);
        self.add_recorder(record_recorder)?;
        Ok(record)
    }

    pub fn add_recorder(&mut self, recorder: impl Recorder + 'static) -> Result<()> {
        self.prod
            .push(RecorderMixerCommand::AddRecorder(Box::new(recorder)))
            .map_err(buffer_is_full)
            .context("add recorder")?;
        Ok(())
    }

    pub fn estimate_latency(&self) -> f64 {
        self.latency.load(Ordering::SeqCst)
    }

    pub fn stream_info(&mut self) -> BackendStreamInfo {
        self.backend.stream_info()
    }

    #[inline(always)]
    pub fn consume_broken(&self) -> bool {
        self.backend.consume_broken()
    }

    #[inline(always)]
    pub fn start(&mut self) -> Result<()> {
        self.backend.start()
    }

    pub fn close(&mut self) -> Result<()> {
        self.backend.close()
    }

    pub fn recover_if_needed(&mut self) -> Result<()> {
        if self.consume_broken() {
            self.start()
        } else {
            Ok(())
        }
    }
}
