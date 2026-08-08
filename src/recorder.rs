mod record;
pub use record::Record;

pub trait Recorder: Send + Sync {
    fn alive(&self) -> bool;
    fn record_mono(&mut self, sample_rate: u32, data: &[f32]);
    fn record_stereo(&mut self, sample_rate: u32, data: &[f32]);
}
