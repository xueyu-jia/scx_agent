use std::path::PathBuf;

use crate::activation::ActivationEvent;

pub struct EbpfRingbufSource {
    enabled: bool,
}

impl EbpfRingbufSource {
    pub fn new(ringbuf_pin: Option<PathBuf>) -> Self {
        Self {
            enabled: ringbuf_pin.is_some(),
        }
    }

    pub fn poll(&mut self) -> Vec<ActivationEvent> {
        if self.enabled {
            // Placeholder for a libbpf/aya-backed ringbuf reader.
            // The source is wired into Activation now; binding a real ringbuf
            // requires the BPF object/map contract.
        }
        Vec::new()
    }
}
