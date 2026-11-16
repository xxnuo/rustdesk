struct Params {
    width: u32,
    height: u32,
    tiles_x: u32,
    tiles_y: u32,
    lossy: u32,
    sharpen: f32,
    force: u32,
    shifts: u32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> frame: array<u32>;
@group(0) @binding(3) var<storage, read> list: array<u32>;
@group(0) @binding(4) var<storage, read_write> args: array<atomic<u32>>;
@group(0) @binding(5) var<storage, read_write> coeffs: array<u32>;
@group(0) @binding(6) var<storage, read_write> canvas: array<u32>;
@group(0) @binding(7) var<storage, read_write> wstate: array<u32>;
@group(0) @binding(8) var<storage, read_write> tile_flags: array<u32>;
@group(0) @binding(9) var<storage, read_write> display: array<u32>;
@group(0) @binding(10) var<storage, read> frame4: array<vec4<u32>>;
@group(0) @binding(11) var<storage, read_write> hashes: array<u32>;

var<workgroup> plane_yc: array<u32, 4096>;
var<workgroup> plane_cg: array<i32, 4096>;
var<workgroup> wg_entry: u32;

fn pack_pair(a: i32, b: i32) -> u32 {
    return (u32(a) & 0xFFFFu) | ((u32(b) & 0xFFFFu) << 16u);
}

fn unpack_lo(w: u32) -> i32 {
    return bitcast<i32>(w << 16u) >> 16u;
}

fn unpack_hi(w: u32) -> i32 {
    return bitcast<i32>(w) >> 16u;
}

fn tri_high(level: u32, axis: u32, lid: u32, sign: i32) {
    let m = 32u >> level;
    let mask = m - 1u;
    let line_mask = (m << 1u) - 1u;
    let sh_a = 5u - level;
    let sh_b = 6u - level;
    let el_sh = select(level, level + 6u, axis == 1u);
    let ln_sh = select(level + 6u, level, axis == 1u);
    let el = 1u << el_sh;
    let total = m << sh_b;
    for (var i = lid; i < total; i += 256u) {
        let line = select(i >> sh_a, i & line_mask, axis == 1u);
        let k = select(i & mask, i >> sh_b, axis == 1u);
        let base = line << ln_sh;
        let at = base + (((k << 1u) + 1u) << el_sh);
        let ib = base + ((min(k + 1u, mask) << 1u) << el_sh);
        let a = plane_yc[at - el];
        let b = plane_yc[ib];
        let cur = plane_yc[at];
        let lo = unpack_lo(cur) - sign * ((unpack_lo(a) + unpack_lo(b)) >> 1u);
        let hi = unpack_hi(cur) - sign * ((unpack_hi(a) + unpack_hi(b)) >> 1u);
        plane_yc[at] = pack_pair(lo, hi);
        plane_cg[at] -= sign * ((plane_cg[at - el] + plane_cg[ib]) >> 1u);
    }
}

fn tri_low(level: u32, axis: u32, lid: u32, sign: i32) {
    let m = 32u >> level;
    let mask = m - 1u;
    let line_mask = (m << 1u) - 1u;
    let sh_a = 5u - level;
    let sh_b = 6u - level;
    let el_sh = select(level, level + 6u, axis == 1u);
    let ln_sh = select(level + 6u, level, axis == 1u);
    let el = 1u << el_sh;
    let total = m << sh_b;
    for (var i = lid; i < total; i += 256u) {
        let line = select(i >> sh_a, i & line_mask, axis == 1u);
        let k = select(i & mask, i >> sh_b, axis == 1u);
        let base = line << ln_sh;
        let at = base + ((k << 1u) << el_sh);
        let ia = select(at - el, at + el, k == 0u);
        let ib = at + el;
        let a = plane_yc[ia];
        let b = plane_yc[ib];
        let cur = plane_yc[at];
        let lo = unpack_lo(cur) + sign * ((unpack_lo(a) + unpack_lo(b) + 2) >> 2u);
        let hi = unpack_hi(cur) + sign * ((unpack_hi(a) + unpack_hi(b) + 2) >> 2u);
        plane_yc[at] = pack_pair(lo, hi);
        plane_cg[at] += sign * ((plane_cg[ia] + plane_cg[ib] + 2) >> 2u);
    }
}

fn forward_tri(lid: u32) {
    for (var level = 0u; level < 3u; level++) {
        for (var axis = 0u; axis < 2u; axis++) {
            tri_high(level, axis, lid, 1);
            workgroupBarrier();
            tri_low(level, axis, lid, 1);
            workgroupBarrier();
        }
    }
}

fn inverse_tri(lid: u32) {
    for (var step = 0u; step < 3u; step++) {
        let level = 2u - step;
        for (var axis_step = 0u; axis_step < 2u; axis_step++) {
            let axis = 1u - axis_step;
            tri_low(level, axis, lid, -1);
            workgroupBarrier();
            tri_high(level, axis, lid, -1);
            workgroupBarrier();
        }
    }
}

fn forward_ycocg(px: u32) -> vec3<i32> {
    let r = i32(px & 255u);
    let g = i32((px >> 8u) & 255u);
    let b = i32((px >> 16u) & 255u);
    let co = r - b;
    let t = b + (co >> 1u);
    let cg = g - t;
    return vec3<i32>(t + (cg >> 1u), co, cg);
}

fn inverse_ycocg(y: i32, co: i32, cg: i32) -> u32 {
    let t = y - (cg >> 1u);
    let g = cg + t;
    let b = t - (co >> 1u);
    let r = b + co;
    return 0xFF000000u | u32(clamp(r, 0, 255)) | (u32(clamp(g, 0, 255)) << 8u) | (u32(clamp(b, 0, 255)) << 16u);
}

fn truncate_coeff(v: i32, c: u32, p: u32) -> i32 {
    let shift = select((params.shifts >> 8u) & 255u, params.shifts & 255u, p == 0u);
    if shift == 0u || (c & 0x1C7u) == 0u { return v; }
    let t = i32((u32(abs(v)) >> shift) << shift);
    return select(t, -t, v < 0);
}

@compute @workgroup_size(16, 16, 1)
fn wavelet_pack(@builtin(workgroup_id) wid: vec3<u32>, @builtin(local_invocation_index) lid: u32) {
    let w = params.width;
    let h = params.height;
    let slot = wid.x;
    if lid == 0u { wg_entry = list[slot * 2u]; }
    let entry = workgroupUniformLoad(&wg_entry);
    let tile = entry & 0xFFFFu;
    let tx = tile % params.tiles_x;
    let ty = tile / params.tiles_x;
    if ((entry >> 16u) & 1u) != 0u { return; }
    for (var k = 0u; k < 16u; k++) {
        let idx = lid + k * 256u;
        let x = min(tx * 64u + (idx & 63u), w - 1u);
        let y = min(ty * 64u + (idx >> 6u), h - 1u);
        let ycc = forward_ycocg(frame[y * w + x]);
        plane_yc[idx] = pack_pair(ycc.x, ycc.y);
        plane_cg[idx] = ycc.z;
    }
    workgroupBarrier();
    forward_tri(lid);
    for (var k = 0u; k < 8u; k++) {
        let word = lid + k * 256u;
        let c0 = word * 2u;
        let yc0 = plane_yc[c0];
        let yc1 = plane_yc[c0 + 1u];
        coeffs[(slot * 3u + 0u) * 2048u + word] = pack_pair(
            truncate_coeff(unpack_lo(yc0), c0, 0u),
            truncate_coeff(unpack_lo(yc1), c0 + 1u, 0u));
        coeffs[(slot * 3u + 1u) * 2048u + word] = pack_pair(
            truncate_coeff(unpack_hi(yc0), c0, 1u),
            truncate_coeff(unpack_hi(yc1), c0 + 1u, 1u));
        coeffs[(slot * 3u + 2u) * 2048u + word] = pack_pair(
            truncate_coeff(plane_cg[c0], c0, 2u),
            truncate_coeff(plane_cg[c0 + 1u], c0 + 1u, 2u));
    }
}

@compute @workgroup_size(16, 16, 1)
fn wavelet_decode(@builtin(workgroup_id) wid: vec3<u32>, @builtin(local_invocation_index) lid: u32) {
    let w = params.width;
    let h = params.height;
    let slot = wid.x;
    if lid == 0u { wg_entry = list[slot * 2u]; }
    let entry = workgroupUniformLoad(&wg_entry);
    let tile = entry & 0xFFFFu;
    let tx = tile % params.tiles_x;
    let ty = tile / params.tiles_x;
    if lid == 0u { tile_flags[tile] = select(params.lossy, 0u, ((entry >> 16u) & 1u) != 0u); }
    if ((entry >> 16u) & 1u) != 0u {
        let color = 0xFF000000u | list[slot * 2u + 1u];
        for (var k = 0u; k < 16u; k++) {
            let idx = lid + k * 256u;
            let x = tx * 64u + idx % 64u;
            let y = ty * 64u + idx / 64u;
            if x < w && y < h { canvas[y * w + x] = color; }
        }
        return;
    }
    for (var k = 0u; k < 8u; k++) {
        let word = lid + k * 256u;
        let vy = coeffs[(slot * 3u + 0u) * 2048u + word];
        let vco = coeffs[(slot * 3u + 1u) * 2048u + word];
        let vcg = coeffs[(slot * 3u + 2u) * 2048u + word];
        if params.lossy != 0u || k == 0u {
            wstate[tile * 6144u + 0u * 2048u + word] = vy;
            wstate[tile * 6144u + 1u * 2048u + word] = vco;
            wstate[tile * 6144u + 2u * 2048u + word] = vcg;
        }
        let c0 = word * 2u;
        plane_yc[c0] = (vy & 0xFFFFu) | ((vco & 0xFFFFu) << 16u);
        plane_yc[c0 + 1u] = (vy >> 16u) | (vco & 0xFFFF0000u);
        plane_cg[c0] = unpack_lo(vcg);
        plane_cg[c0 + 1u] = unpack_hi(vcg);
    }
    workgroupBarrier();
    inverse_tri(lid);
    for (var k = 0u; k < 16u; k++) {
        let idx = lid + k * 256u;
        let x = tx * 64u + (idx & 63u);
        let y = ty * 64u + (idx >> 6u);
        if x < w && y < h {
            let yc = plane_yc[idx];
            canvas[y * w + x] = inverse_ycocg(unpack_lo(yc), unpack_hi(yc), plane_cg[idx]);
        }
    }
}

fn load_canvas(x: i32, y: i32) -> vec3<f32> {
    let cx = u32(clamp(x, 0, i32(params.width) - 1));
    let cy = u32(clamp(y, 0, i32(params.height) - 1));
    let px = canvas[cy * params.width + cx];
    return vec3<f32>(f32(px & 255u), f32((px >> 8u) & 255u), f32((px >> 16u) & 255u));
}

fn tile_lossy(x: i32, y: i32) -> bool {
    let cx = u32(clamp(x, 0, i32(params.width) - 1)) / 64u;
    let cy = u32(clamp(y, 0, i32(params.height) - 1)) / 64u;
    return tile_flags[cy * params.tiles_x + cx] != 0u;
}

@compute @workgroup_size(16, 16, 1)
fn present(@builtin(global_invocation_id) gid: vec3<u32>) {
    if gid.x >= params.width || gid.y >= params.height { return; }
    let x = i32(gid.x);
    let y = i32(gid.y);
    var color = load_canvas(x, y);
    var dx = 0;
    var dy = 0;
    if x % 64 == 0 { dx = -1; } else if x % 64 == 63 { dx = 1; }
    if y % 64 == 0 { dy = -1; } else if y % 64 == 63 { dy = 1; }
    if (dx != 0 || dy != 0) && tile_lossy(x, y) {
        color = (color * 3.0 + load_canvas(x + dx, y + dy)) * 0.25;
    }
    if params.sharpen > 0.0 {
        let ring = load_canvas(x - 1, y) + load_canvas(x + 1, y) + load_canvas(x, y - 1) + load_canvas(x, y + 1);
        color = color + params.sharpen * (4.0 * color - ring) * 0.25;
    }
    let out = vec3<u32>(clamp(color + vec3<f32>(0.5), vec3<f32>(0.0), vec3<f32>(255.0)));
    display[gid.y * params.width + gid.x] = 0xFF000000u | out.x | (out.y << 8u) | (out.z << 16u);
}

struct ResampleParams {
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    sharpness: f32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
}

@group(0) @binding(13) var<storage, read_write> scaled: array<u32>;
@group(0) @binding(14) var<uniform> rparams: ResampleParams;

fn rs_load(x: i32, y: i32) -> vec3<f32> {
    let cx = u32(clamp(x, 0, i32(rparams.src_w) - 1));
    let cy = u32(clamp(y, 0, i32(rparams.src_h) - 1));
    let px = canvas[cy * rparams.src_w + cx];
    return vec3<f32>(f32(px & 255u), f32((px >> 8u) & 255u), f32((px >> 16u) & 255u)) * (1.0 / 255.0);
}

fn lanczos2(x: f32) -> f32 {
    let ax = abs(x);
    if ax < 1e-6 { return 1.0; }
    if ax >= 2.0 { return 0.0; }
    let px = 3.14159265358979 * ax;
    return 2.0 * sin(px) * sin(px * 0.5) / (px * px);
}

fn rs_lanczos(dx: i32, dy: i32) -> vec3<f32> {
    let scale = vec2<f32>(f32(rparams.src_w) / f32(rparams.dst_w), f32(rparams.src_h) / f32(rparams.dst_h));
    let center = (vec2<f32>(f32(dx) + 0.5, f32(dy) + 0.5)) * scale - vec2<f32>(0.5);
    let support = clamp(scale, vec2<f32>(1.0), vec2<f32>(8.0));
    let x0 = i32(floor(center.x - 2.0 * support.x)) + 1;
    let x1 = i32(floor(center.x + 2.0 * support.x));
    let y0 = i32(floor(center.y - 2.0 * support.y)) + 1;
    let y1 = i32(floor(center.y + 2.0 * support.y));
    var acc = vec3<f32>(0.0);
    var total = 0.0;
    for (var y = y0; y <= y1; y++) {
        let wy = lanczos2((f32(y) - center.y) / support.y);
        for (var x = x0; x <= x1; x++) {
            let weight = lanczos2((f32(x) - center.x) / support.x) * wy;
            acc += weight * rs_load(x, y);
            total += weight;
        }
    }
    return acc / total;
}

@compute @workgroup_size(16, 16, 1)
fn resample_sharpen(@builtin(global_invocation_id) gid: vec3<u32>) {
    if gid.x >= rparams.dst_w || gid.y >= rparams.dst_h { return; }
    let x = i32(gid.x);
    let y = i32(gid.y);
    let center = rs_lanczos(x, y);
    var color = center;
    if rparams.sharpness > 0.0 {
        let north = rs_lanczos(x, max(y - 1, 0));
        let south = rs_lanczos(x, min(y + 1, i32(rparams.dst_h) - 1));
        let west = rs_lanczos(max(x - 1, 0), y);
        let east = rs_lanczos(min(x + 1, i32(rparams.dst_w) - 1), y);
        let low = min(min(min(north, south), min(west, east)), center);
        let high = max(max(max(north, south), max(west, east)), center);
        let amp = sqrt(clamp(min(low, vec3<f32>(1.0) - high) / max(high, vec3<f32>(1e-4)), vec3<f32>(0.0), vec3<f32>(1.0)));
        let peak = -1.0 / mix(8.0, 5.0, clamp(rparams.sharpness, 0.0, 1.0));
        let weight = amp * peak;
        color = (weight * (north + south + west + east) + center) / (vec3<f32>(1.0) + 4.0 * weight);
    }
    let out = vec3<u32>(clamp(color * 255.0 + vec3<f32>(0.5), vec3<f32>(0.0), vec3<f32>(255.0)));
    scaled[gid.y * rparams.dst_w + gid.x] = 0xFF000000u | out.x | (out.y << 8u) | (out.z << 16u);
}
