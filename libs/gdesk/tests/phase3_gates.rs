#![cfg(feature = "gpu")]

use gdesk::{
    gpu::{
        wgpu as gdesk_wgpu,
        vram::{GDeskVramDecoder, GDeskVramEncoder, VramTextureHandle},
        GpuContext, GpuDecoder,
    },
    protocol::{CACHE_KEEP, KEYFRAME_RESET},
    GDeskConfig, GDeskDecoder, GDeskEncoder, GDeskFrameHeader, PixelFormat,
};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

static TIMING: Mutex<()> = Mutex::new(());

fn hash(x: usize, y: usize, seed: usize) -> u32 {
    let mut v = (x as u32).wrapping_mul(0x9E37_79B1) ^ (y as u32).wrapping_mul(0x85EB_CA77) ^ (seed as u32).wrapping_mul(0xC2B2_AE3D);
    v ^= v >> 15;
    v = v.wrapping_mul(0x2C1B_3C6D);
    v ^= v >> 12;
    v = v.wrapping_mul(0x297A_2D39);
    v ^ (v >> 15)
}

#[cfg(not(unix))]
fn thread_cpu() -> Duration {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

#[cfg(unix)]
fn thread_cpu() -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

fn rgb_error(rgba_or_rgb: &[u8], stride: usize, frame_rgba: &[u8]) -> u8 {
    assert_eq!(rgba_or_rgb.len() / stride, frame_rgba.len() / 4);
    rgba_or_rgb
        .chunks_exact(stride)
        .zip(frame_rgba.chunks_exact(4))
        .map(|(a, b)| (0..3).map(|c| a[c].abs_diff(b[c])).max().unwrap_or(0))
        .max()
        .unwrap_or(0)
}

fn document_pixel(x: usize, y: usize, width: usize) -> [u8; 3] {
    let n = hash(x, y, 1);
    match (y / 256) % 4 {
        0 => [
            ((x / 4 + (n % 7) as usize) % 256) as u8,
            ((y / 3 + ((n >> 8) % 7) as usize) % 256) as u8,
            (((x + y) / 5 + ((n >> 16) % 7) as usize) % 256) as u8,
        ],
        1 => {
            if hash(x / 3, y / 4, 2) % 3 == 0 { [20, 20, 24] } else { [245, 245, 240] }
        }
        2 => [40 + ((y / 256) % 5) as u8 * 30, 90, 160],
        _ => [(x * 255 / width) as u8, (128 + (y % 64)) as u8, (255 - x * 255 / width) as u8],
    }
}

fn scroll_frames(width: usize, height: usize, count: usize, step: usize) -> Vec<Vec<u8>> {
    let (pane_x, pane_y) = (width / 4, height / 4);
    let (pane_w, pane_h) = (width / 2, height / 2);
    let mut background = vec![255u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let at = (y * width + x) * 4;
            let pixel = if y + 64 >= height || x < 128 {
                [36, 40, 52]
            } else {
                document_pixel(x, y + 4096, width)
            };
            background[at..at + 3].copy_from_slice(&pixel);
        }
    }
    (0..count)
        .map(|k| {
            let mut frame = background.clone();
            for y in 0..pane_h {
                for x in 0..pane_w {
                    let at = ((pane_y + y) * width + pane_x + x) * 4;
                    frame[at..at + 3].copy_from_slice(&document_pixel(x, y + k * step, width));
                }
            }
            frame
        })
        .collect()
}

#[test]
fn gate1_4k_60hz_scroll_vram_cpu_reduction_over_50_percent() {
    let _serial = TIMING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (width, height, count) = (3840usize, 2160usize, 6usize);
    let frames = scroll_frames(width, height, count, 8);
    let config = GDeskConfig { initial_shifts: Some((0, 0)), ..Default::default() };
    let mut cpu_encoder = GDeskEncoder::new(width as u16, height as u16, config).unwrap();
    let mut cpu_decoder = GDeskDecoder::new();
    let (mut cpu_enc, mut cpu_dec, mut cpu_bytes) = (Duration::ZERO, Duration::ZERO, 0usize);
    for (index, frame) in frames.iter().enumerate() {
        let started = thread_cpu();
        let packet = cpu_encoder
            .encode_frame(frame, width * 4, PixelFormat::Rgba, index as u64 * 16_667)
            .unwrap()
            .unwrap();
        let encoded = thread_cpu();
        cpu_decoder.decode_frame(&packet).unwrap();
        let decoded = thread_cpu();
        if index > 0 {
            cpu_enc += encoded - started;
            cpu_dec += decoded - encoded;
            cpu_bytes += packet.len();
        }
    }
    assert_eq!(rgb_error(cpu_decoder.canvas_rgb(), 3, frames.last().unwrap()), 0);

    let ctx = GpuContext::new().expect("GPU adapter required");
    let mut vram_encoder = GDeskVramEncoder::new(ctx.clone(), width as u16, height as u16).unwrap();
    let ids: Vec<usize> = frames
        .iter()
        .map(|frame| {
            vram_encoder
                .register(VramTextureHandle::WgpuBuffer(ctx.create_frame_buffer(frame)))
                .unwrap()
        })
        .collect();
    let mut vram_decoder = GDeskVramDecoder::new(ctx);
    let (mut gpu_enc, mut gpu_dec, mut gpu_bytes) = (Duration::ZERO, Duration::ZERO, 0usize);
    for (index, id) in ids.iter().enumerate() {
        let started = thread_cpu();
        let packet = vram_encoder.encode(*id).unwrap().unwrap();
        let encoded = thread_cpu();
        vram_decoder.decode(&packet).unwrap();
        let decoded = thread_cpu();
        if index > 0 {
            gpu_enc += encoded - started;
            gpu_dec += decoded - encoded;
            gpu_bytes += packet.len();
        }
    }
    let canvas = vram_decoder.canvas().unwrap();
    assert_eq!(rgb_error(&canvas, 4, frames.last().unwrap()), 0);

    let measured = (count - 1) as f64;
    let cpu_total = (cpu_enc + cpu_dec).as_secs_f64();
    let gpu_total = (gpu_enc + gpu_dec).as_secs_f64();
    let reduction = 1.0 - gpu_total / cpu_total;
    println!(
        "GATE1 cpu_path: encode={:.1}ms decode={:.1}ms per frame, {} B/frame | vram_path: encode={:.1}ms decode={:.1}ms per frame, {} B/frame",
        cpu_enc.as_secs_f64() * 1000.0 / measured,
        cpu_dec.as_secs_f64() * 1000.0 / measured,
        cpu_bytes / (count - 1),
        gpu_enc.as_secs_f64() * 1000.0 / measured,
        gpu_dec.as_secs_f64() * 1000.0 / measured,
        gpu_bytes / (count - 1)
    );
    println!(
        "GATE1 cpu_time_per_frame cpu_path={:.1}ms vram_path={:.1}ms reduction={:.1}% core_load_at_60hz cpu_path={:.0}% vram_path={:.0}%",
        cpu_total * 1000.0 / measured,
        gpu_total * 1000.0 / measured,
        reduction * 100.0,
        cpu_total / measured * 60.0 * 100.0,
        gpu_total / measured * 60.0 * 100.0
    );
    assert!(reduction >= 0.7, "reduction {:.1}%", reduction * 100.0);
}

fn desktop(width: usize, height: usize) -> Vec<u8> {
    let mut rgba = vec![255u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let (u, v) = (x % 128, y % 128);
            let n = hash(u, v, 11);
            let mut pixel = [
                ((u * 2 + (n % 5) as usize) % 256) as u8,
                ((v * 2 + ((n >> 8) % 5) as usize) % 256) as u8,
                (((u + v) + ((n >> 16) % 5) as usize) % 256) as u8,
            ];
            if (256..448).contains(&x) && (192..320).contains(&y) {
                let w = hash(x, y, 77);
                pixel = [(w % 251) as u8, ((w >> 8) % 241) as u8, ((w >> 16) % 239) as u8];
            }
            if width > 1920 && (2000..2128).contains(&x) && (100..164).contains(&y) {
                let w = hash(x, y, 99);
                pixel = [(w % 251) as u8, ((w >> 8) % 241) as u8, ((w >> 16) % 239) as u8];
            }
            if y + 48 >= height {
                pixel = [32, 36, 48];
            }
            let at = (y * width + x) * 4;
            rgba[at..at + 3].copy_from_slice(&pixel);
        }
    }
    rgba
}

fn all_zero(canvas: &[u8]) -> bool {
    canvas.iter().all(|b| *b == 0)
}

fn rgba_of(rgb: &[u8]) -> Vec<u8> {
    rgb.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect()
}

#[test]
fn gate2_seamless_resolution_hot_switch_under_100ms() {
    let _serial = TIMING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut encoder = GDeskEncoder::new(1920, 1080, GDeskConfig::default()).unwrap();
    let mut decoder = GDeskDecoder::new();
    let mut base = desktop(1920, 1080);
    for step in 0..4usize {
        for y in 0..8 {
            for x in 0..64 {
                let at = (y * 1920 + 1700 + x) * 4;
                base[at] = (step * 40) as u8;
            }
        }
        let frame = encoder
            .encode_frame(&base, 1920 * 4, PixelFormat::Rgba, step as u64)
            .unwrap()
            .unwrap();
        decoder.decode_frame(&frame).unwrap();
        assert!(!all_zero(decoder.canvas_rgb()), "black frame at step {step}");
    }
    while encoder.has_pending_refinement() {
        if let Some(frame) = encoder.poll_pending_refinement().unwrap() {
            decoder.decode_frame(&frame).unwrap();
        }
    }
    assert_eq!(rgb_error(decoder.canvas_rgb(), 3, &base), 0);
    let epoch_before = encoder.canvas_epoch();
    let hits_before = encoder.stats.cache_hits;

    let target = desktop(2560, 1440);
    let started = Instant::now();
    encoder.resize_seamless(2560, 1440).unwrap();
    let frame = encoder
        .encode_frame(&target, 2560 * 4, PixelFormat::Rgba, 100)
        .unwrap()
        .unwrap();
    let header = GDeskFrameHeader::from_bytes(&frame).unwrap();
    decoder.decode_frame(&frame).unwrap();
    let elapsed = started.elapsed();
    assert_eq!(header.canvas_epoch, epoch_before.wrapping_add(1));
    assert_eq!(header.frame_flags & (KEYFRAME_RESET | CACHE_KEEP), KEYFRAME_RESET | CACHE_KEEP);
    assert_eq!(decoder.dimensions(), Some((2560, 1440)));
    assert!(!all_zero(decoder.canvas_rgb()), "black frame after the switch");
    let error = rgb_error(decoder.canvas_rgb(), 3, &target);
    let hits = encoder.stats.cache_hits - hits_before;
    println!(
        "GATE2 first_frame_ms={:.1} bytes={} cache_hits={hits}/920 epoch {}->{} max_error={error}",
        elapsed.as_secs_f64() * 1000.0,
        frame.len(),
        epoch_before,
        header.canvas_epoch
    );
    assert!(hits > 600, "cache must survive the switch");
    assert!(decoder.is_lossless());
    assert_eq!(error, 0);
    assert!(elapsed < Duration::from_millis(100), "{elapsed:?}");

    let mut survivor = GDeskDecoder::new();
    let mut source = GDeskEncoder::new(256, 128, GDeskConfig::default()).unwrap();
    let small = desktop(256, 128);
    let first = source.encode_frame(&small, 256 * 4, PixelFormat::Rgba, 0).unwrap().unwrap();
    survivor.decode_frame(&first).unwrap();
    let before = survivor.canvas_rgb().to_vec();
    source.resize_seamless(512, 256).unwrap();
    let mut broken = source
        .encode_frame(&desktop(512, 256), 512 * 4, PixelFormat::Rgba, 1)
        .unwrap()
        .unwrap();
    let last = broken.len() - 1;
    broken[last] ^= 0xFF;
    broken.truncate(broken.len() - 40);
    assert!(survivor.decode_frame(&broken).is_err());
    assert_eq!(survivor.canvas_rgb(), &before[..], "a failed epoch switch must keep the old canvas");
    assert!(!all_zero(survivor.canvas_rgb()));

    let ctx = GpuContext::new().expect("GPU adapter required");
    let gpu_canvas = GpuDecoder::new_standalone(ctx.clone(), 2560, 1440).unwrap();
    let canvas_rgba = rgba_of(decoder.canvas_rgb());
    gpu_canvas.upload_canvas(&canvas_rgba).unwrap();
    let identity = gpu_canvas.resample_sharpen(2560, 1440, 0.0).unwrap();
    assert_eq!(rgb_error(&identity, 4, &canvas_rgba), 0, "1:1 resample must be the identity");
    let window = gpu_canvas.resample_sharpen(1920, 1080, 0.8).unwrap();
    assert_eq!(window.len(), 1920 * 1080 * 4);
    assert!(!all_zero(&window));

    let flat = GpuDecoder::new_standalone(ctx.clone(), 128, 64).unwrap();
    flat.upload_canvas(&[200, 100, 50, 255].repeat(128 * 64)).unwrap();
    let shrunk = flat.resample_sharpen(96, 54, 1.0).unwrap();
    assert!(shrunk.chunks_exact(4).all(|p| p[0].abs_diff(200) <= 1 && p[1].abs_diff(100) <= 1 && p[2].abs_diff(50) <= 1));

    let step = GpuDecoder::new_standalone(ctx, 128, 64).unwrap();
    let edge: Vec<u8> = (0..64)
        .flat_map(|_| (0..128).flat_map(|x| if x < 64 { [80u8, 80, 80, 255] } else { [170, 170, 170, 255] }))
        .collect();
    step.upload_canvas(&edge).unwrap();
    let sharp = step.resample_sharpen(192, 96, 0.8).unwrap();
    let row = &sharp[48 * 192 * 4..49 * 192 * 4];
    let luma: Vec<i32> = row.chunks_exact(4).map(|p| p[0] as i32).collect();
    let steepest = luma.windows(2).map(|w| (w[1] - w[0]).abs()).max().unwrap();
    let bilinear_steepest = {
        let sample = |x: f32| -> f32 {
            let source = (x + 0.5) / 1.5 - 0.5;
            let (a, b) = (source.floor().clamp(0.0, 127.0), (source.floor() + 1.0).clamp(0.0, 127.0));
            let value = |i: f32| if i < 64.0 { 80.0 } else { 170.0 };
            value(a) + (value(b) - value(a)) * (source - source.floor())
        };
        (0..191).map(|x| (sample(x as f32 + 1.0) - sample(x as f32)).abs()).fold(0.0, f32::max)
    };
    let overshoot = luma.iter().any(|v| *v > 170 || *v < 80);
    println!("GATE2 resample steepest={steepest} bilinear_steepest={bilinear_steepest:.1} overshoot={overshoot}");
    assert!(steepest as f32 > bilinear_steepest + 5.0);
    assert!(overshoot, "Lanczos/CAS must overshoot where bilinear cannot");
}

#[test]
fn vram_texture_handle_roundtrip_and_native_handles_are_refused() {
    let ctx = GpuContext::new().expect("GPU adapter required");
    let (width, height) = (256u32, 64u32);
    let pixels: Vec<u8> = (0..width * height)
        .flat_map(|i| {
            let n = hash(i as usize, 5, 3);
            [(n % 251) as u8, ((n >> 8) % 241) as u8, ((n >> 16) % 239) as u8, 255]
        })
        .collect();
    let texture = ctx.device.create_texture(&wgpu_texture_descriptor(width, height));
    ctx.queue.write_texture(
        texture.as_image_copy(),
        &pixels,
        wgpu_layout(width),
        texture.size(),
    );
    let mut encoder = GDeskVramEncoder::new(ctx.clone(), width as u16, height as u16).unwrap();
    assert!(encoder
        .register(VramTextureHandle::Dxgi { shared_handle: 1, width, height })
        .is_err());
    assert!(encoder
        .register(VramTextureHandle::DmaBuf { fd: 3, stride: 1024, modifier: 0, width, height })
        .is_err());
    let id = encoder.register(VramTextureHandle::WgpuTexture(texture)).unwrap();
    let packet = encoder.encode(id).unwrap().unwrap();
    assert!(encoder.encode(id).unwrap().is_none(), "an unchanged frame produces nothing");
    let mut decoder = GDeskVramDecoder::new(ctx);
    decoder.decode(&packet).unwrap();
    assert_eq!(rgb_error(&decoder.canvas().unwrap(), 4, &pixels), 0);
}

fn wgpu_texture_descriptor(width: u32, height: u32) -> gdesk_wgpu::TextureDescriptor<'static> {
    gdesk_wgpu::TextureDescriptor {
        label: None,
        size: gdesk_wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: gdesk_wgpu::TextureDimension::D2,
        format: gdesk_wgpu::TextureFormat::Rgba8Unorm,
        usage: gdesk_wgpu::TextureUsages::COPY_DST | gdesk_wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    }
}

fn wgpu_layout(width: u32) -> gdesk_wgpu::TexelCopyBufferLayout {
    gdesk_wgpu::TexelCopyBufferLayout {
        offset: 0,
        bytes_per_row: Some(width * 4),
        rows_per_image: None,
    }
}

#[test]
fn vram_decoder_rejects_malformed_packets() {
    let ctx = GpuContext::new().expect("GPU adapter required");
    let (width, height) = (256u32, 64u32);
    let frame = |seed: usize| -> Vec<u8> {
        (0..width * height)
            .flat_map(|i| {
                let n = hash(i as usize, seed, 3);
                [(n % 251) as u8, ((n >> 8) % 241) as u8, ((n >> 16) % 239) as u8, 255]
            })
            .collect()
    };
    let mut encoder = GDeskVramEncoder::new(ctx.clone(), width as u16, height as u16).unwrap();
    let first = encoder
        .register(VramTextureHandle::WgpuBuffer(ctx.create_frame_buffer(&frame(1))))
        .unwrap();
    let second = encoder
        .register(VramTextureHandle::WgpuBuffer(ctx.create_frame_buffer(&frame(2))))
        .unwrap();
    let reset = encoder.encode(first).unwrap().unwrap();
    let delta = encoder.encode(second).unwrap().unwrap();

    let mut decoder = GDeskVramDecoder::new(ctx.clone());
    assert!(decoder.decode(&delta).is_err(), "a delta cannot start a session");
    let mut bad_flags = reset.clone();
    bad_flags[6] |= 0x80;
    assert!(decoder.decode(&bad_flags).is_err());
    let mut huge = reset.clone();
    huge[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(decoder.decode(&huge).is_err());
    let mut partial = reset.clone();
    partial[16..20].copy_from_slice(&1u32.to_le_bytes());
    assert!(decoder.decode(&partial).is_err(), "a reset must cover every tile");
    decoder.decode(&reset).unwrap();
    let mut stale = delta.clone();
    stale[12..16].copy_from_slice(&7u32.to_le_bytes());
    assert!(decoder.decode(&stale).is_err(), "out of order sequence");
    decoder.decode(&delta).unwrap();
    assert_eq!(rgb_error(&decoder.canvas().unwrap(), 4, &frame(2)), 0);

    let mut compressed_encoder = GDeskVramEncoder::new(ctx.clone(), width as u16, height as u16).unwrap();
    let id = compressed_encoder
        .register(VramTextureHandle::WgpuBuffer(ctx.create_frame_buffer(&frame(3))))
        .unwrap();
    let packet = compressed_encoder.encode(id).unwrap().unwrap();
    let mut fresh = GDeskVramDecoder::new(ctx.clone());
    assert!(fresh.decode(&packet[..packet.len() - 1]).is_err());
    let solid_entry = |index: u32| [(index | 1 << 16).to_le_bytes(), [0u8; 4]].concat();
    let forged = |entries: &[u32], declared: u32| {
        let body: Vec<u8> = entries.iter().flat_map(|index| solid_entry(*index)).collect();
        let mut compressed = lz4_flex::block::compress_prepend_size(&body);
        compressed[0..4].copy_from_slice(&declared.to_le_bytes());
        let mut forged = packet[..16].to_vec();
        forged.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        forged.extend_from_slice(&compressed);
        forged[6] = 1;
        forged[7] = 0;
        forged
    };
    let tiles = gdesk::protocol::tile_count(width as u16, height as u16).unwrap() as u32;
    let all: Vec<u32> = (0..tiles).collect();
    let mut repeated = all.clone();
    repeated[1] = 0;
    assert!(fresh.decode(&forged(&repeated, tiles * 8)).is_err(), "duplicate tile index");
    assert!(fresh.decode(&forged(&all, tiles * 8 + 1)).is_err(), "declared size must match the body");
    fresh.decode(&packet).unwrap();
    assert_eq!(rgb_error(&fresh.canvas().unwrap(), 4, &frame(3)), 0);
}

#[test]
fn vram_register_rejects_bad_textures() {
    let ctx = GpuContext::new().expect("GPU adapter required");
    let mut encoder = GDeskVramEncoder::new(ctx.clone(), 256, 64).unwrap();
    let mut descriptor = wgpu_texture_descriptor(256, 64);
    descriptor.sample_count = 4;
    descriptor.usage = gdesk_wgpu::TextureUsages::RENDER_ATTACHMENT | gdesk_wgpu::TextureUsages::COPY_SRC;
    let multisampled = ctx.device.create_texture(&descriptor);
    assert!(encoder.register(VramTextureHandle::WgpuTexture(multisampled)).is_err());
    let mut descriptor = wgpu_texture_descriptor(256, 64);
    descriptor.format = gdesk_wgpu::TextureFormat::Rgba16Float;
    let wrong_format = ctx.device.create_texture(&descriptor);
    assert!(encoder.register(VramTextureHandle::WgpuTexture(wrong_format)).is_err());
    let mut descriptor = wgpu_texture_descriptor(256, 64);
    descriptor.format = gdesk_wgpu::TextureFormat::Bgra8Unorm;
    let bgra = ctx.device.create_texture(&descriptor);
    assert!(encoder.register(VramTextureHandle::WgpuTexture(bgra)).is_err(), "BGRA would swap channels");
}
