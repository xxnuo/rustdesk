pub mod cache;
pub mod decoder;
pub mod encoder;
#[cfg(feature = "gpu")]
pub mod gpu;
pub mod input;
pub mod math;
pub mod protocol;
pub mod qos;
pub mod sched;
pub mod state;
pub mod wake;

pub use decoder::GDeskDecoder;
pub use encoder::{GDeskConfig, GDeskEncoder};
pub use math::PixelFormat;
pub use protocol::{is_keyframe_packet, GDeskAck, GDeskFrameHeader, GDeskStats, QoSState};
pub use qos::GDeskQos;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub &'static str);
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

pub fn monotonic_us() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_micros()
        .min(u64::MAX as u128) as u64
}
