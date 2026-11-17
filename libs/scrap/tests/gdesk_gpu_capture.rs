#![cfg(all(feature = "gdesk", target_os = "linux"))]

use scrap::{
    codec::{Decoder, EncoderApi, EncoderCfg},
    gdesk_codec::{GDeskEncoderApi, GDeskEncoderConfig},
    Capturer, CodecFormat, Display, Frame, ImageFormat, ImageRgb, ImageTexture, TraitCapturer,
};
use std::time::Duration;

#[test]
#[ignore = "requires the private Xvfb texture fixture and a hardware Vulkan adapter"]
fn production_capture_dispatches_gpu_and_roundtrips_exactly() {
    let expected_backend = std::env::var("GDESK_EXPECT_BACKEND").unwrap_or("GpuVulkan".into());
    let display = Display::primary().unwrap();
    assert_eq!((display.width(), display.height()), (256, 128));
    let mut capturer = Capturer::new(display).unwrap();
    let frame = capturer.frame(Duration::ZERO).unwrap();
    assert!(matches!(frame, Frame::PixelBuffer(_)));
    for datagrams in [false, true] {
        let mut encoder = GDeskEncoderApi::new(
            EncoderCfg::GDesk(GDeskEncoderConfig {
                width: 256,
                height: 128,
            }),
            false,
        )
        .unwrap();
        assert_eq!(encoder.active_backend(), expected_backend);
        encoder.enable_udp_datagrams(datagrams).unwrap();
        let mut pixels = Vec::new();
        let mut scratch = Vec::new();
        let input = frame
            .to(encoder.yuvfmt(), &mut pixels, &mut scratch)
            .unwrap();
        let mut packet = Some(encoder.encode_to_message(input, 1).unwrap());
        let mut decoders = [
            (
                Decoder::new(CodecFormat::GDesk, None),
                ImageRgb::new(ImageFormat::ARGB, 1),
            ),
            (
                Decoder::new(CodecFormat::GDesk, None),
                ImageRgb::new(ImageFormat::ABGR, 1),
            ),
        ];
        for step in 0..1000 {
            if let Some(frame) = packet.take() {
                assert!(frame
                    .gdesks()
                    .frames
                    .iter()
                    .all(|f| f.gdesk_active_backend == expected_backend));
                for (decoder, output) in &mut decoders {
                    assert!(decoder
                        .handle_video_frame(
                            frame.union.as_ref().unwrap(),
                            output,
                            &mut ImageTexture::default(),
                            &mut true,
                            &mut None
                        )
                        .unwrap());
                    assert_eq!(
                        decoder.gdesk_overlay_metrics().unwrap().active_backend,
                        expected_backend
                    );
                }
                if datagrams {
                    assert!(frame.gdesks().frames.iter().all(|f| f.data.len() <= 1170));
                }
                if datagrams {
                    let seq = gdesk::GDeskFrameHeader::from_bytes(&frame.gdesks().frames[0].data)
                        .unwrap()
                        .frame_seq;
                    encoder
                        .apply_tile_ack_bitmap(seq, &decoders[0].0.gdesk_tile_ack_bitmap())
                        .unwrap();
                }
            }
            if !encoder.has_gdesk_pending_refinement() {
                break;
            }
            packet = encoder.poll_refinement(step + 2).unwrap();
        }
        assert!(!encoder.has_gdesk_pending_refinement());
        if expected_backend.starts_with("Gpu") {
            assert!(encoder.gpu_dispatches() > 0);
        } else {
            assert_eq!(encoder.gpu_dispatches(), 0);
        }
        for (_, output) in &decoders {
            let mut max_error = 0;
            for y in 0..128usize {
                for x in 0..256usize {
                    let mut expected = if x < 64 {
                        [255, 0, 0, 255]
                    } else if x < 128 {
                        [0, 0, 255, 255]
                    } else {
                        [
                            ((x * 3 + y * 5) & 255) as u8,
                            ((y * 7) ^ x) as u8,
                            ((x * 11 + y * 13) & 255) as u8,
                            255,
                        ]
                    };
                    if matches!(output.fmt, ImageFormat::ARGB) {
                        expected.swap(0, 2);
                    }
                    let at = (y * 256 + x) * 4;
                    for c in 0..4 {
                        max_error = max_error.max(output.raw[at + c].abs_diff(expected[c]));
                    }
                }
            }
            assert_eq!(max_error, 0);
            println!(
                "backend={} dispatches={} datagrams={datagrams} format={:?} MaxError={max_error}",
                encoder.active_backend(),
                encoder.gpu_dispatches(),
                output.fmt
            );
        }
    }
}
