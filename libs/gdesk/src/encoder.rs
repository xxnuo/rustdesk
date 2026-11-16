use crate::{
    cache::DeterministicLruCache,
    math::{self, PixelFormat},
    protocol::{
        self, GDeskFrameHeader, GDeskStats, QoSState, TileRecord, KEYFRAME_RESET, NEW_OS_FRAME,
    },
    state::TileStateTable,
    Error, Result,
};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct GDeskConfig {
    pub qos_state: QoSState,
    pub initial_shifts: Option<(u8, u8)>,
    pub refine_min_still_frames: u8,
    pub max_refine_tiles_per_frame: usize,
    pub require_confirmed_cache_slots: bool,
}
impl Default for GDeskConfig {
    fn default() -> Self {
        Self {
            qos_state: QoSState::Green,
            initial_shifts: None,
            refine_min_still_frames: 2,
            max_refine_tiles_per_frame: protocol::MAX_TILES,
            require_confirmed_cache_slots: false,
        }
    }
}
impl GDeskConfig {
    fn validate(&self) -> Result<()> {
        if self.initial_shifts.is_some_and(|(y, c)| y > 15 || c > 15)
            || self.max_refine_tiles_per_frame == 0
        {
            return Err(Error("invalid encoder configuration"));
        }
        Ok(())
    }
}

pub struct GDeskEncoder {
    pub config: GDeskConfig,
    pub states: TileStateTable,
    pub cache: DeterministicLruCache,
    pub stats: GDeskStats,
    width: u16,
    height: u16,
    canvas_epoch: u16,
    frame_seq: u32,
    reset_pending: bool,
    last_capture_ts_us: u64,
}
impl GDeskEncoder {
    pub fn new(width: u16, height: u16, config: GDeskConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            states: TileStateTable::new(protocol::tile_count(width, height)?),
            cache: DeterministicLruCache::default(),
            stats: GDeskStats::default(),
            width,
            height,
            canvas_epoch: 0,
            frame_seq: 0,
            reset_pending: true,
            last_capture_ts_us: 0,
        })
    }
    pub fn dimensions(&self) -> (u16, u16) {
        (self.width, self.height)
    }
    pub fn reset(&mut self, width: u16, height: u16) -> Result<()> {
        let count = protocol::tile_count(width, height)?;
        self.width = width;
        self.height = height;
        self.canvas_epoch = self.canvas_epoch.wrapping_add(1);
        self.states.reset(count);
        self.cache.clear();
        self.reset_pending = true;
        Ok(())
    }
    pub fn set_qos_state(&mut self, state: QoSState) {
        self.config.qos_state = state;
        self.stats.qos_state = state;
    }
    pub fn has_pending_refinement(&self) -> bool {
        self.states.tiles.iter().any(|s| s.needs_refinement())
    }
    pub fn encode_frame(
        &mut self,
        pixels: &[u8],
        stride: usize,
        format: PixelFormat,
        capture_ts_us: u64,
    ) -> Result<Option<Vec<u8>>> {
        self.config.validate()?;
        math::validate_image(
            pixels,
            self.width as usize,
            self.height as usize,
            stride,
            format,
        )?;
        let start = Instant::now();
        self.last_capture_ts_us = capture_ts_us;
        let mut records = Vec::new();
        let mut refined = 0;
        for index in 0..self.states.tiles.len() {
            let tile = math::extract_tile(
                pixels,
                self.width as usize,
                self.height as usize,
                stride,
                format,
                index,
            )?;
            let hash = math::tile_hash(&tile);
            let state = &mut self.states.tiles[index];
            if state.initialized && state.last_hash == hash && state.last_rgb == tile {
                state.change_heat = state.change_heat.saturating_sub(1);
                state.still_frames = state.still_frames.saturating_add(1);
                if self.config.qos_state == QoSState::Green
                    && state.needs_refinement()
                    && state.still_frames >= self.config.refine_min_still_frames
                    && refined < self.config.max_refine_tiles_per_frame
                {
                    records.push(self.refine_tile(index)?);
                    refined += 1;
                }
                continue;
            }
            let old_heat = state.change_heat;
            state.last_rgb = tile.clone();
            state.last_hash = hash;
            state.change_heat = state.change_heat.saturating_add(4);
            state.still_frames = 0;
            state.initialized = true;
            state.coefficients = None;
            state.sent_shift_y = 0;
            state.sent_shift_c = 0;
            let tile_index = index as u16;
            if let Some(slot_id) =
                self.cache
                    .find(hash, &tile, self.config.require_confirmed_cache_slots)
            {
                self.cache.touch(slot_id)?;
                records.push(TileRecord::CacheHit {
                    tile_index,
                    slot_id,
                });
                self.stats.cache_hits += 1;
            } else if let Some(color) = math::detect_solid(&tile) {
                records.push(TileRecord::SolidFill { tile_index, color });
            } else if let Some((colors, mask)) = math::detect_two_color(&tile) {
                self.cache.insert(&tile)?;
                records.push(TileRecord::TwoColorText {
                    tile_index,
                    colors,
                    mask,
                });
            } else {
                let (shift_y, shift_c) =
                    self.config
                        .initial_shifts
                        .unwrap_or(match self.config.qos_state {
                            QoSState::Red => (4, 6),
                            QoSState::Yellow => (2, 4),
                            QoSState::Green if old_heat == 0 => (0, 0),
                            QoSState::Green if old_heat < 16 => (2, 4),
                            QoSState::Green => (4, 6),
                        });
                let (shift_y, shift_c) = match self.config.qos_state {
                    QoSState::Green => (shift_y, shift_c),
                    QoSState::Yellow => (shift_y.max(2), shift_c.max(4)),
                    QoSState::Red => (shift_y.max(4), shift_c.max(6)),
                };
                let coeff = math::rgb_to_wavelet(&tile)?;
                let payload = math::encode_wavelet_init(&coeff, shift_y, shift_c)?;
                state.sent_shift_y = shift_y;
                state.sent_shift_c = shift_c;
                if shift_y == 0 && shift_c == 0 {
                    self.cache.insert(&tile)?;
                } else {
                    state.coefficients = Some(coeff);
                }
                records.push(TileRecord::WaveletInit {
                    tile_index,
                    shift_y,
                    shift_c,
                    payload,
                });
            }
            self.stats.changed_tiles += 1;
        }
        self.stats.refined_tiles += refined as u64;
        self.finish(records, NEW_OS_FRAME, capture_ts_us, start)
    }
    pub fn poll_pending_refinement(&mut self) -> Result<Option<Vec<u8>>> {
        self.config.validate()?;
        let start = Instant::now();
        for state in &mut self.states.tiles {
            if state.needs_refinement() {
                state.still_frames = state.still_frames.saturating_add(1);
                state.change_heat = state.change_heat.saturating_sub(1);
            }
        }
        if self.config.qos_state != QoSState::Green {
            return Ok(None);
        }
        let mut records = Vec::new();
        for index in 0..self.states.tiles.len() {
            let state = &self.states.tiles[index];
            if state.needs_refinement() && state.still_frames >= self.config.refine_min_still_frames
            {
                records.push(self.refine_tile(index)?);
                if records.len() >= self.config.max_refine_tiles_per_frame {
                    break;
                }
            }
        }
        self.stats.refined_tiles += records.len() as u64;
        self.finish(records, 0, self.last_capture_ts_us, start)
    }
    fn refine_tile(&mut self, index: usize) -> Result<TileRecord> {
        let state = &mut self.states.tiles[index];
        let coeff = state
            .coefficients
            .as_ref()
            .ok_or(Error("missing refinement coefficients"))?;
        let payload =
            math::encode_wavelet_refine(coeff, state.sent_shift_y, state.sent_shift_c, 0, 0)?;
        let rgb = math::wavelet_to_rgb(coeff)?;
        self.cache.insert(&rgb)?;
        state.sent_shift_y = 0;
        state.sent_shift_c = 0;
        state.coefficients = None;
        Ok(TileRecord::WaveletRefine {
            tile_index: index as u16,
            shift_y: 0,
            shift_c: 0,
            payload,
        })
    }
    fn finish(
        &mut self,
        records: Vec<TileRecord>,
        flags: u8,
        capture_ts_us: u64,
        start: Instant,
    ) -> Result<Option<Vec<u8>>> {
        if records.is_empty() {
            return Ok(None);
        }
        self.frame_seq = self.frame_seq.wrapping_add(1);
        let header = GDeskFrameHeader {
            width: self.width,
            height: self.height,
            canvas_epoch: self.canvas_epoch,
            frame_seq: self.frame_seq,
            frame_flags: flags
                | if self.reset_pending {
                    KEYFRAME_RESET
                } else {
                    0
                },
            qos_state: self.config.qos_state,
            capture_ts_us,
            encode_send_delay_us: start.elapsed().as_micros().min(u32::MAX as u128) as u32,
            ..Default::default()
        };
        let bytes = protocol::encode_frame(header, &records)?;
        self.reset_pending = false;
        self.stats.frames += 1;
        self.stats.encoded_bytes += bytes.len() as u64;
        self.stats.qos_state = self.config.qos_state;
        Ok(Some(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GDeskDecoder;

    #[test]
    fn congestion_overrides_initial_shifts_and_green_refines_losslessly() {
        for (qos, expected) in [(QoSState::Yellow, (2, 4)), (QoSState::Red, (4, 6))] {
            let config = GDeskConfig {
                initial_shifts: Some((0, 0)),
                ..Default::default()
            };
            let mut encoder = GDeskEncoder::new(64, 64, config).unwrap();
            let mut decoder = GDeskDecoder::new();
            let rgb: Vec<u8> = (0..64 * 64 * 3)
                .map(|i| ((i * 173 + i / 7) & 255) as u8)
                .collect();
            encoder.set_qos_state(qos);
            let frame = encoder
                .encode_frame(&rgb, 64 * 3, PixelFormat::Rgb, 1)
                .unwrap()
                .unwrap();
            let (header, records) = protocol::decode_frame(&frame).unwrap();
            assert_eq!(header.qos_state, qos);
            assert!(
                matches!(records[0], TileRecord::WaveletInit { shift_y, shift_c, .. }
                if (shift_y, shift_c) == expected)
            );
            decoder.decode_frame(&frame).unwrap();
            assert!(!decoder.is_lossless());
            for _ in 0..3 {
                assert!(encoder
                    .encode_frame(&rgb, 64 * 3, PixelFormat::Rgb, 2)
                    .unwrap()
                    .is_none());
                assert!(encoder.poll_pending_refinement().unwrap().is_none());
            }
            assert!(encoder.has_pending_refinement());
            encoder.set_qos_state(QoSState::Green);
            let refine = encoder.poll_pending_refinement().unwrap().unwrap();
            assert_eq!(decoder.decode_frame(&refine).unwrap(), rgb);
            assert!(decoder.is_lossless());
            assert!(!encoder.has_pending_refinement());
            assert!(encoder.poll_pending_refinement().unwrap().is_none());
        }
    }
}
