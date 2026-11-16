#![cfg(feature = "gdesk")]

use hbb_common::protobuf::Message;
use scrap::{
    aom::AomEncoderConfig,
    codec::{Decoder, Encoder, EncoderCfg},
    gdesk_codec::{GDeskEncoderConfig, GDeskNoOutput},
    vpxcodec::{VpxEncoderConfig, VpxNoOutput, VpxVideoCodecId},
    CodecFormat, Frame, ImageFormat, ImageRgb, ImageTexture, PixelBuffer, Pixfmt,
};
use std::time::Instant;

const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;

fn mix(x: usize, y: usize, seed: usize) -> u32 {
    let mut v = (x as u32).wrapping_mul(0x9E37_79B1)
        ^ (y as u32).wrapping_mul(0x85EB_CA77)
        ^ (seed as u32).wrapping_mul(0xC2B2_AE3D);
    v ^= v >> 15;
    v = v.wrapping_mul(0x2C1B_3C6D);
    v ^ (v >> 13)
}

/// Generates a 1080p synthetic IDE frame in BGRA format with sharp 1px syntax-colored glyphs,
/// UI chrome, and an optional blinking text cursor.
fn ide_frame_bgra(seed: usize, cursor_on: bool) -> Vec<u8> {
    let mut bgra = vec![255u8; WIDTH * HEIGHT * 4];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let at = (y * WIDTH + x) * 4;
            let rgb = if x < 240 {
                if y % 20 == 8 && (24..180).contains(&x) && ((x + seed * 7) % 5 == 0) {
                    [210u8, 218, 228]
                } else {
                    [30, 34, 42]
                }
            } else if y < 36 {
                if (240..560).contains(&x) && seed % 2 == 0 {
                    [45, 52, 68]
                } else {
                    [24, 26, 32]
                }
            } else if y >= HEIGHT - 28 {
                [0, 122, 204]
            } else if (320..1520).contains(&x) && (80..920).contains(&y) {
                if (640..704).contains(&x) && (256..320).contains(&y) {
                    [24, 26, 32]
                } else {
                    let line = (y - 80) / 18;
                    let col = (x - 320) / 8;
                    let gx = (x - 320) % 8;
                    let gy = (y - 80) % 18;
                    let h = mix(col, line, seed);
                    let is_glyph = (2..14).contains(&gy)
                        && (1..7).contains(&gx)
                        && (h % 7 != 0)
                        && ((gx == 2 && gy % 2 == 0)
                            || (gy == 7 && gx < 6)
                            || (h >> (gx + gy)) & 1 == 1);
                    if is_glyph {
                        match h % 4 {
                            0 => [255, 60, 60],   // high-chroma red keyword (sensitive to 4:2:0 subsampling)
                            1 => [60, 255, 120],  // high-chroma green string
                            2 => [80, 160, 255],  // bright blue type
                            _ => [235, 235, 235], // crisp white identifier
                        }
                    } else {
                        let shade = ((x + y + seed * 3) % 7) as u8;
                        [20 + shade, 22 + shade, 28 + shade]
                    }
                }
            } else {
                [26, 29, 36]
            };
            // BGRA byte order
            bgra[at] = rgb[2];
            bgra[at + 1] = rgb[1];
            bgra[at + 2] = rgb[0];
            bgra[at + 3] = 255;
        }
    }
    if cursor_on {
        for y in 260..276 {
            for x in 648..650 {
                let at = (y * WIDTH + x) * 4;
                bgra[at..at + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    bgra
}

fn percentile_u64(samples: &[u64], pct: usize) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let idx = ((sorted.len() - 1) * pct) / 100;
    sorted[idx]
}

fn frame_jitter_p99_us(frame_times_us: &[u64]) -> u64 {
    if frame_times_us.len() < 2 {
        return 0;
    }
    let deltas: Vec<u64> = frame_times_us
        .windows(2)
        .map(|w| w[0].abs_diff(w[1]))
        .collect();
    percentile_u64(&deltas, 99)
}

fn max_error_and_psnr(decoded_bgra: &[u8], truth_bgra: &[u8]) -> (u8, f64) {
    assert_eq!(decoded_bgra.len(), truth_bgra.len());
    let mut worst = 0u8;
    let mut sq_sum = 0u64;
    let mut count = 0u64;
    for (a, b) in decoded_bgra.chunks_exact(4).zip(truth_bgra.chunks_exact(4)) {
        for c in 0..3 {
            let diff = a[c].abs_diff(b[c]);
            worst = worst.max(diff);
            sq_sum += (diff as u64) * (diff as u64);
            count += 1;
        }
    }
    if sq_sum == 0 {
        return (0, f64::INFINITY);
    }
    let mse = sq_sum as f64 / count as f64;
    let psnr = 10.0 * ((255.0 * 255.0) / mse).log10();
    (worst, psnr)
}

#[derive(Debug, Clone)]
struct CodecBenchmarkResult {
    name: &'static str,
    enc_mean_us: f64,
    enc_p99_us: u64,
    dec_mean_us: f64,
    dec_p99_us: u64,
    jitter_p99_us: u64,
    total_bytes: usize,
    switch_back_bytes: usize,
    static_4frames_bytes: usize,
    max_error: u8,
    psnr_db: f64,
    cache_hit_ratio_pct: Option<f64>,
}

fn run_codec_workload(
    name: &'static str,
    cfg: EncoderCfg,
    format: CodecFormat,
    frames: &[Vec<u8>],
) -> CodecBenchmarkResult {
    let mut encoder = Encoder::new(cfg, false).expect("create encoder");
    let mut decoder = Decoder::new(format, None);
    let mut yuv = Vec::new();
    let mut scratch = Vec::new();
    let mut rgb_out = ImageRgb::new(ImageFormat::ARGB, 1);
    let mut texture = ImageTexture::default();

    let mut enc_times_us = Vec::new();
    let mut dec_times_us = Vec::new();
    let mut frame_times_us = Vec::new();
    let mut frame_sizes = Vec::new();
    let mut measured_switch_back_hit_ratio = None;

    for (idx, bgra) in frames.iter().enumerate() {
        let pb = Frame::PixelBuffer(PixelBuffer::new(bgra, Pixfmt::BGRA, WIDTH, HEIGHT));
        let t_enc = Instant::now();
        let input = pb.to(encoder.yuvfmt(), &mut yuv, &mut scratch).unwrap();
        let pts_ms = (idx as i64) * 100;
        let encoded_res = encoder.encode_to_message(input, pts_ms);
        let vf_opt = match encoded_res {
            Ok(vf) => Some(vf),
            Err(err) if err.is::<GDeskNoOutput>() => encoder.poll_refinement(pts_ms).unwrap(),
            Err(err) if err.is::<VpxNoOutput>() || err.to_string().contains("no valid frame") => None,
            Err(err) => panic!("{} encode failed at frame {}: {}", name, idx, err),
        };
        let mut enc_us = t_enc.elapsed().as_micros() as u64;

        let mut dec_us = 0u64;
        let mut bytes = 0usize;
        if let Some(vf) = vf_opt {
            bytes = vf.write_to_bytes().unwrap().len();
            if encoder.is_gdesk() && idx == 9 {
                let raw_frame = &vf.gdesks().frames[0].data;
                let (_, records) = gdesk::protocol::decode_frame(raw_frame).unwrap();
                let hits = records
                    .iter()
                    .filter(|r| matches!(r, gdesk::protocol::TileRecord::CacheHit { .. }))
                    .count();
                assert!(!records.is_empty(), "switch-back frame must contain tile records");
                measured_switch_back_hit_ratio =
                    Some((hits as f64 / records.len() as f64) * 100.0);
            }
            if let Some(union) = vf.union.as_ref() {
                let t_dec = Instant::now();
                let mut pixelbuffer = true;
                let mut chroma = None;
                let ok = decoder
                    .handle_video_frame(union, &mut rgb_out, &mut texture, &mut pixelbuffer, &mut chroma)
                    .unwrap();
                assert!(ok, "{} decoder did not produce frame at {}", name, idx);
                dec_us = t_dec.elapsed().as_micros() as u64;
            }
        }
        // For GDesk on window switch frame (step 8 = Tab B), drain remaining refinements so Tab B is settled
        // before switching back to Tab A at step 9, and include their encode/decode microseconds.
        if encoder.is_gdesk() && idx == 8 {
            while encoder.has_gdesk_pending_refinement() {
                let t_ref_enc = Instant::now();
                let polled = encoder.poll_refinement(pts_ms).unwrap();
                enc_us += t_ref_enc.elapsed().as_micros() as u64;
                if let Some(vf) = polled {
                    bytes += vf.write_to_bytes().unwrap().len();
                    if let Some(union) = vf.union.as_ref() {
                        let t_ref_dec = Instant::now();
                        let mut pixelbuffer = true;
                        let mut chroma = None;
                        decoder
                            .handle_video_frame(
                                union,
                                &mut rgb_out,
                                &mut texture,
                                &mut pixelbuffer,
                                &mut chroma,
                            )
                            .unwrap();
                        dec_us += t_ref_dec.elapsed().as_micros() as u64;
                    }
                }
            }
        }

        enc_times_us.push(enc_us);
        dec_times_us.push(dec_us);
        frame_times_us.push(enc_us + dec_us);
        frame_sizes.push(bytes);
    }

    let (max_error, psnr_db) = max_error_and_psnr(&rgb_out.raw, frames.last().unwrap());
    let enc_mean_us = enc_times_us.iter().sum::<u64>() as f64 / enc_times_us.len() as f64;
    let dec_mean_us = dec_times_us.iter().sum::<u64>() as f64 / dec_times_us.len() as f64;
    let enc_p99_us = percentile_u64(&enc_times_us, 99);
    let dec_p99_us = percentile_u64(&dec_times_us, 99);
    let jitter_p99_us = frame_jitter_p99_us(&frame_times_us);
    let total_bytes: usize = frame_sizes.iter().sum();
    let switch_back_bytes = frame_sizes[9];
    let static_4frames_bytes: usize = frame_sizes[10..14].iter().sum();

    println!(
        "CODEC_DETAIL name={name} per_frame_bytes={frame_sizes:?} switch_back_bytes={switch_back_bytes} static_4frames_bytes={static_4frames_bytes} max_error={max_error} psnr={psnr_db:.2}"
    );

    CodecBenchmarkResult {
        name,
        enc_mean_us,
        enc_p99_us,
        dec_mean_us,
        dec_p99_us,
        jitter_p99_us,
        total_bytes,
        switch_back_bytes,
        static_4frames_bytes,
        max_error,
        psnr_db,
        cache_hit_ratio_pct: measured_switch_back_hit_ratio,
    }
}

#[test]
fn gdesk_vs_vp9_av1_h265_phase5_benchmark() {
    let tab_a = ide_frame_bgra(1, false);
    let tab_a_blink = ide_frame_bgra(1, true);
    let tab_b = ide_frame_bgra(2, false);

    // 14-frame canonical desktop sequence:
    // [0..4]: Initial Tab A + 3 frames to settle progressive lossless refinement
    // [4..8]: 4 cursor blink frames on Tab A
    // [8]: Switch to Tab B
    // [9]: Switch back to Tab A (tests 4096-slot LRU cache hit vs inter-frame motion estimation)
    // [10..14]: 4 static idle frames on Tab A (tests zero-bandwidth steady state)
    let sequence = vec![
        tab_a.clone(),
        tab_a.clone(),
        tab_a.clone(),
        tab_a.clone(),
        tab_a_blink.clone(),
        tab_a.clone(),
        tab_a_blink.clone(),
        tab_a.clone(),
        tab_b.clone(),
        tab_a.clone(),
        tab_a.clone(),
        tab_a.clone(),
        tab_a.clone(),
        tab_a.clone(),
    ];

    let gdesk_res = run_codec_workload(
        "GDesk",
        EncoderCfg::GDesk(GDeskEncoderConfig {
            width: WIDTH,
            height: HEIGHT,
        }),
        CodecFormat::GDesk,
        &sequence,
    );

    let vp9_res = run_codec_workload(
        "VP9 (I420)",
        EncoderCfg::VPX(VpxEncoderConfig {
            width: WIDTH as _,
            height: HEIGHT as _,
            quality: 2.0,
            codec: VpxVideoCodecId::VP9,
            keyframe_interval: None,
        }),
        CodecFormat::VP9,
        &sequence,
    );

    let av1_res = run_codec_workload(
        "AV1 (AOM I420)",
        EncoderCfg::AOM(AomEncoderConfig {
            width: WIDTH as _,
            height: HEIGHT as _,
            quality: 2.0,
            keyframe_interval: None,
        }),
        CodecFormat::AV1,
        &sequence,
    );

    let supported_enc = Encoder::supported_encoding();
    println!(
        "HOST_CODEC_CAPABILITIES gdesk={} vp9=true av1={} h264={} h265={}",
        supported_enc.gdesk, supported_enc.av1, supported_enc.h264, supported_enc.h265
    );

    println!("\n### Cross-Codec 1080p Benchmark Summary (GDesk vs VP9 vs AV1 vs H.265)\n");
    println!(
        "| Codec | Mean Enc (µs) | P99 Enc (µs) | Mean Dec (µs) | P99 Dec (µs) | P99 Jitter (µs) | Total Bytes | Switch-Back (B) | Static 4-Frame (B) | Max Error | PSNR (dB) | LRU Hit Ratio |"
    );
    println!(
        "| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    for r in [&gdesk_res, &vp9_res, &av1_res] {
        let psnr_str = if r.psnr_db.is_infinite() {
            "INF (Lossless)".to_string()
        } else {
            format!("{:.2}", r.psnr_db)
        };
        let hit_str = r
            .cache_hit_ratio_pct
            .map_or("N/A".to_string(), |v| format!("{v:.1}%"));
        println!(
            "| {} | {:.1} | {} | {:.1} | {} | {} | {} | {} | {} | {} | {} | {} |",
            r.name,
            r.enc_mean_us,
            r.enc_p99_us,
            r.dec_mean_us,
            r.dec_p99_us,
            r.jitter_p99_us,
            r.total_bytes,
            r.switch_back_bytes,
            r.static_4frames_bytes,
            r.max_error,
            psnr_str,
            hit_str
        );
    }
    println!(
        "| H.265 (HEVC HW) | N/A (`h265={}`, `hwcodec` disabled) | N/A | N/A | N/A | N/A | N/A | N/A | N/A | N/A (unmeasured) | N/A | N/A |",
        supported_enc.h265
    );

    assert_eq!(gdesk_res.max_error, 0, "GDesk must achieve bit-exact Max Error == 0");
    assert_eq!(
        gdesk_res.static_4frames_bytes, 0,
        "GDesk must emit 0 bytes on static frames after convergence"
    );
    assert!(
        vp9_res.max_error > 0 && av1_res.max_error > 0,
        "YUV420 VP9/AV1 cannot preserve 1px RGB syntax text without chroma loss"
    );
    assert!(
        gdesk_res.switch_back_bytes < vp9_res.switch_back_bytes
            && gdesk_res.switch_back_bytes < av1_res.switch_back_bytes,
        "GDesk LRU cache switch-back ({} B) must beat VP9 ({} B) and AV1 ({} B)",
        gdesk_res.switch_back_bytes,
        vp9_res.switch_back_bytes,
        av1_res.switch_back_bytes
    );
}
