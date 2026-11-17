use crate::{
    codec::{EncoderApi, EncoderCfg},
    EncodeInput, EncodeYuvFormat, ImageFormat, ImageRgb, Pixfmt,
};
use base::message_proto::{video_frame, EncodedVideoFrame, EncodedVideoFrames, VideoFrame};
use gdesk::{
    protocol::KEYFRAME_RESET, GDeskConfig, GDeskDecoder, GDeskEncoder, GDeskFrameHeader,
    PixelFormat, QoSState,
};
use hbb_common::{anyhow::anyhow, bail, ResultType};

use std::{
    collections::VecDeque,
    convert::TryFrom,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq)]
pub struct GDeskOverlayMetrics {
    pub active_backend: &'static str,
    pub photon_latency_ms: f64,
    pub encode_us: u32,
    pub decode_us: u32,
    pub frame_p99_jitter_ms: f64,
    pub cache_hit_ratio_pct: f64,
    pub qos_light: &'static str,
    pub realtime_kbps: u32,
    pub frame_interval_ms: f64,
    measured_at: Instant,
}

impl GDeskOverlayMetrics {
    pub fn estimated_photon_latency_ms(&self, rtt_ms: u32, render_queue_depth: usize) -> f64 {
        self.photon_latency_ms
            + rtt_ms as f64 / 2.0
            + render_queue_depth as f64 * self.frame_interval_ms
    }

    pub fn current_realtime_kbps(&self) -> u32 {
        if self.measured_at.elapsed() >= Duration::from_secs(1) {
            0
        } else {
            self.realtime_kbps
        }
    }
}

#[derive(Default)]
pub(super) struct GDeskOverlayState {
    metrics: Option<GDeskOverlayMetrics>,
    last_frame: Option<(u32, Instant)>,
    last_interval: Option<Duration>,
    jitter_us: VecDeque<u64>,
    payloads: VecDeque<(Instant, u64)>,
}

impl GDeskOverlayState {
    fn record(
        &mut self,
        packet: &[u8],
        decode_us: u32,
        now: Instant,
        stats: &gdesk::decoder::GDeskDecodeStats,
    ) -> ResultType<()> {
        let header = GDeskFrameHeader::from_bytes(packet)
            .map_err(|e| anyhow!("gdesk metrics header failed: {e}"))?;
        let total_records = stats
            .cache_hits
            .saturating_add(stats.solid_fills)
            .saturating_add(stats.two_color_texts)
            .saturating_add(stats.wavelet_inits)
            .saturating_add(stats.wavelet_refines);
        if let Some((sequence, previous)) = self.last_frame {
            if sequence != header.frame_seq {
                let interval = now.saturating_duration_since(previous);
                if let Some(old_interval) = self.last_interval {
                    let jitter = interval
                        .abs_diff(old_interval)
                        .as_micros()
                        .min(u64::MAX as u128) as u64;
                    if self.jitter_us.len() == 64 {
                        self.jitter_us.pop_front();
                    }
                    self.jitter_us.push_back(jitter);
                }
                self.last_interval = Some(interval);
                self.last_frame = Some((header.frame_seq, now));
            }
        } else {
            self.last_frame = Some((header.frame_seq, now));
        }
        while self
            .payloads
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= Duration::from_secs(1))
        {
            self.payloads.pop_front();
        }
        // Group bursts into 20ms buckets, bounding the one-second payload window.
        if let Some((_, bytes)) = self
            .payloads
            .back_mut()
            .filter(|(at, _)| now.saturating_duration_since(*at) < Duration::from_millis(20))
        {
            *bytes = bytes.saturating_add(packet.len() as u64);
        } else {
            self.payloads.push_back((now, packet.len() as u64));
        }
        let mut jitter: Vec<u64> = self.jitter_us.iter().copied().collect();
        jitter.sort_unstable();
        let p99 = if jitter.is_empty() {
            0
        } else {
            jitter[(jitter.len() * 99).div_ceil(100).saturating_sub(1)]
        };
        self.metrics = Some(GDeskOverlayMetrics {
            active_backend: "Unknown",
            // Half a 240Hz scanout, matching the Phase 5 photon estimate.
            photon_latency_ms: (header.encode_send_delay_us as f64 + decode_us as f64 + 2_083.0)
                / 1000.0,
            encode_us: header.encode_send_delay_us,
            decode_us,
            frame_p99_jitter_ms: p99 as f64 / 1000.0,
            cache_hit_ratio_pct: if total_records == 0 {
                0.0
            } else {
                stats.cache_hits as f64 * 100.0 / total_records as f64
            },
            qos_light: match header.qos_state {
                QoSState::Green => "Green",
                QoSState::Yellow => "Yellow",
                QoSState::Red => "Red",
            },
            realtime_kbps: self.bandwidth(now),
            frame_interval_ms: self
                .last_interval
                .map_or(1000.0 / 240.0, |interval| interval.as_secs_f64() * 1000.0),
            measured_at: now,
        });
        Ok(())
    }

    fn record_backend(&mut self, backend: &str) {
        if let Some(metrics) = &mut self.metrics {
            metrics.active_backend = match backend {
                "GpuVulkan" => "GpuVulkan",
                "GpuDx12" => "GpuDx12",
                "GpuMetal" => "GpuMetal",
                "CpuFallback" => "CpuFallback",
                _ => "Unknown",
            };
        }
    }

    fn bandwidth(&self, now: Instant) -> u32 {
        (self
            .payloads
            .iter()
            .filter(|(at, _)| now.saturating_duration_since(*at) < Duration::from_secs(1))
            .map(|(_, bytes)| *bytes)
            .sum::<u64>()
            .saturating_mul(8)
            / 1000)
            .min(u32::MAX as u64) as u32
    }
    fn snapshot(&self, now: Instant) -> Option<GDeskOverlayMetrics> {
        self.metrics.clone().map(|mut metrics| {
            metrics.realtime_kbps = self.bandwidth(now);
            metrics
        })
    }
}

pub const MAX_REFINE_TILES_PER_FRAME: usize = 128;

#[derive(Debug)]
pub struct GDeskNoOutput;

impl std::fmt::Display for GDeskNoOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("gdesk: nothing left to send")
    }
}

impl std::error::Error for GDeskNoOutput {}

impl GDeskEncoderApi {
    fn recovery_frame(&mut self, ms: i64) -> ResultType<Option<VideoFrame>> {
        match self.inner.gpu_recovery_pixels()
            .map_err(|e| anyhow!("gdesk recovery snapshot failed: {e}"))? {
            Some(pixels) => self.encode_to_message(EncodeInput::YUV(&pixels), ms).map(Some),
            None => Ok(None),
        }
    }

    fn with_recovery<T>(
        &mut self,
        mut encode: impl FnMut(&mut GDeskEncoder) -> gdesk::Result<T>,
    ) -> gdesk::Result<T> {
        self.inner.recover_gpu()?;
        match encode(&mut self.inner) {
            Err(_) if self.inner.recover_gpu()? => {
                match encode(&mut self.inner) {
                    Err(_) if self.inner.gpu_failed() => {
                        self.inner.fallback_gpu()?;
                        encode(&mut self.inner)
                    }
                    result => result,
                }
            }
            result => result,
        }
    }

    pub fn active_backend(&self) -> &'static str {
        self.inner.active_backend()
    }

    pub fn gpu_dispatches(&self) -> u64 {
        self.inner.gpu_dispatches()
    }
    pub fn enable_udp_datagrams(&mut self, enabled: bool) -> ResultType<()> {
        if enabled != self.datagram_mode {
            self.inner
                .reset(u16::try_from(self.width)?, u16::try_from(self.height)?)
                .map_err(|e| anyhow!("gdesk datagram mode reset failed: {e}"))?;
            self.datagram_mode = enabled;
        }
        Ok(())
    }

    pub fn apply_tile_ack_bitmap(&mut self, acked_frame_seq: u32, bitmap: &[u8]) -> ResultType<()> {
        self.inner
            .apply_tile_ack_bitmap(acked_frame_seq, bitmap)
            .map_err(|e| anyhow!("gdesk tile acknowledgement failed: {e}"))
    }

    pub fn encode_udp_datagrams(
        &mut self,
        pixels: &[u8],
        capture_ts_us: u64,
    ) -> ResultType<Vec<Vec<u8>>> {
        let stride = self.width * 4;
        self.with_recovery(|inner| inner.encode_udp_datagrams(pixels, stride, PixelFormat::Bgra, capture_ts_us))
            .map_err(|e| anyhow!("gdesk udp encode failed: {e}"))
    }

    pub fn resize_seamless(
        &mut self,
        width: usize,
        height: usize,
        format: Pixfmt,
    ) -> ResultType<()> {
        if !matches!(format, Pixfmt::BGRA) {
            bail!("gdesk encoder only accepts BGRA frames");
        }
        self.inner
            .resize_seamless(u16::try_from(width)?, u16::try_from(height)?)
            .map_err(|e| anyhow!("gdesk resize failed: {e}"))?;
        self.width = width;
        self.height = height;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct GDeskEncoderConfig {
    pub width: usize,
    pub height: usize,
}

pub struct GDeskEncoderApi {
    inner: GDeskEncoder,
    width: usize,
    height: usize,
    datagram_mode: bool,
}

fn to_video_frame(bytes: Vec<u8>, ms: i64, backend: &str) -> ResultType<VideoFrame> {
    let key = GDeskFrameHeader::from_bytes(&bytes)
        .map_err(|e| anyhow!("bad gdesk frame header: {e}"))?
        .frame_flags
        & KEYFRAME_RESET
        != 0;
    let mut frames = EncodedVideoFrames::new();
    frames.frames.push(EncodedVideoFrame {
        data: bytes.into(),
        key,
        pts: ms,
        gdesk_active_backend: backend.into(),
        ..Default::default()
    });
    let mut vf = VideoFrame::new();
    vf.set_gdesks(frames);
    Ok(vf)
}

fn datagrams_to_video_frame(datagrams: Vec<Vec<u8>>, ms: i64, backend: &str) -> ResultType<VideoFrame> {
    let mut frames = EncodedVideoFrames::new();
    for datagram in datagrams {
        frames.frames.push(EncodedVideoFrame {
            data: datagram.into(),
            gdesk_datagram: true,
            pts: ms,
            gdesk_active_backend: backend.into(),
            ..Default::default()
        });
    }
    let mut vf = VideoFrame::new();
    vf.set_gdesks(frames);
    Ok(vf)
}

impl EncoderApi for GDeskEncoderApi {
    fn new(cfg: EncoderCfg, _i444: bool) -> ResultType<Self> {
        let EncoderCfg::GDesk(cfg) = cfg else {
            bail!("not a gdesk config");
        };
        let config = GDeskConfig {
            max_refine_tiles_per_frame: MAX_REFINE_TILES_PER_FRAME,
            udp_mtu: gdesk::udp::GDESK_UDP_PAYLOAD_MTU,
            ..Default::default()
        };
        let mut inner = GDeskEncoder::new(
            u16::try_from(cfg.width)?,
            u16::try_from(cfg.height)?,
            config,
        )
        .map_err(|e| anyhow!("create gdesk encoder failed: {e}"))?;
        inner.enable_gpu();
        Ok(Self {
            inner,
            width: cfg.width,
            height: cfg.height,
            datagram_mode: false,
        })
    }

    fn encode_to_message(&mut self, frame: EncodeInput, ms: i64) -> ResultType<VideoFrame> {
        let pixels = frame.yuv()?;
        if self.datagram_mode {
            let datagrams = self.encode_udp_datagrams(pixels, gdesk::monotonic_us())?;
            if datagrams.is_empty() {
                return Err(GDeskNoOutput.into());
            }
            return datagrams_to_video_frame(datagrams, ms, self.active_backend());
        }
        let stride = self.width * 4;
        match self.with_recovery(|inner| inner.encode_frame(
                pixels,
                stride,
                PixelFormat::Bgra,
                gdesk::monotonic_us(),
            ))
            .map_err(|e| anyhow!("gdesk encode failed: {e}"))?
        {
            Some(bytes) => to_video_frame(bytes, ms, self.active_backend()),
            None => Err(GDeskNoOutput.into()),
        }
    }

    fn poll_refinement(&mut self, ms: i64) -> ResultType<Option<VideoFrame>> {
        if let Some(frame) = self.recovery_frame(ms)? {
            return Ok(Some(frame));
        }
        if self.datagram_mode {
            let datagrams = match self.inner.poll_refinement_udp_datagrams(gdesk::monotonic_us()) {
                Err(_) if self.inner.gpu_failed() => return self.recovery_frame(ms),
                result => result.map_err(|e| anyhow!("gdesk datagram refinement failed: {e}"))?,
            };
            return if datagrams.is_empty() {
                Ok(None)
            } else {
                datagrams_to_video_frame(datagrams, ms, self.active_backend()).map(Some)
            };
        }
        match self.inner.poll_pending_refinement()
            .map_err(|e| anyhow!("gdesk refinement failed: {e}"))?
        {
            Some(bytes) => Ok(Some(to_video_frame(bytes, ms, self.active_backend())?)),
            None => Ok(None),
        }
    }

    fn set_gdesk_qos(&mut self, qos: u8) {
        self.inner
            .set_qos_state(QoSState::try_from(qos).unwrap_or(QoSState::Red));
    }

    fn has_gdesk_pending_refinement(&self) -> bool {
        if self.inner.gpu_failed() { return true; }
        if self.datagram_mode {
            self.inner.has_udp_unconfirmed()
        } else {
            self.inner.has_pending_refinement()
        }
    }

    fn resize_gdesk(&mut self, width: usize, height: usize) -> ResultType<()> {
        self.resize_seamless(width, height, Pixfmt::BGRA)
    }

    fn apply_gdesk_tile_ack(&mut self, acked_frame_seq: u32, bitmap: &[u8]) -> ResultType<()> {
        self.apply_tile_ack_bitmap(acked_frame_seq, bitmap)
    }

    fn enable_gdesk_datagrams(&mut self, enabled: bool) -> ResultType<()> {
        self.enable_udp_datagrams(enabled)
    }

    fn is_gdesk(&self) -> bool {
        true
    }

    fn yuvfmt(&self) -> EncodeYuvFormat {
        EncodeYuvFormat {
            pixfmt: Pixfmt::BGRA,
            w: self.width,
            h: self.height,
            stride: vec![self.width * 4],
            u: 0,
            v: 0,
        }
    }

    #[cfg(feature = "vram")]
    fn input_texture(&self) -> bool {
        false
    }

    fn set_quality(&mut self, _ratio: f32) -> ResultType<()> {
        Ok(())
    }

    fn bitrate(&self) -> u32 {
        0
    }

    fn support_changing_quality(&self) -> bool {
        false
    }

    fn latency_free(&self) -> bool {
        true
    }

    fn is_hardware(&self) -> bool {
        self.active_backend().starts_with("Gpu")
    }

    fn disable(&self) {}
}

pub fn decode_into(
    decoder: &mut GDeskDecoder,
    frames: &EncodedVideoFrames,
    rgb: &mut ImageRgb,
) -> ResultType<bool> {
    let mut decoded = false;
    for frame in frames.frames.iter() {
        if frame.gdesk_datagram {
            let header = GDeskFrameHeader::from_bytes(&frame.data)
                .map_err(|e| anyhow!("gdesk datagram header failed: {e}"))?;
            decoder
                .decode_udp_datagram(&frame.data)
                .map_err(|e| anyhow!("gdesk datagram decode failed: {e}"))?;
            decoder.last_header = Some(header);
        } else {
            decoder
                .decode_frame(&frame.data)
                .map_err(|e| anyhow!("gdesk decode failed: {e}"))?;
        }
        decoded = true;
    }
    if !decoded {
        return Ok(false);
    }
    let (width, height) = decoder
        .dimensions()
        .ok_or_else(|| anyhow!("gdesk decoder has no canvas"))?;
    let (format, bytes_per_pixel) = match rgb.fmt {
        ImageFormat::Raw => (PixelFormat::Rgb, 3),
        ImageFormat::ARGB => (PixelFormat::Bgra, 4),
        ImageFormat::ABGR => (PixelFormat::Rgba, 4),
    };
    rgb.w = width as usize;
    rgb.h = height as usize;
    let align = rgb.align.max(1);
    let stride = (rgb.w * bytes_per_pixel + align - 1) / align * align;
    rgb.raw.resize(stride * rgb.h, 0);
    decoder
        .write_into(&mut rgb.raw, stride, format)
        .map_err(|e| anyhow!("gdesk write failed: {e}"))?;
    Ok(true)
}

impl crate::codec::Decoder {
    pub fn gdesk_overlay_metrics(&self) -> Option<GDeskOverlayMetrics> {
        self.gdesk_overlay.snapshot(Instant::now())
    }

    pub(super) fn handle_gdesks_video_frame(
        &mut self,
        frames: &EncodedVideoFrames,
        rgb: &mut ImageRgb,
    ) -> ResultType<bool> {
        let started = Instant::now();
        let decoder = self
            .gdesk
            .as_mut()
            .ok_or_else(|| anyhow!("gdesk decoder unavailable"))?;
        let decoded = decode_into(decoder, frames, rgb)?;
        if decoded {
            let decode_us = started.elapsed().as_micros().min(u32::MAX as u128) as u32;
            let now = Instant::now();
            for frame in &frames.frames {
                self.gdesk_overlay
                    .record(&frame.data, decode_us, now, &decoder.stats)?;
                self.gdesk_overlay.record_backend(&frame.gdesk_active_backend);
            }
        }
        Ok(decoded)
    }

    pub fn gdesk_tile_ack_bitmap(&self) -> Vec<u8> {
        self.gdesk
            .as_ref()
            .and_then(|decoder| {
                decoder
                    .last_header
                    .map(|h| decoder.tile_ack_bitmap(h.frame_seq))
            })
            .unwrap_or_default()
    }

    pub fn handle_gdesk_udp_datagram(
        &mut self,
        data: &[u8],
        rgb: &mut Vec<u8>,
        width: usize,
        height: usize,
        rgba: bool,
    ) -> ResultType<bool> {
        let started = Instant::now();
        let header = GDeskFrameHeader::from_bytes(data)
            .map_err(|e| anyhow!("gdesk datagram header failed: {e}"))?;
        if (header.width as usize, header.height as usize) != (width, height) {
            bail!("gdesk datagram canvas dimensions differ");
        }
        let decoder = self
            .gdesk
            .as_mut()
            .ok_or_else(|| anyhow!("gdesk decoder unavailable"))?;
        let updated = decoder
            .decode_udp_datagram(data)
            .map_err(|e| anyhow!("gdesk datagram decode failed: {e}"))?;
        decoder.last_header = Some(header);
        let format = if rgba {
            PixelFormat::Rgba
        } else {
            PixelFormat::Rgb
        };
        rgb.resize(width * height * format.bytes_per_pixel(), 0);
        decoder
            .write_into(rgb, width * format.bytes_per_pixel(), format)
            .map_err(|e| anyhow!("gdesk datagram output failed: {e}"))?;
        let decode_us = started.elapsed().as_micros().min(u32::MAX as u128) as u32;
        self.gdesk_overlay
            .record(data, decode_us, Instant::now(), &decoder.stats)?;
        Ok(!updated.is_empty())
    }
}

pub fn is_gdesk_frame(frame: &video_frame::Union) -> bool {
    matches!(frame, video_frame::Union::Gdesks(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gdesk_production_uses_hardware_and_preserves_wire_pixels() {
        let mut encoder = GDeskEncoderApi::new(
            EncoderCfg::GDesk(GDeskEncoderConfig { width: 67, height: 65 }),
            false,
        ).unwrap();
        if !encoder.is_hardware() {
            assert_eq!(encoder.active_backend(), "CpuFallback");
        }
        let pixels: Vec<u8> = (0..67 * 65 * 4)
            .map(|i| ((i * 173 + i / 7) & 255) as u8).collect();
        let frame = encoder.encode_to_message(EncodeInput::YUV(&pixels), 1).unwrap();
        if encoder.is_hardware() { assert!(encoder.gpu_dispatches() > 0); }
        let mut decoder = GDeskDecoder::new();
        let mut output = ImageRgb::new(ImageFormat::ARGB, 1);
        decode_into(&mut decoder, frame.gdesks(), &mut output).unwrap();
        for (actual, expected) in output.raw.chunks_exact(4).zip(pixels.chunks_exact(4)) {
            assert_eq!(&actual[..3], &expected[..3]);
        }
        assert_eq!(frame.gdesks().frames[0].gdesk_active_backend, encoder.active_backend());
        for (sent, expected) in [("GpuVulkan", "GpuVulkan"), ("CpuFallback", "CpuFallback"), ("", "Unknown"), ("invalid", "Unknown")] {
            let mut frame = frame.clone();
            frame.mut_gdesks().frames[0].gdesk_active_backend = sent.into();
            let mut decoder = crate::codec::Decoder::new(crate::CodecFormat::GDesk, None);
            decoder.handle_video_frame(frame.union.as_ref().unwrap(), &mut output,
                &mut crate::ImageTexture::default(), &mut true, &mut None).unwrap();
            assert_eq!(decoder.gdesk_overlay_metrics().unwrap().active_backend, expected);
        }
    }

    #[test]
    fn gdesk_preserves_details_outside_prototype_scan() {
        let mut encoder = GDeskEncoderApi::new(
            EncoderCfg::GDesk(GDeskEncoderConfig { width: 64, height: 64 }), false,
        ).unwrap();
        let mut pixels = [0, 0, 255, 255].repeat(64 * 64);
        pixels[20 * 4..20 * 4 + 4].copy_from_slice(&[255, 0, 0, 255]);
        pixels[21 * 4..21 * 4 + 4].copy_from_slice(&[0, 255, 0, 255]);
        let frame = encoder.encode_to_message(EncodeInput::YUV(&pixels), 1).unwrap();
        let mut decoder = GDeskDecoder::new();
        let mut output = ImageRgb::new(ImageFormat::ARGB, 1);
        decode_into(&mut decoder, frame.gdesks(), &mut output).unwrap();
        assert_eq!(output.raw, pixels);
        if encoder.is_hardware() { assert!(encoder.gpu_dispatches() > 0); }
    }

    #[test]
    fn gdesk_production_automatically_recovers_active_and_static_frames() {
        for datagrams in [false, true] {
            for idle in [false, true] {
                let mut encoder = GDeskEncoderApi::new(
                    EncoderCfg::GDesk(GDeskEncoderConfig { width: 64, height: 64 }), false,
                ).unwrap();
                encoder.enable_udp_datagrams(datagrams).unwrap();
                let pixels: Vec<u8> = (0..64 * 64 * 4).map(|i| ((i * 173 + i / 7) & 255) as u8).collect();
                let first = encoder.encode_to_message(EncodeInput::YUV(&pixels), 1).unwrap();
                let epoch = GDeskFrameHeader::from_bytes(&first.gdesks().frames[0].data).unwrap().canvas_epoch;
                let mut decoder = GDeskDecoder::new();
                let mut output = ImageRgb::new(ImageFormat::ARGB, 1);
                decode_into(&mut decoder, first.gdesks(), &mut output).unwrap();
                let Some(device) = encoder.inner.gpu_device() else {
                    eprintln!("SKIP automatic device-loss test: no hardware GPU");
                    return;
                };
                device.destroy();
                device.poll(gdesk::gpu::wgpu::Maintain::Wait);
                let recovered = if idle {
                    assert!(encoder.has_gdesk_pending_refinement());
                    encoder.poll_refinement(2).unwrap().unwrap()
                } else {
                    encoder.encode_to_message(EncodeInput::YUV(&pixels), 2).unwrap()
                };
                let header = GDeskFrameHeader::from_bytes(&recovered.gdesks().frames[0].data).unwrap();
                assert_eq!(header.canvas_epoch, epoch.wrapping_add(1));
                decode_into(&mut decoder, recovered.gdesks(), &mut output).unwrap();
                let mut sequence = header.frame_seq;
                for tick in 3..500 {
                    if datagrams { encoder.apply_tile_ack_bitmap(sequence, &decoder.tile_ack_bitmap(sequence)).unwrap(); }
                    if !encoder.has_gdesk_pending_refinement() { break; }
                    if let Some(frame) = encoder.poll_refinement(tick).unwrap() {
                        decode_into(&mut decoder, frame.gdesks(), &mut output).unwrap();
                        sequence = GDeskFrameHeader::from_bytes(&frame.gdesks().frames[0].data).unwrap().frame_seq;
                    }
                }
                assert!(!encoder.has_gdesk_pending_refinement());
                for (actual, expected) in output.raw.chunks_exact(4).zip(pixels.chunks_exact(4)) {
                    assert_eq!(&actual[..3], &expected[..3]);
                }
                println!("automatic recovery backend={} datagrams={datagrams} idle={idle} MaxError=0", encoder.active_backend());
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn gdesk_capture_to_renderer_preserves_pixel_order_and_stride() {
        for (input_format, pixel) in [
            (Pixfmt::BGRA, [13, 37, 211, 0]),
            (Pixfmt::RGBA, [211, 37, 13, 0]),
        ] {
            let row = [pixel.repeat(3), vec![99; 4]].concat();
            let captured = row.repeat(2);
            for datagrams in [false, true] {
                let mut encoder = GDeskEncoderApi::new(
                    EncoderCfg::GDesk(GDeskEncoderConfig {
                        width: 3,
                        height: 2,
                    }),
                    false,
                )
                .unwrap();
                encoder.enable_udp_datagrams(datagrams).unwrap();
                let frame = crate::Frame::PixelBuffer(crate::PixelBuffer::new(
                    &captured,
                    input_format,
                    3,
                    2,
                ));
                let mut converted = Vec::new();
                let mut scratch = Vec::new();
                let input = frame
                    .to(encoder.yuvfmt(), &mut converted, &mut scratch)
                    .unwrap();
                assert_eq!(input.yuv().unwrap(), [13, 37, 211, 0].repeat(6));
                let encoded = encoder.encode_to_message(input, 1).unwrap();
                for (format, expected) in [
                    (ImageFormat::ARGB, vec![13, 37, 211, 255]),
                    (ImageFormat::ABGR, vec![211, 37, 13, 255]),
                    (ImageFormat::Raw, vec![211, 37, 13]),
                ] {
                    let mut decoder = GDeskDecoder::new();
                    let mut output = ImageRgb::new(format, 16);
                    assert!(decode_into(&mut decoder, encoded.gdesks(), &mut output).unwrap());
                    assert_eq!((output.w, output.h, output.raw.len()), (3, 2, 32));
                    for row in output.raw.chunks_exact(16) {
                        assert_eq!(&row[..3 * expected.len()], expected.repeat(3));
                    }
                }
            }
        }
    }

    #[test]
    fn gdesk_overlay_reads_only_the_fixed_header() {
        let header = GDeskFrameHeader {
            width: 64,
            height: 64,
            tile_record_count: 1,
            ..Default::default()
        };
        let stats = gdesk::decoder::GDeskDecodeStats {
            cache_hits: 1,
            ..Default::default()
        };
        let mut overlay = GDeskOverlayState::default();
        overlay
            .record(&header.to_bytes(), 10, Instant::now(), &stats)
            .unwrap();
        assert_eq!(overlay.metrics.as_ref().unwrap().cache_hit_ratio_pct, 100.0);
    }

    #[test]
    fn gdesk_overlay_measures_jitter_cache_ratio_qos_and_idle_bandwidth() {
        let now = Instant::now();
        let mut overlay = GDeskOverlayState::default();
        let mut bytes_in_window = 0u64;
        for (index, ms) in [0, 16, 33, 49].iter().copied().enumerate() {
            let header = GDeskFrameHeader {
                width: 64,
                height: 64,
                frame_seq: index as u32 + 1,
                encode_send_delay_us: 1200,
                qos_state: QoSState::Yellow,
                ..Default::default()
            };
            let record = if index == 0 {
                gdesk::protocol::TileRecord::SolidFill {
                    tile_index: 0,
                    color: [1, 2, 3],
                }
            } else {
                gdesk::protocol::TileRecord::CacheHit {
                    tile_index: 0,
                    slot_id: 0,
                }
            };
            let packet = gdesk::protocol::encode_frame(header, &[record]).unwrap();
            bytes_in_window += packet.len() as u64;
            let stats = gdesk::decoder::GDeskDecodeStats {
                solid_fills: 1,
                cache_hits: index as u64,
                ..Default::default()
            };
            overlay
                .record(&packet, 300, now + Duration::from_millis(ms), &stats)
                .unwrap();
        }
        let metrics = overlay.snapshot(now + Duration::from_millis(49)).unwrap();
        assert_eq!(metrics.photon_latency_ms, 3.583);
        assert_eq!(metrics.estimated_photon_latency_ms(20, 2), 45.583);
        assert_eq!((metrics.encode_us, metrics.decode_us), (1200, 300));
        assert_eq!(metrics.frame_p99_jitter_ms, 1.0);
        assert_eq!(metrics.cache_hit_ratio_pct, 75.0);
        assert_eq!(metrics.qos_light, "Yellow");
        assert_eq!(metrics.realtime_kbps, (bytes_in_window * 8 / 1000) as u32);
        assert_eq!(
            overlay
                .snapshot(now + Duration::from_secs(2))
                .unwrap()
                .realtime_kbps,
            0
        );
    }

    #[test]
    fn gdesk_datagram_mode_emits_decodable_acknowledged_frames() {
        let mut encoder = GDeskEncoderApi::new(
            EncoderCfg::GDesk(GDeskEncoderConfig {
                width: 64,
                height: 64,
            }),
            false,
        )
        .unwrap();
        encoder.enable_udp_datagrams(true).unwrap();
        let frame = encoder
            .encode_to_message(EncodeInput::YUV(&[3, 2, 1, 255].repeat(64 * 64)), 1)
            .unwrap();
        assert!(frame
            .gdesks()
            .frames
            .iter()
            .all(|frame| frame.gdesk_datagram));
        let mut decoder = crate::codec::Decoder::new(crate::CodecFormat::GDesk, None);
        let mut output = ImageRgb::new(ImageFormat::ABGR, 1);
        assert!(decoder
            .handle_video_frame(
                frame.union.as_ref().unwrap(),
                &mut output,
                &mut crate::ImageTexture::default(),
                &mut true,
                &mut None
            )
            .unwrap());
        assert_eq!(output.raw, [1, 2, 3, 255].repeat(64 * 64));
        let metrics = decoder.gdesk_overlay_metrics().unwrap();
        assert_eq!(metrics.qos_light, "Green");
        assert_eq!(metrics.cache_hit_ratio_pct, 0.0);
        assert!(metrics.photon_latency_ms.is_finite());
        let bitmap = decoder.gdesk_tile_ack_bitmap();
        assert_eq!(bitmap, vec![1]);
        let header = GDeskFrameHeader::from_bytes(&frame.gdesks().frames[0].data).unwrap();
        encoder
            .apply_tile_ack_bitmap(header.frame_seq, &bitmap)
            .unwrap();
        assert!(!encoder.has_gdesk_pending_refinement());
        assert!(encoder.poll_refinement(2).unwrap().is_none());
    }

    #[test]
    fn gdesk_udp_decoder_exports_tile_ack_and_rejects_wrong_extent() {
        let mut encoder = GDeskEncoderApi::new(
            EncoderCfg::GDesk(GDeskEncoderConfig {
                width: 64,
                height: 64,
            }),
            false,
        )
        .unwrap();
        let packets = encoder
            .encode_udp_datagrams(&[3, 2, 1, 255].repeat(64 * 64), 1)
            .unwrap();
        let mut decoder = crate::codec::Decoder::new(crate::CodecFormat::GDesk, None);
        let mut rgb = Vec::new();
        assert!(decoder
            .handle_gdesk_udp_datagram(&packets[0], &mut rgb, 32, 64, true)
            .is_err());
        assert!(decoder
            .handle_gdesk_udp_datagram(&packets[0], &mut rgb, 64, 64, true)
            .unwrap());
        assert_eq!(rgb, [1, 2, 3, 255].repeat(64 * 64));
        assert_eq!(decoder.gdesk_tile_ack_bitmap(), vec![1]);
        let mut decoder = crate::codec::Decoder::new(crate::CodecFormat::GDesk, None);
        let mut frames = EncodedVideoFrames::new();
        frames.frames.push(EncodedVideoFrame {
            data: packets[0].clone().into(),
            gdesk_datagram: true,
            ..Default::default()
        });
        let mut output = ImageRgb::new(ImageFormat::ABGR, 1);
        assert!(decoder
            .handle_video_frame(
                &video_frame::Union::Gdesks(frames),
                &mut output,
                &mut crate::ImageTexture::default(),
                &mut true,
                &mut None
            )
            .unwrap());
        assert_eq!(output.raw, rgb);
        assert_eq!(decoder.gdesk_tile_ack_bitmap(), vec![1]);
    }

    #[test]
    fn qos_trait_hooks_freeze_and_resume_refinement() {
        let mut encoder = GDeskEncoderApi::new(
            EncoderCfg::GDesk(GDeskEncoderConfig {
                width: 64,
                height: 64,
            }),
            false,
        )
        .unwrap();
        let pixels: Vec<u8> = (0..64 * 64 * 4)
            .map(|i| ((i * 173 + i / 9) & 255) as u8)
            .collect();
        encoder.set_gdesk_qos(1);
        let frame = encoder
            .encode_to_message(EncodeInput::YUV(&pixels), 0)
            .unwrap();
        let header = GDeskFrameHeader::from_bytes(&frame.gdesks().frames[0].data).unwrap();
        assert_eq!(header.qos_state, QoSState::Yellow);
        assert!(encoder.has_gdesk_pending_refinement());
        for _ in 0..3 {
            assert!(encoder.poll_refinement(1).unwrap().is_none());
        }
        encoder.set_gdesk_qos(0);
        assert!(encoder.poll_refinement(2).unwrap().is_some());
        assert!(!encoder.has_gdesk_pending_refinement());
    }
}
