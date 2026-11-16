use crate::{
    cache::DeterministicLruCache,
    math::{self, PixelFormat, WaveletState},
    protocol::{self, GDeskFrameHeader, TileRecord, CACHE_KEEP, KEYFRAME_RESET},
    Error, Result,
};

#[derive(Default)]
pub struct GDeskDecoder {
    pub cache: DeterministicLruCache,
    pub last_header: Option<GDeskFrameHeader>,
    pub(crate) canvas: Vec<u8>,
    pub(crate) rendered_canvas: Vec<u8>,
    pub(crate) wavelets: Vec<Option<WaveletState>>,
    pub(crate) lossless: Vec<bool>,
    pub(crate) needs_reset: bool,
    pub udp: crate::udp::UdpDecoderState,
}
impl GDeskDecoder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn dimensions(&self) -> Option<(u16, u16)> {
        self.last_header.map(|h| (h.width, h.height))
    }
    pub fn canvas_rgb(&self) -> &[u8] {
        if self.is_lossless() {
            &self.canvas
        } else {
            &self.rendered_canvas
        }
    }
    pub fn is_lossless(&self) -> bool {
        !self.lossless.is_empty() && self.lossless.iter().all(|v| *v) && !self.needs_reset
    }
    pub fn tile_is_lossless(&self, index: usize) -> bool {
        self.lossless.get(index).copied().unwrap_or(false)
    }
    pub fn decode_frame(&mut self, bytes: &[u8]) -> Result<&[u8]> {
        let (header, records) = protocol::decode_frame(bytes)?;
        let reset = header.frame_flags & KEYFRAME_RESET != 0;
        let count = protocol::tile_count(header.width, header.height)?;
        let mut previous_state = None;
        let mut previous_udp = None;
        if reset {
            if header.frame_flags & CACHE_KEEP != 0 && (self.needs_reset || self.last_header.is_none()) {
                return Err(Error("cannot keep cache when decoder needs full reset"));
            }
            if records.len() != count {
                return Err(Error("reset must initialize every tile"));
            }
            // The previous canvas stays presentable until the new one is complete.
            previous_state = Some((
                std::mem::replace(
                    &mut self.canvas,
                    vec![0; header.width as usize * header.height as usize * 3],
                ),
                std::mem::take(&mut self.rendered_canvas),
                std::mem::replace(&mut self.wavelets, vec![None; count]),
                std::mem::replace(&mut self.lossless, vec![false; count]),
            ));
            if header.frame_flags & CACHE_KEEP == 0 {
                self.cache.clear();
            }
            previous_udp = Some(std::mem::take(&mut self.udp));
            self.needs_reset = false;
        } else {
            if self.udp.active {
                return Err(Error("decoder requires a reset frame"));
            }
            let previous = self
                .last_header
                .ok_or(Error("first frame must reset canvas"))?;
            if self.needs_reset
                || previous.canvas_epoch != header.canvas_epoch
                || previous.width != header.width
                || previous.height != header.height
                || header.frame_seq != previous.frame_seq.wrapping_add(1)
            {
                return Err(Error("decoder requires a reset frame"));
            }
        }
        for record in records {
            if let Err(error) = self.apply_record(&header, record) {
                // A partially applied malformed delta cannot be used as the next frame's base.
                self.needs_reset = true;
                self.cache.clear();
                if let Some(udp) = previous_udp {
                    self.udp = udp;
                }
                if let Some((canvas, rendered, wavelets, lossless)) = previous_state {
                    self.canvas = canvas;
                    self.rendered_canvas = if rendered.is_empty() {
                        self.canvas.clone()
                    } else {
                        rendered
                    };
                    self.wavelets = wavelets;
                    self.lossless = lossless;
                }
                return Err(error);
            }
        }
        self.last_header = Some(header);
        if reset {
            self.udp = crate::udp::UdpDecoderState::reliable(header.canvas_epoch);
        }
        if !self.is_lossless() {
            self.render();
        }
        Ok(self.canvas_rgb())
    }
    pub(crate) fn apply_record(&mut self, header: &GDeskFrameHeader, record: TileRecord) -> Result<()> {
        let index = record.tile_index() as usize;
        let mut wavelet = None;
        let (rgb, lossless, insert) = match record {
            TileRecord::CacheHit { slot_id, .. } => {
                let rgb = self
                    .cache
                    .get(slot_id)
                    .ok_or(Error("missing cache reference"))?
                    .rgb
                    .clone();
                self.cache.touch(slot_id)?;
                (rgb, true, false)
            }
            TileRecord::SolidFill { color, .. } => (math::restore_solid(color), true, false),
            TileRecord::TwoColorText { colors, mask, .. } => {
                (math::restore_two_color(colors, &mask), true, true)
            }
            TileRecord::WaveletInit {
                shift_y,
                shift_c,
                payload,
                ..
            } => {
                let state = math::decode_wavelet_init(&payload, shift_y, shift_c)?;
                let rgb = state.reconstruct()?;
                let lossless = shift_y == 0 && shift_c == 0;
                if !lossless {
                    wavelet = Some(state);
                }
                (rgb, lossless, lossless)
            }
            TileRecord::WaveletRefine {
                shift_y,
                shift_c,
                payload,
                ..
            } => {
                let mut state = self.wavelets[index]
                    .clone()
                    .ok_or(Error("refinement has no initial state"))?;
                math::decode_wavelet_refine(&mut state, &payload, shift_y, shift_c)?;
                let rgb = state.reconstruct()?;
                let lossless = shift_y == 0 && shift_c == 0;
                if !lossless {
                    wavelet = Some(state);
                }
                (rgb, lossless, lossless)
            }
        };
        if insert && !self.udp.active {
            self.cache.insert(&rgb)?;
        }
        self.wavelets[index] = wavelet;
        self.lossless[index] = lossless;
        math::write_tile(
            &rgb,
            &mut self.canvas,
            header.width as usize,
            header.height as usize,
            header.width as usize * 3,
            PixelFormat::Rgb,
            index,
        )
    }
    pub(crate) fn render(&mut self) {
        self.rendered_canvas.clone_from(&self.canvas);
        let Some(header) = self.last_header else {
            return;
        };
        let width = header.width as usize;
        let height = header.height as usize;
        let cols = width.div_ceil(64);
        // Read only the unfiltered canvas; never alter a lossless tile or cached RGB.
        for x in (64..width).step_by(64) {
            for y in 0..height {
                let left_tile = y / 64 * cols + x / 64 - 1;
                let right_tile = left_tile + 1;
                self.smooth_pixel(
                    y * width + x - 1,
                    y * width + x,
                    !self.lossless[left_tile],
                    !self.lossless[right_tile],
                );
            }
        }
        for y in (64..height).step_by(64) {
            for x in 0..width {
                let top_tile = (y / 64 - 1) * cols + x / 64;
                let bottom_tile = top_tile + cols;
                self.smooth_pixel(
                    (y - 1) * width + x,
                    y * width + x,
                    !self.lossless[top_tile],
                    !self.lossless[bottom_tile],
                );
            }
        }
    }
    fn smooth_pixel(&mut self, a: usize, b: usize, filter_a: bool, filter_b: bool) {
        for c in 0..3 {
            let av = self.canvas[a * 3 + c] as u16;
            let bv = self.canvas[b * 3 + c] as u16;
            if filter_a {
                self.rendered_canvas[a * 3 + c] = ((3 * av + bv + 2) / 4) as u8;
            }
            if filter_b {
                self.rendered_canvas[b * 3 + c] = ((3 * bv + av + 2) / 4) as u8;
            }
        }
    }
    pub fn write_into(&self, dst: &mut [u8], stride: usize, format: PixelFormat) -> Result<()> {
        let (width, height) = self.dimensions().ok_or(Error("no decoded canvas"))?;
        if self.needs_reset {
            return Err(Error("decoder requires a reset frame"));
        }
        math::validate_image(dst, width as usize, height as usize, stride, format)?;
        let row_bytes = width as usize * format.bytes_per_pixel();
        for (y, src_row) in self
            .canvas_rgb()
            .chunks_exact(width as usize * 3)
            .enumerate()
        {
            let dst_row = &mut dst[y * stride..y * stride + row_bytes];
            if format == PixelFormat::Rgb {
                dst_row.copy_from_slice(src_row);
            } else {
                for (src, dst) in src_row.chunks_exact(3).zip(dst_row.chunks_exact_mut(4)) {
                    if format == PixelFormat::Bgra {
                        dst[..3].copy_from_slice(&[src[2], src[1], src[0]]);
                    } else {
                        dst[..3].copy_from_slice(src);
                    }
                    dst[3] = 255;
                }
            }
        }
        Ok(())
    }
    pub fn output(&self, format: PixelFormat) -> Result<Vec<u8>> {
        let (width, height) = self.dimensions().ok_or(Error("no decoded canvas"))?;
        let stride = width as usize * format.bytes_per_pixel();
        let mut out = vec![0; stride * height as usize];
        self.write_into(&mut out, stride, format)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::{GDeskConfig, GDeskEncoder};
    fn image(width: usize, height: usize, seed: usize) -> Vec<u8> {
        (0..width * height * 3)
            .map(|i| ((i * 73 + i / (width * 3) * 11 + seed) & 255) as u8)
            .collect()
    }
    #[test]
    fn progressive_roundtrip_and_static_silence() {
        let (w, h) = (129, 67);
        let rgb = image(w, h, 17);
        let config = GDeskConfig {
            initial_shifts: Some((3, 5)),
            ..Default::default()
        };
        let mut encoder = GDeskEncoder::new(w as u16, h as u16, config).unwrap();
        let mut decoder = GDeskDecoder::new();
        let first = encoder
            .encode_frame(&rgb, w * 3, PixelFormat::Rgb, 100)
            .unwrap()
            .unwrap();
        decoder.decode_frame(&first).unwrap();
        assert!(!decoder.is_lossless());
        assert!(encoder.poll_pending_refinement().unwrap().is_none());
        let refine = encoder.poll_pending_refinement().unwrap().unwrap();
        assert_eq!(decoder.decode_frame(&refine).unwrap(), rgb);
        assert!(decoder.is_lossless());
        for _ in 0..3 {
            assert!(encoder
                .encode_frame(&rgb, w * 3, PixelFormat::Rgb, 200)
                .unwrap()
                .is_none());
            assert!(encoder.poll_pending_refinement().unwrap().is_none());
        }
        assert_eq!(
            encoder.cache.lru_slots().collect::<Vec<_>>(),
            decoder.cache.lru_slots().collect::<Vec<_>>()
        );
    }
    #[test]
    fn cache_return_resolution_reset_and_qos_freeze() {
        let a = image(128, 64, 1);
        let b = image(128, 64, 97);
        let mut encoder = GDeskEncoder::new(
            128,
            64,
            GDeskConfig {
                initial_shifts: Some((0, 0)),
                ..Default::default()
            },
        )
        .unwrap();
        let mut decoder = GDeskDecoder::new();
        for rgb in [&a, &b, &a] {
            let packet = encoder
                .encode_frame(rgb, 384, PixelFormat::Rgb, 0)
                .unwrap()
                .unwrap();
            assert_eq!(decoder.decode_frame(&packet).unwrap(), *rgb);
        }
        assert_eq!(encoder.stats.cache_hits, 2);
        encoder.reset(65, 1).unwrap();
        encoder.config.initial_shifts = Some((3, 5));
        let rgb = image(65, 1, 42);
        let packet = encoder
            .encode_frame(&rgb, 195, PixelFormat::Rgb, 0)
            .unwrap()
            .unwrap();
        decoder.decode_frame(&packet).unwrap();
        assert_eq!(decoder.dimensions(), Some((65, 1)));
        encoder.config.qos_state = protocol::QoSState::Red;
        for _ in 0..3 {
            assert!(encoder.poll_pending_refinement().unwrap().is_none());
        }
        encoder.config.qos_state = protocol::QoSState::Green;
        assert_eq!(
            decoder
                .decode_frame(&encoder.poll_pending_refinement().unwrap().unwrap())
                .unwrap(),
            rgb
        );
    }
    #[test]
    fn write_into_preserves_stride_and_handles_lossy_to_lossless() {
        let (width, height) = (65, 3);
        let rgb = image(width, height, 27);
        let mut encoder = GDeskEncoder::new(
            width as u16,
            height as u16,
            GDeskConfig {
                initial_shifts: Some((3, 5)),
                refine_min_still_frames: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let mut decoder = GDeskDecoder::new();
        assert!(decoder.write_into(&mut [], 0, PixelFormat::Rgb).is_err());
        let init = encoder
            .encode_frame(&rgb, width * 3, PixelFormat::Rgb, 0)
            .unwrap()
            .unwrap();
        let refine = encoder.poll_pending_refinement().unwrap().unwrap();
        for packet in [init, refine] {
            decoder.decode_frame(&packet).unwrap();
            for format in [PixelFormat::Rgb, PixelFormat::Rgba, PixelFormat::Bgra] {
                let row_bytes = width * format.bytes_per_pixel();
                let stride = row_bytes + 7;
                let mut dst = vec![99; height * stride];
                decoder.write_into(&mut dst, stride, format).unwrap();
                for y in 0..height {
                    for x in 0..width {
                        let src = &decoder.canvas_rgb()[(y * width + x) * 3..][..3];
                        let dst_pixel = &dst[y * stride + x * format.bytes_per_pixel()..];
                        let expected = if format == PixelFormat::Bgra {
                            [src[2], src[1], src[0]]
                        } else {
                            [src[0], src[1], src[2]]
                        };
                        assert_eq!(&dst_pixel[..3], &expected);
                        if format != PixelFormat::Rgb {
                            assert_eq!(dst_pixel[3], 255);
                        }
                    }
                    assert!(dst[y * stride + row_bytes..(y + 1) * stride]
                        .iter()
                        .all(|v| *v == 99));
                }
                assert!(decoder
                    .write_into(&mut dst[..row_bytes], stride, format)
                    .is_err());
                assert!(decoder.write_into(&mut dst, row_bytes - 1, format).is_err());
            }
        }
        assert_eq!(decoder.canvas_rgb(), rgb);
    }
    #[test]
    fn bad_delta_requires_clean_reset() {
        let mut encoder = GDeskEncoder::new(64, 64, GDeskConfig::default()).unwrap();
        let mut decoder = GDeskDecoder::new();
        let rgb = image(64, 64, 0);
        let first = encoder
            .encode_frame(&rgb, 192, PixelFormat::Rgb, 0)
            .unwrap()
            .unwrap();
        decoder.decode_frame(&first).unwrap();
        let mut h = decoder.last_header.unwrap();
        h.frame_seq += 1;
        h.frame_flags = 0;
        let invalid = protocol::encode_frame(
            h,
            &[TileRecord::CacheHit {
                tile_index: 0,
                slot_id: 4095,
            }],
        )
        .unwrap();
        assert!(decoder.decode_frame(&invalid).is_err());
        assert!(decoder.output(PixelFormat::Rgb).is_err());
        encoder.reset(64, 64).unwrap();
        let packet = encoder
            .encode_frame(&rgb, 192, PixelFormat::Rgb, 0)
            .unwrap()
            .unwrap();
        assert_eq!(decoder.decode_frame(&packet).unwrap(), rgb);
    }

    #[test]
    fn failed_keep_cache_reset_rolls_back_and_taints() {
        let solid = |tile_index: u16, value: u8| TileRecord::SolidFill {
            tile_index,
            color: [value; 3],
        };
        let frame = |flags: u8, epoch: u16, seq: u32, records: Vec<TileRecord>| {
            let header = GDeskFrameHeader {
                width: 128,
                height: 64,
                canvas_epoch: epoch,
                frame_seq: seq,
                frame_flags: flags,
                ..Default::default()
            };
            protocol::encode_frame(header, &records).unwrap()
        };
        let mut decoder = GDeskDecoder::new();
        decoder
            .decode_frame(&frame(KEYFRAME_RESET, 0, 0, vec![solid(0, 10), solid(1, 20)]))
            .unwrap();
        let before = decoder.canvas_rgb().to_vec();
        assert!(decoder
            .decode_frame(&frame(
                KEYFRAME_RESET | CACHE_KEEP,
                1,
                1,
                vec![
                    solid(0, 30),
                    TileRecord::WaveletRefine {
                        tile_index: 1,
                        shift_y: 0,
                        shift_c: 0,
                        payload: vec![1, 2, 3],
                    },
                ],
            ))
            .is_err());
        assert_eq!(decoder.canvas_rgb(), &before[..]);
        let keep = frame(KEYFRAME_RESET | CACHE_KEEP, 1, 2, vec![solid(0, 30), solid(1, 40)]);
        assert!(decoder.decode_frame(&keep).is_err());
        decoder
            .decode_frame(&frame(KEYFRAME_RESET, 2, 3, vec![solid(0, 50), solid(1, 60)]))
            .unwrap();
        decoder.decode_frame(&keep).unwrap();
    }

    #[test]
    fn udp_to_reliable_reset_restores_lru_cache_and_rejects_stale_udp() {
        let a = image(128, 64, 11);
        let b = image(128, 64, 77);
        let mut encoder = GDeskEncoder::new(
            128,
            64,
            GDeskConfig {
                initial_shifts: Some((0, 0)),
                refine_min_still_frames: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let mut decoder = GDeskDecoder::new();

        let udp_datagrams = encoder
            .encode_udp_datagrams(&a, 384, PixelFormat::Rgb, 1_000)
            .unwrap();
        assert!(!udp_datagrams.is_empty());
        for d in &udp_datagrams {
            decoder.decode_udp_datagram(d).unwrap();
        }
        let mut ts = 2_000;
        while !decoder.is_lossless() {
            let refines = encoder.poll_refinement_udp_datagrams(ts).unwrap();
            assert!(!refines.is_empty());
            for d in &refines {
                decoder.decode_udp_datagram(d).unwrap();
            }
            ts += 1_000;
        }
        assert!(decoder.udp.active);
        assert_eq!(decoder.canvas_rgb(), &a[..]);

        // Transition back to reliable mode via encoder.reset() + KEYFRAME_RESET frame.
        encoder.reset(128, 64).unwrap();
        for rgb in [&a, &b, &a] {
            let frame = encoder
                .encode_frame(rgb, 384, PixelFormat::Rgb, 2_000)
                .unwrap()
                .unwrap();
            assert_eq!(decoder.decode_frame(&frame).unwrap(), *rgb);
        }
        assert!(!decoder.udp.active);
        assert!(encoder.stats.cache_hits >= 2);
        assert_eq!(
            encoder.cache.lru_slots().collect::<Vec<_>>(),
            decoder.cache.lru_slots().collect::<Vec<_>>()
        );
        assert!(decoder.decode_udp_datagram(&udp_datagrams[0]).is_err());
    }
}
