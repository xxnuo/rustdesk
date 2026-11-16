use crate::{
    codec::{EncoderApi, EncoderCfg},
    EncodeInput, EncodeYuvFormat, ImageFormat, ImageRgb, Pixfmt,
};
use base::message_proto::{video_frame, EncodedVideoFrame, EncodedVideoFrames, VideoFrame};
use gdesk::{
    protocol::KEYFRAME_RESET, GDeskConfig, GDeskDecoder, GDeskEncoder, GDeskFrameHeader,
    PixelFormat,
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

#[derive(Debug, Clone)]
pub struct GDeskEncoderConfig {
    pub width: usize,
    pub height: usize,
}

pub struct GDeskEncoderApi {
    inner: GDeskEncoder,
    width: usize,
    height: usize,
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
        })
    }

    fn encode_to_message(&mut self, frame: EncodeInput, ms: i64) -> ResultType<VideoFrame> {
        let pixels = frame.yuv()?;
        match self
            .inner
            .encode_frame(
                pixels,
                self.width * 4,
                PixelFormat::Bgra,
                (ms.max(0) as u64).saturating_mul(1000),
            )
            .map_err(|e| anyhow!("gdesk encode failed: {e}"))?
        {
            Some(bytes) => to_video_frame(bytes, ms),
            None => Err(GDeskNoOutput.into()),
        }
    }

    fn poll_refinement(&mut self, ms: i64) -> ResultType<Option<VideoFrame>> {
        match self
            .inner
            .poll_pending_refinement()
            .map_err(|e| anyhow!("gdesk refinement failed: {e}"))?
        {
            Some(bytes) => Ok(Some(to_video_frame(bytes, ms)?)),
            None => Ok(None),
        }
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
        decoder
            .decode_frame(&frame.data)
            .map_err(|e| anyhow!("gdesk decode failed: {e}"))?;
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

pub fn is_gdesk_frame(frame: &video_frame::Union) -> bool {
    matches!(frame, video_frame::Union::Gdesks(_))
}
