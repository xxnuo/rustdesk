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

use std::convert::TryFrom;

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
        self.inner
            .encode_udp_datagrams(pixels, self.width * 4, PixelFormat::Bgra, capture_ts_us)
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

fn to_video_frame(bytes: Vec<u8>, ms: i64) -> ResultType<VideoFrame> {
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
        ..Default::default()
    });
    let mut vf = VideoFrame::new();
    vf.set_gdesks(frames);
    Ok(vf)
}

fn datagrams_to_video_frame(datagrams: Vec<Vec<u8>>, ms: i64) -> ResultType<VideoFrame> {
    let mut frames = EncodedVideoFrames::new();
    for datagram in datagrams {
        frames.frames.push(EncodedVideoFrame {
            data: datagram.into(),
            gdesk_datagram: true,
            pts: ms,
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
            ..Default::default()
        };
        let inner = GDeskEncoder::new(
            u16::try_from(cfg.width)?,
            u16::try_from(cfg.height)?,
            config,
        )
        .map_err(|e| anyhow!("create gdesk encoder failed: {e}"))?;
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
            return datagrams_to_video_frame(datagrams, ms);
        }
        match self
            .inner
            .encode_frame(
                pixels,
                self.width * 4,
                PixelFormat::Bgra,
                gdesk::monotonic_us(),
            )
            .map_err(|e| anyhow!("gdesk encode failed: {e}"))?
        {
            Some(bytes) => to_video_frame(bytes, ms),
            None => Err(GDeskNoOutput.into()),
        }
    }

    fn poll_refinement(&mut self, ms: i64) -> ResultType<Option<VideoFrame>> {
        if self.datagram_mode {
            let datagrams = self
                .inner
                .poll_refinement_udp_datagrams(gdesk::monotonic_us())
                .map_err(|e| anyhow!("gdesk datagram refinement failed: {e}"))?;
            return if datagrams.is_empty() {
                Ok(None)
            } else {
                datagrams_to_video_frame(datagrams, ms).map(Some)
            };
        }
        match self
            .inner
            .poll_pending_refinement()
            .map_err(|e| anyhow!("gdesk refinement failed: {e}"))?
        {
            Some(bytes) => Ok(Some(to_video_frame(bytes, ms)?)),
            None => Ok(None),
        }
    }

    fn set_gdesk_qos(&mut self, qos: u8) {
        self.inner
            .set_qos_state(QoSState::try_from(qos).unwrap_or(QoSState::Red));
    }

    fn has_gdesk_pending_refinement(&self) -> bool {
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
        false
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
