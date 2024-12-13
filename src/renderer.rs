mod music;
pub use music::{Music, MusicParams};

mod sfx;
pub use sfx::{Sfx, PlaySfxParams};

pub trait Renderer: Send + Sync {
    fn alive(&self) -> bool;
    fn render_mono(&mut self, sample_rate: u32, data: &mut [i16]);
    fn render_stereo(&mut self, sample_rate: u32, data: &mut [i16]);
}
