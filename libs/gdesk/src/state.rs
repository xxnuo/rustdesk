use crate::math::WaveletCoefficients;

#[derive(Debug, Clone)]
pub struct TileState {
    pub last_hash: u64,
    pub change_heat: u8,
    pub still_frames: u8,
    pub sent_shift_y: u8,
    pub sent_shift_c: u8,
    pub coefficients: Option<WaveletCoefficients>,
    pub initialized: bool,
    pub last_rgb: Vec<u8>,
}
impl Default for TileState {
    fn default() -> Self {
        Self {
            last_hash: 0,
            change_heat: 0,
            still_frames: 0,
            sent_shift_y: 255,
            sent_shift_c: 255,
            coefficients: None,
            initialized: false,
            last_rgb: Vec::new(),
        }
    }
}
impl TileState {
    pub fn is_lossless(&self) -> bool {
        self.initialized && self.sent_shift_y == 0 && self.sent_shift_c == 0
    }
    pub fn needs_refinement(&self) -> bool {
        self.initialized && !self.is_lossless() && self.coefficients.is_some()
    }
}
#[derive(Debug, Clone)]
pub struct TileStateTable {
    pub tiles: Vec<TileState>,
}
impl TileStateTable {
    pub fn new(count: usize) -> Self {
        Self {
            tiles: vec![TileState::default(); count],
        }
    }
    pub fn reset(&mut self, count: usize) {
        self.tiles = vec![TileState::default(); count];
    }
}
