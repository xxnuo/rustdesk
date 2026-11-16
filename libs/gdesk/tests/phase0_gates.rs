use gdesk::{GDeskConfig, GDeskDecoder, GDeskEncoder, PixelFormat};

fn hash2(x: u32, y: u32, seed: u32) -> u32 {
    let mut v = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77) ^ seed.wrapping_mul(0xC2B2_AE3D);
    v ^= v >> 15;
    v = v.wrapping_mul(0x2C1B_3C6D);
    v ^= v >> 12;
    v = v.wrapping_mul(0x297A_2D39);
    v ^ (v >> 15)
}

fn set(img: &mut [u8], width: usize, x: usize, y: usize, rgb: [u8; 3]) {
    let at = (y * width + x) * 3;
    img[at..at + 3].copy_from_slice(&rgb);
}

fn fill(img: &mut [u8], width: usize, x0: usize, y0: usize, w: usize, h: usize, rgb: [u8; 3]) {
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            set(img, width, x, y, rgb);
        }
    }
}

fn synthetic(width: usize, height: usize, seed: u32) -> Vec<u8> {
    let mut img = vec![255u8; width * height * 3];
    for y in 0..height {
        for x in 0..width {
            let (tx, ty) = (x / 64, y / 64);
            let kind = hash2(tx as u32, ty as u32, seed) % 5;
            let rgb = match kind {
                0 => [(x * 255 / width) as u8, (y * 255 / height) as u8, ((x + y + seed as usize) % 256) as u8],
                1 => {
                    if hash2((x / 2) as u32, (y / 3) as u32, seed) % 3 == 0 { [255, 255, 255] } else { [0, 0, 0] }
                }
                2 => [(30 + hash2(tx as u32, ty as u32, seed) % 200) as u8; 3],
                3 => {
                    let n = hash2(x as u32, y as u32, seed);
                    [(x as u32 * 3 + n % 9) as u8, (y as u32 * 2 + (n >> 8) % 9) as u8, ((n >> 16) as u8 % 16).wrapping_add((tx * 7) as u8)]
                }
                _ => {
                    let edge = ((x * 3 + y * 5 + seed as usize) % 64) as u8;
                    [edge.wrapping_mul(4), 255 - edge.wrapping_mul(2), edge]
                }
            };
            set(&mut img, width, x, y, rgb);
        }
    }
    img
}

fn decode(dec: &mut GDeskDecoder, frame: &[u8]) {
    dec.decode_frame(frame).expect("decode");
}

fn max_error(a: &[u8], b: &[u8]) -> u8 {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(l, r)| l.abs_diff(*r)).max().unwrap_or(0)
}

fn feed(enc: &mut GDeskEncoder, dec: &mut GDeskDecoder, img: &[u8], ts: u64) -> usize {
    let mut bytes = 0;
    if let Some(frame) = enc.encode_frame(img, enc.dimensions().0 as usize * 3, PixelFormat::Rgb, ts).unwrap() {
        bytes += frame.len();
        decode(dec, &frame);
    }
    bytes
}

#[test]
fn gate1_mathematical_bit_exact_lossless() {
    let (w, h) = (1920usize, 1080usize);
    let mut img = synthetic(w, h, 1);
    fill(&mut img, w, 0, 0, 640, 128, [0, 0, 0]);
    for row in 0..16 {
        for col in 0..60 {
            if hash2(col, row, 9) % 2 == 0 {
                set(&mut img, w, 8 + col as usize * 3, 8 + row as usize * 8, [255, 255, 255]);
            }
        }
    }
    let config = GDeskConfig { initial_shifts: Some((3, 5)), refine_min_still_frames: 1, ..Default::default() };
    let mut enc = GDeskEncoder::new(w as u16, h as u16, config).unwrap();
    let mut dec = GDeskDecoder::new();
    let first = feed(&mut enc, &mut dec, &img, 0);
    assert!(first > 0);
    assert!(enc.has_pending_refinement());
    assert!(max_error(dec.canvas_rgb(), &img) > 0, "frame 1 must be lossy");
    let second = feed(&mut enc, &mut dec, &img, 1);
    assert!(second > 0);
    assert!(!enc.has_pending_refinement(), "frame 2 must finish the upgrade");
    let err = max_error(dec.canvas_rgb(), &img);
    println!("GATE1 total_bytes={} first_frame={first} upgrade_frame={second} max_error={err}", first + second);
    assert_eq!(err, 0);
    assert!(dec.is_lossless());
}

#[test]
fn gate2_static_zero_bandwidth_and_cursor_isolation() {
    let (w, h) = (1920usize, 1080usize);
    let mut base = synthetic(w, h, 2);
    fill(&mut base, w, 320, 320, 64, 64, [255, 255, 255]);
    let config = GDeskConfig { initial_shifts: Some((2, 4)), refine_min_still_frames: 1, ..Default::default() };
    let mut enc = GDeskEncoder::new(w as u16, h as u16, config).unwrap();
    let mut dec = GDeskDecoder::new();
    let mut sizes = Vec::new();
    for frame in 0..10 {
        let mut img = base.clone();
        if frame % 2 == 0 {
            fill(&mut img, w, 340, 340, 8, 16, [0, 0, 0]);
        }
        sizes.push(feed(&mut enc, &mut dec, &img, frame as u64));
        if frame == 0 {
            assert!(max_error(dec.canvas_rgb(), &img) > 0, "frame 0 must be lossy");
        } else {
            assert_eq!(max_error(dec.canvas_rgb(), &img), 0, "frame {frame}");
        }
        if frame == 2 {
            assert!(!enc.has_pending_refinement());
            assert!((0..510).all(|tile| dec.tile_is_lossless(tile)), "all 510 tiles lossless by frame 3");
        }
    }
    println!("GATE2 sizes={sizes:?}");
    assert!(sizes[3..].iter().all(|size| *size < 200), "{sizes:?}");
    let steady = base.clone();
    assert!(feed(&mut enc, &mut dec, &steady, 100) < 200);
    assert_eq!(feed(&mut enc, &mut dec, &steady, 101), 0);
    assert_eq!(feed(&mut enc, &mut dec, &steady, 102), 0);
}

#[test]
fn gate3_alt_tab_4k_cache_instant_switch() {
    let (w, h) = (3840usize, 2160usize);
    let a = synthetic(w, h, 3);
    let b = synthetic(w, h, 4);
    let mut enc = GDeskEncoder::new(w as u16, h as u16, GDeskConfig::default()).unwrap();
    let mut dec = GDeskDecoder::new();
    let a_bytes = feed(&mut enc, &mut dec, &a, 0);
    let b_bytes = feed(&mut enc, &mut dec, &b, 1);
    let back = feed(&mut enc, &mut dec, &a, 2);
    let err = max_error(dec.canvas_rgb(), &a);
    println!("GATE3 a={a_bytes} b={b_bytes} switch_back={back} max_error={err}");
    assert!(back <= 15_360, "{back}");
    assert_eq!(err, 0);
}

#[cfg(feature = "gpu")]
#[test]
fn gate4_gpu_1080p_benchmark() {
    #[cfg(feature = "gpu")]
    use gdesk::{
        gpu::{GpuContext, GpuDecoder, GpuEncoder},
        math,
    };
    let (w, h) = (1920usize, 1080usize);
    let to_rgba = |rgb: &[u8]| -> Vec<u8> {
        rgb.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect()
    };
    let frame_a = to_rgba(&synthetic(w, h, 5));
    let mut frame_b = frame_a.clone();
    for tile in 0..102usize {
        let (tx, ty) = (tile % 30, tile / 30);
        for y in ty * 64..ty * 64 + 64 {
            for x in tx * 64..tx * 64 + 64 {
                let n = hash2(x as u32, y as u32, 77);
                let at = (y * w + x) * 4;
                frame_b[at..at + 3].copy_from_slice(&[(x as u32 + n % 5) as u8, (y as u32 + (n >> 8) % 5) as u8, (n >> 16) as u8]);
            }
        }
    }
    let ctx = GpuContext::new().expect("GPU adapter required for gate 4");
    println!("GATE4 adapter={} backend={} timestamps={}", ctx.adapter_name, ctx.backend, ctx.timestamps);
    let mut enc = GpuEncoder::new(ctx.clone(), w as u16, h as u16).unwrap();
    let mut dec = GpuDecoder::new(ctx.clone(), &enc).unwrap();
    enc.upload_frame_slot(0, &frame_a).unwrap();
    enc.upload_frame_slot(1, &frame_b).unwrap();
    let canvas_error = |canvas: &[u8], frame: &[u8]| {
        canvas.chunks_exact(4).zip(frame.chunks_exact(4)).map(|(a, b)| (0..3).map(|c| a[c].abs_diff(b[c])).max().unwrap_or(0)).max().unwrap_or(0)
    };
    let coeff_error = |tiles: Vec<gdesk::gpu::GpuTile>, shifts: (u32, u32)| {
        let mut worst = 0i32;
        for tile in tiles {
            let cpu_tile = math::extract_tile(&frame_a, w, h, w * 4, PixelFormat::Rgba, tile.tile as usize).unwrap();
            if let Some(color) = tile.solid {
                assert_eq!(math::detect_solid(&cpu_tile), Some(color));
                continue;
            }
            let cpu = math::rgb_to_wavelet(&cpu_tile).unwrap();
            for plane in 0..3 {
                let shift = if plane == 0 { shifts.0 } else { shifts.1 };
                for (i, (gpu, cpu)) in tile.planes[plane].iter().zip(&cpu.planes[plane]).enumerate() {
                    let cpu = *cpu as i32;
                    let expected = if shift == 0 || (i / 64 < 8 && i % 64 < 8) {
                        cpu
                    } else {
                        let kept = ((cpu.unsigned_abs() >> shift) << shift) as i32;
                        if cpu < 0 { -kept } else { kept }
                    };
                    worst = worst.max((gpu - expected).abs());
                }
            }
        }
        worst
    };

    enc.set_quality(3, 5, false);
    let slot = enc.encode_resident(0).unwrap();
    let stats = enc.wait_stats(slot).unwrap();
    assert_eq!(stats.dirty_tiles, 510);
    let dslot = dec.decode(true).unwrap();
    dec.wait_ns(dslot).unwrap();
    let lossy_coeff_error = coeff_error(enc.read_tiles(stats.dirty_tiles).unwrap(), (3, 5));
    let lossy_error = canvas_error(&dec.read_canvas().unwrap(), &frame_a);
    assert!(lossy_error > 0, "truncated frame must be lossy");

    enc.set_quality(0, 0, true);
    let slot = enc.encode_resident(0).unwrap();
    let stats = enc.wait_stats(slot).unwrap();
    assert_eq!(stats.dirty_tiles, 510);
    let dslot = dec.decode(false).unwrap();
    dec.wait_ns(dslot).unwrap();
    let upgrade_coeff_error = coeff_error(enc.read_tiles(stats.dirty_tiles).unwrap(), (0, 0));
    let first_error = canvas_error(&dec.read_canvas().unwrap(), &frame_a);
    assert_eq!(dec.present(0.0).unwrap(), dec.read_canvas().unwrap(), "present must not alter a lossless canvas");

    enc.set_quality(0, 0, false);
    for i in 0..151 {
        let slot = enc.encode_resident((i + 1) % 2).unwrap();
        enc.wait_stats(slot).unwrap();
        let dslot = dec.decode(false).unwrap();
        dec.wait_ns(dslot).unwrap();
    }
    let mut last = &frame_a;
    let iters = 100usize;
    let mut enc_in_flight = std::collections::VecDeque::new();
    let mut dec_in_flight = std::collections::VecDeque::new();
    let mut samples: Vec<[u64; 3]> = Vec::new();
    for i in 0..iters + 2 {
        if i < iters {
            last = if i % 2 == 0 { &frame_a } else { &frame_b };
            let slot = enc.encode_resident(i % 2).unwrap();
            enc_in_flight.push_back(slot);
        }
        if enc_in_flight.len() > 2 || (i >= iters && !enc_in_flight.is_empty()) {
            let slot = enc_in_flight.pop_front().unwrap();
            let stats = enc.wait_stats(slot).unwrap();
            assert_eq!(stats.dirty_tiles, 102);
            samples.push([stats.scan_ns, stats.gpu_ns, 0]);
        }
    }
    let mut dec_idx = 0usize;
    for i in 0..iters + 2 {
        if i < iters {
            let dslot = dec.decode(false).unwrap();
            dec_in_flight.push_back(dslot);
        }
        if dec_in_flight.len() > 2 || (i >= iters && !dec_in_flight.is_empty()) {
            let dslot = dec_in_flight.pop_front().unwrap();
            let decode = dec.wait_ns(dslot).unwrap();
            samples[dec_idx][2] = decode;
            dec_idx += 1;
        }
    }
    let final_error = canvas_error(&dec.read_canvas().unwrap(), last);
    assert!(dec.wavelet_state().unwrap().iter().any(|v| *v != 0));
    let median = |column: usize| { let mut v: Vec<u64> = samples.iter().map(|s| s[column]).collect(); v.sort(); v[v.len() / 2] as f64 / 1e6 };
    let trimmed_avg_ms = |column: usize| {
        let mut v: Vec<u64> = samples.iter().map(|s| s[column]).collect();
        v.sort_unstable();
        let keep = (v.len() * 80) / 100;
        v[..keep].iter().sum::<u64>() as f64 / (keep as f64 * 1e6)
    };
    println!("GATE4 median_ms scan={:.4} encode={:.4} decode={:.4}", median(0), median(1), median(2));
    let (scan_ms, enc_ms, dec_ms) = (trimmed_avg_ms(0), trimmed_avg_ms(1), trimmed_avg_ms(2));
    println!("GATE4 scan_avg_ms={scan_ms:.4}");
    println!("GATE4 encode_avg_ms={enc_ms:.4} decode_avg_ms={dec_ms:.4} lossy_coeff_error={lossy_coeff_error} coeff_max_error={upgrade_coeff_error} first_frame_error={first_error} final_error={final_error}");
    assert_eq!(lossy_coeff_error, 0);
    assert_eq!(upgrade_coeff_error, 0);
    assert_eq!(first_error, 0);
    assert_eq!(final_error, 0);
    assert!(enc_ms < 1.0, "encode {enc_ms} ms");
    assert!(dec_ms < 1.0, "decode {dec_ms} ms");
}
