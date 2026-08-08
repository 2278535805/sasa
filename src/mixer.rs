use ringbuf::HeapConsumer;
use crate::{Renderer, Recorder};

pub(crate) enum RenderMixerCommand {
    AddRenderer(Box<dyn Renderer>),
}
pub(crate) struct Mixer {
    pub(crate) sample_rate: u32,

    renderers: Vec<Box<dyn Renderer>>,
    cons: HeapConsumer<RenderMixerCommand>,
}

impl Mixer {
    pub(crate) fn new(sample_rate: u32, cons: HeapConsumer<RenderMixerCommand>) -> Self {
        Self {
            sample_rate,

            renderers: Vec::new(),
            cons,
        }
    }

    fn consume_commands(&mut self) {
        for cmd in self.cons.pop_iter() {
            match cmd {
                RenderMixerCommand::AddRenderer(renderer) => self.renderers.push(renderer),
            }
        }
    }

    pub fn render_mono(&mut self, data: &mut [f32]) {
        self.consume_commands();
        data.fill(0.);

        self.renderers.retain_mut(|renderer| {
            renderer.render_mono(self.sample_rate, data);
            renderer.alive()
        });
    }

    pub fn render_stereo(&mut self, data: &mut [f32]) {
        self.consume_commands();
        data.fill(0.);

        self.renderers.retain_mut(|renderer| {
            renderer.render_stereo(self.sample_rate, data);
            renderer.alive()
        });
    }
}

pub(crate) enum RecorderMixerCommand {
    AddRecorder(Box<dyn Recorder>),
}

pub(crate) struct RecorderMixer {
    pub(crate) sample_rate: u32,

    recorders: Vec<Box<dyn Recorder>>,
    cons: HeapConsumer<RecorderMixerCommand>,
}

impl RecorderMixer {
    pub(crate) fn new(sample_rate: u32, cons: HeapConsumer<RecorderMixerCommand>) -> Self {
        Self {
            sample_rate,

            recorders: Vec::new(),
            cons,
        }
    }

    fn consume_commands(&mut self) {
        for cmd in self.cons.pop_iter() {
            match cmd {
                RecorderMixerCommand::AddRecorder(recorder) => self.recorders.push(recorder),
            }
        }
    }

    pub fn record_mono(&mut self, data: &[f32]) {
        self.consume_commands();

        self.recorders.retain_mut(|recorder| {
            recorder.record_mono(self.sample_rate, data);
            recorder.alive()
        });
    }

    pub fn record_stereo(&mut self, data: &[f32]) {
        self.consume_commands();

        self.recorders.retain_mut(|recorder| {
            recorder.record_stereo(self.sample_rate, data);
            recorder.alive()
        });
    }
}
