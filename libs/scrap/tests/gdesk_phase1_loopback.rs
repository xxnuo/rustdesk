#![cfg(feature = "gdesk")]

use base::message_proto::{supported_decoding::PreferCodec, video_frame, VideoFrame};
use hbb_common::{
    protobuf::Message,
    tokio::{
        self,
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    },
};
use scrap::{
    codec::{Decoder, Encoder, EncoderCfg, EncodingUpdate},
    gdesk_codec::{GDeskEncoderConfig, GDeskNoOutput},
    CodecFormat, EncodeInput, Frame, ImageFormat, ImageRgb, ImageTexture, PixelBuffer, Pixfmt,
};
use std::time::{Duration, Instant};

const WIDTH: usize = 640;
const HEIGHT: usize = 360;

fn mix(x: usize, y: usize, seed: usize) -> u32 {
    let mut v = (x as u32).wrapping_mul(0x9E37_79B1)
        ^ (y as u32).wrapping_mul(0x85EB_CA77)
        ^ (seed as u32).wrapping_mul(0xC2B2_AE3D);
    v ^= v >> 15;
    v = v.wrapping_mul(0x2C1B_3C6D);
    v ^ (v >> 13)
}

fn desktop(seed: usize) -> Vec<u8> {
    let mut bgra = vec![255u8; WIDTH * HEIGHT * 4];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let at = (y * WIDTH + x) * 4;
            let pixel = if (96..288).contains(&x) && (64..192).contains(&y) {
                let n = mix(x, y, seed);
                [
                    (n % 251) as u8,
                    ((n >> 8) % 241) as u8,
                    ((n >> 16) % 239) as u8,
                ]
            } else {
                [
                    (x * 255 / WIDTH) as u8,
                    (y * 255 / HEIGHT) as u8,
                    ((x + y) % 256) as u8,
                ]
            };
            bgra[at..at + 3].copy_from_slice(&pixel);
        }
    }
    bgra
}

async fn send(stream: &mut TcpStream, frame: &VideoFrame, sent: &mut usize) {
    let bytes = frame.write_to_bytes().unwrap();
    stream
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .await
        .unwrap();
    stream.write_all(&bytes).await.unwrap();
    *sent += 4 + bytes.len();
}

struct ServerReport {
    final_image: Vec<u8>,
    bytes_before_quiet: usize,
    bytes_after_converged: usize,
    refinement_frames: usize,
    quiet_started: Instant,
}

async fn controlled_side(mut stream: TcpStream) -> ServerReport {
    let mut encoder = Encoder::new(
        EncoderCfg::GDesk(GDeskEncoderConfig {
            width: WIDTH,
            height: HEIGHT,
        }),
        false,
    )
    .unwrap();
    let mut sent = 0usize;
    let mut image = desktop(0);
    for seed in 0..6 {
        image = desktop(seed);
        let frame = encoder
            .encode_to_message(EncodeInput::YUV(&image), seed as i64)
            .unwrap();
        send(&mut stream, &frame, &mut sent).await;
        if seed < 5 {
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
    }
    let bytes_before_quiet = sent;

    let quiet_started = Instant::now();
    let mut refinement_frames = 0;
    let mut converged_at = None;
    let mut idle_polls = 0;
    for _ in 0..400 {
        if quiet_started.elapsed() >= Duration::from_millis(100) && idle_polls >= 20 {
            break;
        }
        let polled = encoder.poll_refinement(100).unwrap();
        match polled {
            Some(frame) => {
                send(&mut stream, &frame, &mut sent).await;
                refinement_frames += 1;
                idle_polls = 0;
                converged_at = None;
            }
            None => {
                idle_polls += 1;
                if idle_polls == 5 {
                    converged_at = Some(sent);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let converged = converged_at.expect("encoder never converged to lossless");
    let err = encoder
        .encode_to_message(EncodeInput::YUV(&image), 200)
        .err()
        .expect("static frame must produce no output");
    assert!(err.is::<GDeskNoOutput>(), "unexpected error: {:?}", err);
    stream.shutdown().await.unwrap();
    ServerReport {
        final_image: image,
        bytes_before_quiet,
        bytes_after_converged: sent - converged,
        refinement_frames,
        quiet_started,
    }
}

async fn controlling_side(listener: TcpListener) -> (Vec<u8>, usize, Instant) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut decoder = Decoder::new(CodecFormat::GDesk, None);
    assert!(decoder.valid());
    let mut rgb = ImageRgb::new(ImageFormat::ARGB, 1);
    let mut texture = ImageTexture::default();
    let mut frames = 0;
    let expected = desktop(5);
    let mut lossless_at = None;
    loop {
        let mut len = [0u8; 4];
        if stream.read_exact(&mut len).await.is_err() {
            break;
        }
        let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
        stream.read_exact(&mut body).await.unwrap();
        let frame = VideoFrame::parse_from_bytes(&body).unwrap();
        let union = frame.union.as_ref().unwrap();
        assert!(matches!(union, video_frame::Union::Gdesks(_)));
        assert_eq!(CodecFormat::from(&frame), CodecFormat::GDesk);
        let mut pixelbuffer = true;
        let mut chroma = None;
        assert!(decoder
            .handle_video_frame(union, &mut rgb, &mut texture, &mut pixelbuffer, &mut chroma)
            .unwrap());
        if lossless_at.is_none() && rgb.raw == expected {
            lossless_at = Some(Instant::now());
        }
        frames += 1;
    }
    (
        rgb.raw,
        frames,
        lossless_at.expect("receiver never reached max_error == 0"),
    )
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[test]
fn rgba_frame_conversion_and_invalid_stride() {
    let source = [1, 2, 3, 255, 4, 5, 6, 127];
    let mut converted = Vec::new();
    let mut scratch = Vec::new();
    for (src_format, dst_format) in [
        (Pixfmt::RGBA, Pixfmt::BGRA),
        (Pixfmt::BGRA, Pixfmt::RGBA),
        (Pixfmt::RGBA, Pixfmt::RGBA),
        (Pixfmt::BGRA, Pixfmt::BGRA),
    ] {
        let frame = Frame::PixelBuffer(PixelBuffer::new(&source, src_format, 2, 1));
        let mut format = scrap::EncodeYuvFormat {
            pixfmt: dst_format,
            w: 2,
            h: 1,
            stride: vec![12],
            u: 0,
            v: 0,
        };
        let output = frame
            .to(format.clone(), &mut converted, &mut scratch)
            .unwrap();
        let expected = if src_format == dst_format {
            source
        } else {
            [3, 2, 1, 255, 6, 5, 4, 127]
        };
        assert_eq!(&output.yuv().unwrap()[..8], &expected);
        format.stride = vec![7];
        assert!(frame
            .to(format.clone(), &mut converted, &mut scratch)
            .is_err());
        format.stride.clear();
        assert!(frame.to(format, &mut converted, &mut scratch).is_err());
    }
}

#[test]
fn gdesk_loopback_negotiation_lossless_and_silent() {
    let mut decoding = Decoder::supported_decodings(None, false, None, &vec![]);
    assert!(decoding.ability_gdesk > 0);
    assert!(Encoder::supported_encoding().gdesk);
    decoding.prefer = PreferCodec::GDesk.into();
    Encoder::update(EncodingUpdate::Update(9001, decoding));
    assert_eq!(Encoder::negotiated_codec(), CodecFormat::GDesk);
    Encoder::update(EncodingUpdate::NewOnlyVP9(9002));
    assert_ne!(Encoder::negotiated_codec(), CodecFormat::GDesk);
    Encoder::update(EncodingUpdate::Remove(9002));
    assert_eq!(Encoder::negotiated_codec(), CodecFormat::GDesk);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (report, (decoded, frames, lossless_at)) = runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).await.unwrap();
        tokio::join!(controlled_side(stream), controlling_side(listener))
    });

    let mut worst = 0u8;
    assert_eq!(decoded.len(), report.final_image.len());
    for (a, b) in decoded.iter().zip(&report.final_image) {
        worst = worst.max(a.abs_diff(*b));
    }
    println!(
        "GDESK_LOOPBACK frames={frames} bytes_before_quiet={} refinement_frames={} max_error={worst} bytes_after_converged={} convergence_ms={}",
        report.bytes_before_quiet, report.refinement_frames, report.bytes_after_converged, lossless_at.saturating_duration_since(report.quiet_started).as_millis()
    );
    Encoder::update(EncodingUpdate::Remove(9001));
    assert!(
        report.refinement_frames > 0,
        "lossy frames must be refined during the quiet period"
    );
    assert_eq!(worst, 0);
    assert!(
        lossless_at.saturating_duration_since(report.quiet_started) < Duration::from_millis(100),
        "lossless convergence exceeded 100ms: {:?}",
        lossless_at.saturating_duration_since(report.quiet_started)
    );
    assert_eq!(report.bytes_after_converged, 0);
}
