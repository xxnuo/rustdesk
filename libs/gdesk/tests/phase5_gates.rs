use gdesk::{GDeskConfig, GDeskDecoder, GDeskEncoder, PixelFormat, QoSState};
use std::time::Instant;

const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;

#[derive(Debug, Clone)]
pub struct TelemetrySnapshot {
    pub photon_latency_us: u64,
    pub cpu_encode_mean_us: f64,
    pub cpu_encode_p99_us: u64,
    pub cpu_decode_mean_us: f64,
    pub cpu_decode_p99_us: u64,
    pub gpu_encode_us: u64,
    pub gpu_decode_us: u64,
    pub frame_time_p99_jitter_us: u64,
    pub cache_hit_ratio_pct: f64,
    pub qos_state: QoSState,
    pub realtime_kbps: f64,
    pub total_bytes: usize,
    pub max_error: u8,
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

fn mix(x: usize, y: usize, seed: usize) -> u32 {
    let mut v = (x as u32).wrapping_mul(0x9E37_79B1)
        ^ (y as u32).wrapping_mul(0x85EB_CA77)
        ^ (seed as u32).wrapping_mul(0xC2B2_AE3D);
    v ^= v >> 15;
    v = v.wrapping_mul(0x2C1B_3C6D);
    v ^ (v >> 13)
}

fn ide_frame_rgb(seed: usize, cursor_on: bool) -> Vec<u8> {
    let mut rgb = vec![0u8; WIDTH * HEIGHT * 3];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let at = (y * WIDTH + x) * 3;
            let px = if x < 240 {
                // Sidebar with crisp 1px glyph strokes
                if y % 20 == 8 && (24..180).contains(&x) && ((x + seed * 7) % 5 == 0) {
                    [210, 218, 228]
                } else {
                    [30, 34, 42]
                }
            } else if y < 36 {
                // Tab bar
                if (240..560).contains(&x) && seed % 2 == 0 {
                    [45, 52, 68]
                } else {
                    [24, 26, 32]
                }
            } else if y >= HEIGHT - 28 {
                // Status bar
                [0, 122, 204]
            } else if (320..1520).contains(&x) && (80..920).contains(&y) {
                // Code editor pane: 1px syntax-colored text glyphs + subtle background gradient
                let line = (y - 80) / 18;
                let col = (x - 320) / 8;
                let gx = (x - 320) % 8;
                let gy = (y - 80) % 18;
                let h = mix(col, line, seed);
                let is_glyph = (2..14).contains(&gy)
                    && (1..7).contains(&gx)
                    && (h % 7 != 0)
                    && ((gx == 2 && gy % 2 == 0) || (gy == 7 && gx < 6) || (h >> (gx + gy)) & 1 == 1);
                if (640..704).contains(&x) && (256..320).contains(&y) {
                    // Active cursor tile: solid editor background where cursor blinks
                    [24, 26, 32]
                } else if is_glyph {
                    match h % 4 {
                        0 => [86, 156, 214],  // keyword blue
                        1 => [206, 145, 120], // string coral
                        2 => [78, 201, 176],  // type teal
                        _ => [212, 212, 212], // identifier light gray
                    }
                } else {
                    let shade = ((x + y + seed * 3) % 7) as u8;
                    [20 + shade, 22 + shade, 28 + shade]
                }
            } else {
                [26, 29, 36]
            };
            rgb[at..at + 3].copy_from_slice(&px);
        }
    }
    if cursor_on {
        for y in 260..276 {
            for x in 648..650 {
                let at = (y * WIDTH + x) * 3;
                rgb[at..at + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
    }
    rgb
}

fn max_error(a: &[u8], b: &[u8]) -> u8 {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)
}

#[test]
fn phase5_end_to_end_observability_and_scenarios_benchmark() {
    // Scenario 1: 1080p IDE Coding & Cursor Blink (10 frames: 1 initial + 3 refine + 6 cursor blink)
    let config = GDeskConfig {
        initial_shifts: Some((2, 4)),
        refine_min_still_frames: 1,
        max_refine_tiles_per_frame: 256,
        ..Default::default()
    };
    let mut enc = GDeskEncoder::new(WIDTH as u16, HEIGHT as u16, config).unwrap();
    let mut dec = GDeskDecoder::new();

    let base_a = ide_frame_rgb(1, false);
    let blink_a = ide_frame_rgb(1, true);

    let mut enc_us_samples = Vec::new();
    let mut dec_us_samples = Vec::new();
    let mut frame_total_us = Vec::new();
    let mut scenario1_bytes = Vec::new();

    for step in 0..10usize {
        let frame_rgb = if step < 4 {
            &base_a
        } else if step % 2 == 0 {
            &blink_a
        } else {
            &base_a
        };
        let capture_ts_us = (step as u64 + 1) * 16_666;
        let t0 = Instant::now();
        let pkt = enc
            .encode_frame(frame_rgb, WIDTH * 3, PixelFormat::Rgb, capture_ts_us)
            .unwrap();
        let enc_us = t0.elapsed().as_micros() as u64;
        let mut dec_us = 0u64;
        let mut bytes = 0usize;
        if let Some(data) = pkt {
            bytes = data.len();
            let t1 = Instant::now();
            dec.decode_frame(&data).unwrap();
            dec_us = t1.elapsed().as_micros() as u64;
        }
        enc_us_samples.push(enc_us);
        dec_us_samples.push(dec_us);
        frame_total_us.push(enc_us + dec_us);
        scenario1_bytes.push(bytes);
    }
    let s1_err = max_error(
        dec.canvas_rgb(),
        if 9 % 2 == 0 { &blink_a } else { &base_a },
    );
    assert_eq!(s1_err, 0, "Scenario 1 must converge to bit-exact lossless");
    assert!(
        scenario1_bytes[4..].iter().all(|&b| b < 250),
        "cursor blink frames must be < 250 B: {scenario1_bytes:?}"
    );

    // Scenario 2: 1080p Window/Tab Switch A -> B -> A (4096-slot LRU cache hit rate)
    let tab_b = ide_frame_rgb(2, false);
    let hits_before_b = enc.stats.cache_hits;
    let b_init = enc
        .encode_frame(&tab_b, WIDTH * 3, PixelFormat::Rgb, 200_000)
        .unwrap()
        .unwrap();
    dec.decode_frame(&b_init).unwrap();
    while enc.has_pending_refinement() {
        if let Some(pkt) = enc.poll_pending_refinement().unwrap() {
            dec.decode_frame(&pkt).unwrap();
        }
    }
    assert_eq!(max_error(dec.canvas_rgb(), &tab_b), 0);

    let hits_before_switch_back = enc.stats.cache_hits;
    let changed_before_switch_back = enc.stats.changed_tiles;
    let t_sw0 = Instant::now();
    let switch_back_pkt = enc
        .encode_frame(&base_a, WIDTH * 3, PixelFormat::Rgb, 300_000)
        .unwrap()
        .unwrap();
    let sw_enc_us = t_sw0.elapsed().as_micros() as u64;
    let t_sw1 = Instant::now();
    dec.decode_frame(&switch_back_pkt).unwrap();
    let sw_dec_us = t_sw1.elapsed().as_micros() as u64;
    let switch_back_bytes = switch_back_pkt.len();
    let switch_back_hits = enc.stats.cache_hits - hits_before_switch_back;
    let switch_back_changed = enc.stats.changed_tiles - changed_before_switch_back;
    let switch_back_hit_ratio = if switch_back_changed > 0 {
        (switch_back_hits as f64 / switch_back_changed as f64) * 100.0
    } else {
        100.0
    };
    assert_eq!(max_error(dec.canvas_rgb(), &base_a), 0);
    assert!(
        switch_back_hits >= 150,
        "expected high cache hits on A->B->A switch, got {switch_back_hits}/{switch_back_changed}"
    );

    // Scenario 3: Static Desktop Idle (5 frames) -> 0 B bandwidth & Max Error == 0
    let mut idle_bytes = Vec::new();
    for idle_idx in 0..5u64 {
        let pkt = enc
            .encode_frame(&base_a, WIDTH * 3, PixelFormat::Rgb, 400_000 + idle_idx * 16_666)
            .unwrap();
        let b = pkt.as_ref().map_or(0, Vec::len);
        idle_bytes.push(b);
    }
    assert_eq!(idle_bytes, vec![0, 0, 0, 0, 0]);
    assert_eq!(max_error(dec.canvas_rgb(), &base_a), 0);

    // GPU Compute Pipeline measurement (if `gpu` feature is enabled, run real wgpu compute shader)
    #[cfg(feature = "gpu")]
    let (gpu_enc_us, gpu_dec_us) = {
        use gdesk::gpu::{GpuContext, GpuDecoder, GpuEncoder};
        let rgba_a: Vec<u8> = base_a
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect();
        let rgba_b: Vec<u8> = blink_a
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect();
        let ctx = GpuContext::new().unwrap();
        let mut genc = GpuEncoder::new(ctx.clone(), WIDTH as u16, HEIGHT as u16).unwrap();
        let mut gdec = GpuDecoder::new(ctx, &genc).unwrap();
        genc.upload_frame_slot(0, &rgba_a).unwrap();
        genc.upload_frame_slot(1, &rgba_b).unwrap();
        genc.set_quality(0, 0, false);
        for i in 0..6 {
            let s = genc.encode_resident(i % 2).unwrap();
            genc.wait_stats(s).unwrap();
            let ds = gdec.decode(false).unwrap();
            gdec.wait_ns(ds).unwrap();
        }
        let slot = genc.encode_resident(0).unwrap();
        let gstats = genc.wait_stats(slot).unwrap();
        let dslot = gdec.decode(false).unwrap();
        let dec_ns = gdec.wait_ns(dslot).unwrap();
        let gpu_enc_us = ((gstats.scan_ns + gstats.gpu_ns) / 1_000).max(1);
        let gpu_dec_us = (dec_ns / 1_000).max(1);
        enc.stats.gpu_encode_us = gpu_enc_us;
        enc.stats.gpu_decode_us = gpu_dec_us;
        (gpu_enc_us, gpu_dec_us)
    };
    #[cfg(not(feature = "gpu"))]
    let (gpu_enc_us, gpu_dec_us) = (0u64, 0u64);

    let last_header = dec.last_header.unwrap();
    let cpu_enc_mean =
        enc_us_samples.iter().sum::<u64>() as f64 / enc_us_samples.len() as f64;
    let cpu_dec_mean =
        dec_us_samples.iter().sum::<u64>() as f64 / dec_us_samples.len() as f64;
    let cpu_enc_p99 = percentile_u64(&enc_us_samples, 99);
    let cpu_dec_p99 = percentile_u64(&dec_us_samples, 99);
    let jitter_p99 = frame_jitter_p99_us(&frame_total_us);
    // End-to-end photon latency estimate = encode_send_delay_us + decode_us + vsync_scanout_half_period(2083us @ 240Hz)
    let photon_latency_us =
        last_header.encode_send_delay_us as u64 + sw_dec_us + 2_083;
    let s1_total_bytes: usize = scenario1_bytes.iter().sum();
    let duration_s = 10.0 / 60.0;
    let realtime_kbps = (s1_total_bytes as f64 * 8.0 / 1000.0) / duration_s;

    let snapshot = TelemetrySnapshot {
        photon_latency_us,
        cpu_encode_mean_us: cpu_enc_mean,
        cpu_encode_p99_us: cpu_enc_p99,
        cpu_decode_mean_us: cpu_dec_mean,
        cpu_decode_p99_us: cpu_dec_p99,
        gpu_encode_us: gpu_enc_us,
        gpu_decode_us: gpu_dec_us,
        frame_time_p99_jitter_us: jitter_p99,
        cache_hit_ratio_pct: switch_back_hit_ratio,
        qos_state: enc.stats.qos_state,
        realtime_kbps,
        total_bytes: s1_total_bytes,
        max_error: s1_err,
    };

    println!("=== PHASE 5 GDESK TELEMETRY & SCENARIO BENCHMARK ===");
    println!(
        "SCENARIO1_CODING_BLINK frames=10 per_frame_bytes={scenario1_bytes:?} total_bytes={s1_total_bytes} blink_avg_bytes={:.1} max_error={s1_err}",
        scenario1_bytes[4..].iter().sum::<usize>() as f64 / 6.0
    );
    println!(
        "SCENARIO2_TAB_SWITCH b_init_bytes={} switch_back_bytes={switch_back_bytes} cache_hits={switch_back_hits}/{switch_back_changed} ({switch_back_hit_ratio:.2}%) sw_enc_us={sw_enc_us} sw_dec_us={sw_dec_us}",
        b_init.len()
    );
    println!("SCENARIO3_STATIC_IDLE frames=5 per_frame_bytes={idle_bytes:?} max_error=0");
    println!(
        "TELEMETRY_OVERLAY photon_latency_us={} cpu_enc_mean_us={:.1} cpu_enc_p99_us={} cpu_dec_mean_us={:.1} cpu_dec_p99_us={} gpu_enc_us={} gpu_dec_us={} p99_jitter_us={} cache_hit_ratio_pct={:.2}% qos={:?} realtime_kbps={:.1}",
        snapshot.photon_latency_us,
        snapshot.cpu_encode_mean_us,
        snapshot.cpu_encode_p99_us,
        snapshot.cpu_decode_mean_us,
        snapshot.cpu_decode_p99_us,
        snapshot.gpu_encode_us,
        snapshot.gpu_decode_us,
        snapshot.frame_time_p99_jitter_us,
        snapshot.cache_hit_ratio_pct,
        snapshot.qos_state,
        snapshot.realtime_kbps,
    );

    // Verify UDP datagram path also populates `encode_send_delay_us` in header.
    let udp_datagrams = enc
        .encode_udp_datagrams(&tab_b, WIDTH * 3, PixelFormat::Rgb, 500_000)
        .unwrap();
    assert!(!udp_datagrams.is_empty());
    let (udp_hdr, _) = gdesk::udp::unpack_udp_datagram(&udp_datagrams[0]).unwrap();
    assert!(
        udp_hdr.encode_send_delay_us > 0,
        "UDP datagram header must populate encode_send_delay_us"
    );
    let _ = hits_before_b;
}
