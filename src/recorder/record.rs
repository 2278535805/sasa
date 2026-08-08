use crate::Recorder;
use ringbuf::{HeapConsumer, HeapProducer, HeapRb};
use std::sync::{Arc, Weak};

pub struct Record {
    _arc: Arc<()>,
    cons: HeapConsumer<f32>,
}

impl Record {
    pub const DEFAULT_BUFFER_SIZE: usize = 4096;

    pub(crate) fn new(buffer_size: Option<usize>) -> (Record, RecordRecorder) {
    let (prod, cons) = HeapRb::new(buffer_size.unwrap_or(Record::DEFAULT_BUFFER_SIZE)).split();
    let arc = Arc::new(());
    let recorder = RecordRecorder {
        arc: Arc::downgrade(&arc),
        prod,
    };
    (Record { _arc: arc, cons }, recorder)
}

    pub fn available(&self) -> usize {
        self.cons.len()
    }

    pub fn read(&mut self, data: &mut [f32]) -> usize {
        let n = data.len().min(self.cons.len());
        if n == 0 {
            return 0;
        }
        for i in 0..n {
            data[i] = self.cons.pop().unwrap();
        }
        n
    }
}

pub struct RecordRecorder {
    arc: Weak<()>,
    prod: HeapProducer<f32>,
}

impl Recorder for RecordRecorder {
    fn alive(&self) -> bool {
        self.arc.strong_count() != 0
    }

    fn record_mono(&mut self, _sample_rate: u32, data: &[f32]) {
        for &sample in data {
            let _ = self.prod.push(sample);
        }
    }

    fn record_stereo(&mut self, _sample_rate: u32, data: &[f32]) {
        for &sample in data {
            let _ = self.prod.push(sample);
        }
    }
}

