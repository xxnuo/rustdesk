pub mod cache;
pub mod decoder;
pub mod encoder;
pub mod gpu;
pub mod math;
pub mod protocol;
pub mod state;

pub use decoder::GDeskDecoder;
pub use encoder::{GDeskConfig, GDeskEncoder};
pub use math::PixelFormat;
pub use protocol::{is_keyframe_packet, GDeskAck, GDeskFrameHeader, GDeskStats, QoSState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub &'static str);
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
