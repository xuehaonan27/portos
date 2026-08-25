//! The health metric of the whole architecture.
//! 
//! context bytes / data-plane bytes. Context bytes are whatever crosses into
//! the model-facing control plane (handles, metadata, previews); data-plane
//! bytes move as chunked streams and never count against context.
//! 
//! ## Documentation
//! ### architecture-v0.md §4.5

#[derive(Debug, Default)]
pub struct ContextMeter {
    pub context_bytes: u64,
    pub data_bytes: u64,
}

impl ContextMeter {
    /// Record bytes that entered the model-facing control plane.
    pub fn count_context(&mut self, n: u64) {
        self.context_bytes += n;
    }

    /// Record payload bytes that moved through the data plane.
    pub fn count_data(&mut self, n: u64) {
        self.data_bytes += n;
    }

    /// context/data ratio; the whole point is keeping this tiny.
    pub fn ratio(&self) -> f64 {
        if self.data_bytes == 0 {
            return if self.context_bytes == 0 { 0.0 } else { f64::INFINITY };
        }
        self.context_bytes as f64 / self.data_bytes as f64
    }
}
