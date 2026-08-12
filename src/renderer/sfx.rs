use crate::{buffer_is_full, AudioClip, MusicClock, Renderer};
use anyhow::{anyhow, Context, Result};
use ringbuf::{HeapConsumer, HeapProducer, HeapRb};
use std::collections::VecDeque;
use std::sync::{Arc, Weak};

#[derive(Debug, Clone)]
pub struct PlaySfxParams {
    pub amplifier: f32,
}
impl Default for PlaySfxParams {
    fn default() -> Self {
        Self { amplifier: 1. }
    }
}

enum SfxCommand {
    Play(PlaySfxParams),
    Schedule(Vec<(f64, PlaySfxParams)>),
    SetClock(MusicClock),
}

pub(crate) struct SfxRenderer {
    clip: AudioClip,
    arc: Weak<()>,
    cmd_cons: HeapConsumer<SfxCommand>,
    active_prod: HeapProducer<(f64, PlaySfxParams)>,
    active_cons: HeapConsumer<(f64, PlaySfxParams)>,
    clock: Option<MusicClock>,
    scheduled: VecDeque<(f64, PlaySfxParams)>,
    buffer: Vec<f32>,
}

impl SfxRenderer {
    fn prepare(&mut self, buffer_time: f64) {
        for cmd in self.cmd_cons.pop_iter() {
            match cmd {
                SfxCommand::Play(params) => {
                    let _ = self.active_prod.push((0., params));
                }
                SfxCommand::Schedule(times) => {
                    self.scheduled = VecDeque::from(times);
                }
                SfxCommand::SetClock(clock) => self.clock = Some(clock),
            }
        }
        if let Some(clock) = &self.clock {
            let now = clock.load();
            while let Some(&(time, _)) = self.scheduled.front() {
                if time < now - buffer_time {
                    let _ = self.scheduled.pop_front();
                    continue;
                }
                if time > now {
                    break;
                }
                if let Some((time, params)) = self.scheduled.pop_front() {
                    let delay = (time - now + buffer_time).max(0.);
                    let _ = self.active_prod.push((-delay, params));
                }
            }
        }
    }
}

impl Renderer for SfxRenderer {
    fn alive(&self) -> bool {
        !self.cmd_cons.is_empty()
            || !self.active_cons.is_empty()
            || !self.scheduled.is_empty()
            || self.arc.strong_count() != 0
    }

    fn render_mono(&mut self, sample_rate: u32, data: &mut [f32]) {
        let delta = 1. / sample_rate as f64;
        self.prepare(data.len() as f64 * delta);
        let mut pop_count = 0;
        self.buffer.resize(data.len(), 0.0);
        for (position, params) in self.active_cons.iter_mut() {
            for sample in self.buffer.iter_mut() {
                if *position < 0. {
                    *position += delta;
                    continue;
                }
                if let Some(frame) = self.clip.sample(*position) {
                    *sample += frame.avg() * params.amplifier;
                } else {
                    pop_count += 1;
                    break;
                }
                *position += delta;
            }
        }
        for (data_sample, buffer_sample) in data.iter_mut().zip(self.buffer.iter_mut()) {
            *data_sample += std::mem::take(buffer_sample).clamp(-1.0, 1.0);
        }
        unsafe {
            self.active_cons.advance(pop_count);
        }
    }

    fn render_stereo(&mut self, sample_rate: u32, data: &mut [f32]) {
        let delta = 1. / sample_rate as f64;
        self.prepare(data.len() as f64 / 2. * delta);
        let mut pop_count = 0;
        self.buffer.resize(data.len(), 0.0);
        for (position, params) in self.active_cons.iter_mut() {
            for sample in self.buffer.chunks_exact_mut(2) {
                if *position < 0. {
                    *position += delta;
                    continue;
                }
                if let Some(frame) = self.clip.sample(*position) {
                    sample[0] += frame.0 * params.amplifier;
                    sample[1] += frame.1 * params.amplifier;
                } else {
                    pop_count += 1;
                    break;
                }
                *position += delta;
            }
        }
        for (data_sample, buffer_sample) in data.iter_mut().zip(self.buffer.iter_mut()) {
            *data_sample += std::mem::take(buffer_sample).clamp(-1.0, 1.0);
        }
        unsafe {
            self.active_cons.advance(pop_count);
        }
    }
}

pub struct Sfx {
    _arc: Arc<()>,
    prod: HeapProducer<SfxCommand>,
    clock: Option<MusicClock>,
}
impl Sfx {
    pub const DEFAULT_BUFFER_SIZE: usize = 64;

    pub(crate) fn new(clip: AudioClip, buffer_size: Option<usize>) -> (Sfx, SfxRenderer) {
        let size = buffer_size.unwrap_or(Self::DEFAULT_BUFFER_SIZE);
        let (prod, cmd_cons) = HeapRb::new(size).split();
        let (active_prod, active_cons) = HeapRb::new(size).split();
        let arc = Arc::new(());
        let renderer = SfxRenderer {
            clip,
            arc: Arc::downgrade(&arc),
            cmd_cons,
            active_prod,
            active_cons,
            clock: None,
            scheduled: VecDeque::new(),
            buffer: Vec::new(),
        };
        (
            Self {
                _arc: arc,
                prod,
                clock: None,
            },
            renderer,
        )
    }

    pub fn play(&mut self, params: PlaySfxParams) -> Result<()> {
        self.prod
            .push(SfxCommand::Play(params))
            .map_err(buffer_is_full)
            .context("play sfx")
    }

    pub fn set_clock(&mut self, clock: MusicClock) -> Result<()> {
        self.clock = Some(clock.clone());
        self.prod
            .push(SfxCommand::SetClock(clock))
            .map_err(buffer_is_full)
            .context("set clock")
    }

    /// Schedule a list of playback times (in seconds, relative to the bound clock).
    ///
    /// The `times` slice MUST be sorted in ascending order, since the pending
    /// queue is drained from the front. Calling this replaces any previously
    /// scheduled times. Timestamps earlier than the current clock position are
    /// discarded instead of being played.
    pub fn schedule_play(&mut self, times: &[f64], params: PlaySfxParams) -> Result<()> {
        if self.clock.is_none() {
            return Err(anyhow!("bind a clock via set_clock before scheduling"));
        }
        let items = times.iter().map(|&time| (time, params.clone())).collect();
        self.prod
            .push(SfxCommand::Schedule(items))
            .map_err(buffer_is_full)
            .context("schedule sfx")
    }
}
