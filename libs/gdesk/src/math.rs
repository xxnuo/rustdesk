use crate::{Error, Result};

pub const TILE_SIZE: usize = 64;
pub const TILE_PIXELS: usize = TILE_SIZE * TILE_SIZE;
pub const TILE_RGB_BYTES: usize = TILE_PIXELS * 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Rgb,
    Rgba,
    Bgra,
}
impl PixelFormat {
    pub fn bytes_per_pixel(self) -> usize {
        if self == Self::Rgb {
            3
        } else {
            4
        }
    }
}

pub fn validate_image(
    bytes: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    format: PixelFormat,
) -> Result<()> {
    let row = width
        .checked_mul(format.bytes_per_pixel())
        .ok_or(Error("image size overflow"))?;
    let required = height
        .checked_sub(1)
        .and_then(|h| h.checked_mul(stride))
        .and_then(|n| n.checked_add(row))
        .ok_or(Error("invalid image dimensions"))?;
    if width == 0 || stride < row || bytes.len() < required {
        return Err(Error("invalid image buffer"));
    }
    Ok(())
}

pub fn extract_tile(
    bytes: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    format: PixelFormat,
    tile_index: usize,
) -> Result<Vec<u8>> {
    validate_image(bytes, width, height, stride, format)?;
    let cols = width.div_ceil(TILE_SIZE);
    if tile_index >= cols * height.div_ceil(TILE_SIZE) {
        return Err(Error("invalid tile index"));
    }
    let mut tile = vec![0; TILE_RGB_BYTES];
    let x0 = tile_index % cols * TILE_SIZE;
    let y0 = tile_index / cols * TILE_SIZE;
    for y in 0..TILE_SIZE {
        for x in 0..TILE_SIZE {
            let src = (y0 + y).min(height - 1) * stride
                + (x0 + x).min(width - 1) * format.bytes_per_pixel();
            let dst = (y * TILE_SIZE + x) * 3;
            let p = &bytes[src..src + 3];
            if format == PixelFormat::Bgra {
                tile[dst..dst + 3].copy_from_slice(&[p[2], p[1], p[0]]);
            } else {
                tile[dst..dst + 3].copy_from_slice(p);
            }
        }
    }
    Ok(tile)
}

pub fn write_tile(
    tile: &[u8],
    output: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    format: PixelFormat,
    tile_index: usize,
) -> Result<()> {
    validate_image(output, width, height, stride, format)?;
    if tile.len() != TILE_RGB_BYTES {
        return Err(Error("invalid RGB tile"));
    }
    let cols = width.div_ceil(TILE_SIZE);
    if tile_index >= cols * height.div_ceil(TILE_SIZE) {
        return Err(Error("invalid tile index"));
    }
    let x0 = tile_index % cols * TILE_SIZE;
    let y0 = tile_index / cols * TILE_SIZE;
    for y in 0..TILE_SIZE.min(height - y0) {
        for x in 0..TILE_SIZE.min(width - x0) {
            let src = (y * TILE_SIZE + x) * 3;
            let dst = (y0 + y) * stride + (x0 + x) * format.bytes_per_pixel();
            let p = &tile[src..src + 3];
            if format == PixelFormat::Bgra {
                output[dst..dst + 3].copy_from_slice(&[p[2], p[1], p[0]]);
            } else {
                output[dst..dst + 3].copy_from_slice(p);
            }
            if format != PixelFormat::Rgb {
                output[dst + 3] = 255;
            }
        }
    }
    Ok(())
}

pub fn tile_hash(rgb: &[u8]) -> u64 {
    xxhash_rust::xxh64::xxh64(rgb, 0)
}
pub fn detect_solid(rgb: &[u8]) -> Option<[u8; 3]> {
    if rgb.len() != TILE_RGB_BYTES {
        return None;
    }
    let color = [rgb[0], rgb[1], rgb[2]];
    rgb.chunks_exact(3).all(|p| p == color).then_some(color)
}
pub fn restore_solid(color: [u8; 3]) -> Vec<u8> {
    color.repeat(TILE_PIXELS)
}
pub type TwoColorTile = ([[u8; 3]; 2], Box<[u8; 512]>);
pub fn detect_two_color(rgb: &[u8]) -> Option<TwoColorTile> {
    if rgb.len() != TILE_RGB_BYTES {
        return None;
    }
    let first = [rgb[0], rgb[1], rgb[2]];
    let mut second = None;
    let mut mask = Box::new([0; 512]);
    for (i, p) in rgb.chunks_exact(3).enumerate() {
        if p != first {
            let color = [p[0], p[1], p[2]];
            if second.is_some_and(|s| s != color) {
                return None;
            }
            second = Some(color);
            mask[i / 8] |= 1 << (i % 8);
        }
    }
    second.map(|s| ([first, s], mask))
}
pub fn restore_two_color(colors: [[u8; 3]; 2], mask: &[u8; 512]) -> Vec<u8> {
    let mut rgb = vec![0; TILE_RGB_BYTES];
    for i in 0..TILE_PIXELS {
        rgb[i * 3..i * 3 + 3].copy_from_slice(&colors[((mask[i / 8] >> (i % 8)) & 1) as usize]);
    }
    rgb
}

pub fn forward_ycocg([r, g, b]: [u8; 3]) -> [i16; 3] {
    let co = r as i16 - b as i16;
    let t = b as i16 + (co >> 1);
    let cg = g as i16 - t;
    [t + (cg >> 1), co, cg]
}
pub fn inverse_ycocg([y, co, cg]: [i16; 3]) -> [u8; 3] {
    let t = y as i32 - (cg as i32 >> 1);
    let g = cg as i32 + t;
    let b = t - (co as i32 >> 1);
    let r = b + co as i32;
    [
        r.clamp(0, 255) as u8,
        g.clamp(0, 255) as u8,
        b.clamp(0, 255) as u8,
    ]
}

pub fn forward_53(values: &mut [i16]) {
    let m = values.len() / 2;
    if m == 0 || values.len() % 2 != 0 {
        return;
    }
    let mut low: Vec<i32> = values.iter().step_by(2).map(|v| *v as i32).collect();
    let mut high: Vec<i32> = values
        .iter()
        .skip(1)
        .step_by(2)
        .map(|v| *v as i32)
        .collect();
    for k in 0..m {
        high[k] -= (low[k] + low[(k + 1).min(m - 1)]) >> 1;
    }
    for k in 0..m {
        low[k] += (high[k.saturating_sub(1)] + high[k] + 2) >> 2;
    }
    for k in 0..m {
        values[k] = low[k] as i16;
        values[m + k] = high[k] as i16;
    }
}
pub fn inverse_53(values: &mut [i16]) {
    let m = values.len() / 2;
    if m == 0 || values.len() % 2 != 0 {
        return;
    }
    let mut low: Vec<i32> = values[..m].iter().map(|v| *v as i32).collect();
    let mut high: Vec<i32> = values[m..].iter().map(|v| *v as i32).collect();
    for k in 0..m {
        low[k] -= (high[k.saturating_sub(1)] + high[k] + 2) >> 2;
    }
    for k in 0..m {
        high[k] += (low[k] + low[(k + 1).min(m - 1)]) >> 1;
    }
    for k in 0..m {
        values[2 * k] = low[k].clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        values[2 * k + 1] = high[k].clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    }
}
pub fn forward_wavelet(plane: &mut [i16]) -> Result<()> {
    if plane.len() != TILE_PIXELS {
        return Err(Error("invalid wavelet plane"));
    }
    let mut column = [0; TILE_SIZE];
    for size in [64, 32, 16] {
        for y in 0..size {
            forward_53(&mut plane[y * 64..y * 64 + size]);
        }
        for x in 0..size {
            for y in 0..size {
                column[y] = plane[y * 64 + x];
            }
            forward_53(&mut column[..size]);
            for y in 0..size {
                plane[y * 64 + x] = column[y];
            }
        }
    }
    Ok(())
}
pub fn inverse_wavelet(plane: &mut [i16]) -> Result<()> {
    if plane.len() != TILE_PIXELS {
        return Err(Error("invalid wavelet plane"));
    }
    let mut column = [0; TILE_SIZE];
    for size in [16, 32, 64] {
        for x in 0..size {
            for y in 0..size {
                column[y] = plane[y * 64 + x];
            }
            inverse_53(&mut column[..size]);
            for y in 0..size {
                plane[y * 64 + x] = column[y];
            }
        }
        for y in 0..size {
            inverse_53(&mut plane[y * 64..y * 64 + size]);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaveletCoefficients {
    pub planes: [Vec<i16>; 3],
}
pub fn rgb_to_wavelet(rgb: &[u8]) -> Result<WaveletCoefficients> {
    if rgb.len() != TILE_RGB_BYTES {
        return Err(Error("invalid RGB tile"));
    }
    let mut planes = std::array::from_fn(|_| vec![0; TILE_PIXELS]);
    for (i, p) in rgb.chunks_exact(3).enumerate() {
        let ycocg = forward_ycocg([p[0], p[1], p[2]]);
        for c in 0..3 {
            planes[c][i] = ycocg[c];
        }
    }
    for plane in &mut planes {
        forward_wavelet(plane)?;
    }
    Ok(WaveletCoefficients { planes })
}
pub fn wavelet_to_rgb(coeff: &WaveletCoefficients) -> Result<Vec<u8>> {
    let mut planes = coeff.planes.clone();
    for plane in &mut planes {
        inverse_wavelet(plane)?;
    }
    let mut rgb = vec![0; TILE_RGB_BYTES];
    for i in 0..TILE_PIXELS {
        rgb[i * 3..i * 3 + 3].copy_from_slice(&inverse_ycocg([
            planes[0][i],
            planes[1][i],
            planes[2][i],
        ]));
    }
    Ok(rgb)
}
fn ll3(i: usize) -> bool {
    i / 64 < 8 && i % 64 < 8
}
fn channel_shift(channel: usize, shift_y: u8, shift_c: u8) -> u8 {
    if channel == 0 {
        shift_y
    } else {
        shift_c
    }
}
fn validate_shifts(y: u8, c: u8) -> Result<()> {
    if y > 15 || c > 15 {
        return Err(Error("invalid wavelet shift"));
    }
    Ok(())
}
fn validate_coefficients(coeff: &WaveletCoefficients) -> Result<()> {
    if coeff.planes.iter().any(|p| p.len() != TILE_PIXELS) {
        return Err(Error("invalid wavelet coefficients"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaveletState {
    pub known_mag: [Vec<u16>; 3],
    pub sign: [Vec<bool>; 3],
    pub shift_y: u8,
    pub shift_c: u8,
}
impl WaveletState {
    pub fn reconstruct(&self) -> Result<Vec<u8>> {
        validate_shifts(self.shift_y, self.shift_c)?;
        let mut coeff = WaveletCoefficients {
            planes: std::array::from_fn(|_| vec![0; TILE_PIXELS]),
        };
        for c in 0..3 {
            if self.known_mag[c].len() != TILE_PIXELS || self.sign[c].len() != TILE_PIXELS {
                return Err(Error("invalid wavelet state"));
            }
            for i in 0..TILE_PIXELS {
                let shift = if ll3(i) {
                    0
                } else {
                    channel_shift(c, self.shift_y, self.shift_c)
                };
                let mut mag = self.known_mag[c][i] as i32;
                // Midpoint reconstruction must never contaminate persistent refinement state.
                if mag != 0 && shift != 0 {
                    mag += 1 << (shift - 1);
                }
                let value = if self.sign[c][i] { -mag } else { mag };
                coeff.planes[c][i] = value.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            }
        }
        wavelet_to_rgb(&coeff)
    }
}

pub fn encode_wavelet_init(
    coeff: &WaveletCoefficients,
    shift_y: u8,
    shift_c: u8,
) -> Result<Vec<u8>> {
    validate_shifts(shift_y, shift_c)?;
    validate_coefficients(coeff)?;
    let mut bits = BitWriter::default();
    for c in 0..3 {
        let max = coeff.planes[c]
            .iter()
            .map(|v| v.unsigned_abs())
            .max()
            .unwrap_or(0);
        let width = (16 - max.leading_zeros()) as u8;
        bits.put(width as u16, 5);
        for (i, value) in coeff.planes[c].iter().enumerate() {
            let shift = if ll3(i) {
                0
            } else {
                channel_shift(c, shift_y, shift_c)
            };
            let mag = value.unsigned_abs() >> shift;
            bits.put(mag, width.saturating_sub(shift));
            if mag != 0 {
                bits.put(u16::from(*value < 0), 1);
            }
        }
    }
    Ok(bits.bytes)
}
pub fn decode_wavelet_init(payload: &[u8], shift_y: u8, shift_c: u8) -> Result<WaveletState> {
    validate_shifts(shift_y, shift_c)?;
    let mut bits = BitReader {
        bytes: payload,
        offset: 0,
    };
    let mut state = WaveletState {
        known_mag: std::array::from_fn(|_| vec![0; TILE_PIXELS]),
        sign: std::array::from_fn(|_| vec![false; TILE_PIXELS]),
        shift_y,
        shift_c,
    };
    for c in 0..3 {
        let width = bits.get(5)? as u8;
        if width > 16 {
            return Err(Error("invalid coefficient width"));
        }
        for i in 0..TILE_PIXELS {
            let shift = if ll3(i) {
                0
            } else {
                channel_shift(c, shift_y, shift_c)
            };
            let mag = (bits.get(width.saturating_sub(shift))? as u32) << shift;
            let sign = mag != 0 && bits.get(1)? != 0;
            if mag > 32768 || (mag == 32768 && !sign) {
                return Err(Error("coefficient overflow"));
            }
            state.known_mag[c][i] = mag as u16;
            state.sign[c][i] = sign;
        }
    }
    bits.finish()?;
    Ok(state)
}
pub fn encode_wavelet_refine(
    coeff: &WaveletCoefficients,
    old_y: u8,
    old_c: u8,
    new_y: u8,
    new_c: u8,
) -> Result<Vec<u8>> {
    validate_shifts(old_y, old_c)?;
    validate_shifts(new_y, new_c)?;
    validate_coefficients(coeff)?;
    if new_y > old_y || new_c > old_c || (new_y == old_y && new_c == old_c) {
        return Err(Error("invalid refinement transition"));
    }
    let mut bits = BitWriter::default();
    for c in 0..3 {
        let old = channel_shift(c, old_y, old_c);
        let new = channel_shift(c, new_y, new_c);
        let count = old - new;
        if count == 0 {
            continue;
        }
        let mask = (1u16 << count) - 1;
        for (i, value) in coeff.planes[c].iter().enumerate() {
            if ll3(i) {
                continue;
            }
            let mag = value.unsigned_abs();
            let delta = (mag >> new) & mask;
            bits.put(delta, count);
            if mag >> old == 0 && delta != 0 {
                bits.put(u16::from(*value < 0), 1);
            }
        }
    }
    Ok(bits.bytes)
}
pub fn decode_wavelet_refine(
    state: &mut WaveletState,
    payload: &[u8],
    new_y: u8,
    new_c: u8,
) -> Result<()> {
    validate_shifts(new_y, new_c)?;
    validate_shifts(state.shift_y, state.shift_c)?;
    if new_y > state.shift_y
        || new_c > state.shift_c
        || (new_y == state.shift_y && new_c == state.shift_c)
    {
        return Err(Error("invalid refinement transition"));
    }
    let mut next = state.clone();
    let mut bits = BitReader {
        bytes: payload,
        offset: 0,
    };
    for c in 0..3 {
        if next.known_mag[c].len() != TILE_PIXELS || next.sign[c].len() != TILE_PIXELS {
            return Err(Error("invalid wavelet state"));
        }
        let old = channel_shift(c, state.shift_y, state.shift_c);
        let new = channel_shift(c, new_y, new_c);
        let count = old - new;
        if count == 0 {
            continue;
        }
        for i in 0..TILE_PIXELS {
            if ll3(i) {
                continue;
            }
            let delta = bits.get(count)?;
            if next.known_mag[c][i] == 0 && delta != 0 {
                next.sign[c][i] = bits.get(1)? != 0;
            }
            let mag = next.known_mag[c][i] as u32 | ((delta as u32) << new);
            if mag > 32768 || (mag == 32768 && !next.sign[c][i]) {
                return Err(Error("coefficient overflow"));
            }
            next.known_mag[c][i] = mag as u16;
        }
    }
    bits.finish()?;
    next.shift_y = new_y;
    next.shift_c = new_c;
    *state = next;
    Ok(())
}

/// Same bitplanes as `encode_wavelet_refine`, restricted to the coefficients whose index is
/// congruent to `part` modulo `parts`, so one refinement can be spread over several packets.
pub fn encode_wavelet_refine_part(
    coeff: &WaveletCoefficients,
    old: (u8, u8),
    new: (u8, u8),
    part: usize,
    parts: usize,
) -> Result<Vec<u8>> {
    validate_shifts(old.0, old.1)?;
    validate_shifts(new.0, new.1)?;
    validate_coefficients(coeff)?;
    if parts == 0 || part >= parts || new.0 > old.0 || new.1 > old.1 || new == old {
        return Err(Error("invalid refinement transition"));
    }
    let mut bits = BitWriter::default();
    for c in 0..3 {
        let old_shift = channel_shift(c, old.0, old.1);
        let new_shift = channel_shift(c, new.0, new.1);
        let count = old_shift - new_shift;
        if count == 0 {
            continue;
        }
        let mask = (1u16 << count) - 1;
        for (i, value) in coeff.planes[c].iter().enumerate() {
            if ll3(i) || i % parts != part {
                continue;
            }
            let mag = value.unsigned_abs();
            let delta = (mag >> new_shift) & mask;
            bits.put(delta, count);
            if mag >> old_shift == 0 && delta != 0 {
                bits.put(u16::from(*value < 0), 1);
            }
        }
    }
    Ok(bits.bytes)
}

/// Applies one part to the staged state without touching its shifts; the caller commits the new
/// shifts after the last part.
pub fn decode_wavelet_refine_part(
    state: &mut WaveletState,
    payload: &[u8],
    new: (u8, u8),
    part: usize,
    parts: usize,
) -> Result<()> {
    validate_shifts(new.0, new.1)?;
    validate_shifts(state.shift_y, state.shift_c)?;
    if parts == 0
        || part >= parts
        || new.0 > state.shift_y
        || new.1 > state.shift_c
        || new == (state.shift_y, state.shift_c)
    {
        return Err(Error("invalid refinement transition"));
    }
    let mut next = state.clone();
    let mut bits = BitReader {
        bytes: payload,
        offset: 0,
    };
    for c in 0..3 {
        if next.known_mag[c].len() != TILE_PIXELS || next.sign[c].len() != TILE_PIXELS {
            return Err(Error("invalid wavelet state"));
        }
        let old_shift = channel_shift(c, state.shift_y, state.shift_c);
        let new_shift = channel_shift(c, new.0, new.1);
        let count = old_shift - new_shift;
        if count == 0 {
            continue;
        }
        for i in 0..TILE_PIXELS {
            if ll3(i) || i % parts != part {
                continue;
            }
            let delta = bits.get(count)?;
            if next.known_mag[c][i] == 0 && delta != 0 {
                next.sign[c][i] = bits.get(1)? != 0;
            }
            let mag = next.known_mag[c][i] as u32 | ((delta as u32) << new_shift);
            if mag > 32768 || (mag == 32768 && !next.sign[c][i]) {
                return Err(Error("coefficient overflow"));
            }
            next.known_mag[c][i] = mag as u16;
        }
    }
    bits.finish()?;
    *state = next;
    Ok(())
}
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    offset: usize,
}
impl BitWriter {
    fn put(&mut self, value: u16, count: u8) {
        for b in 0..count {
            if self.offset % 8 == 0 {
                self.bytes.push(0);
            }
            self.bytes[self.offset / 8] |= (((value >> b) & 1) as u8) << (self.offset % 8);
            self.offset += 1;
        }
    }
}
struct BitReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl BitReader<'_> {
    fn get(&mut self, count: u8) -> Result<u16> {
        if count > 16 || self.offset + count as usize > self.bytes.len() * 8 {
            return Err(Error("truncated coefficient bitstream"));
        }
        let mut value = 0;
        for b in 0..count {
            value |= (((self.bytes[self.offset / 8] >> (self.offset % 8)) & 1) as u16) << b;
            self.offset += 1;
        }
        Ok(value)
    }
    fn finish(self) -> Result<()> {
        if self.offset.div_ceil(8) != self.bytes.len() {
            return Err(Error("trailing coefficient bitstream"));
        }
        if self.offset % 8 != 0
            && self
                .bytes
                .last()
                .is_some_and(|b| b >> (self.offset % 8) != 0)
        {
            return Err(Error("nonzero coefficient padding"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pixels() -> Vec<u8> {
        (0..TILE_RGB_BYTES)
            .map(|i| ((i * 173 + i / 9 * 37) & 255) as u8)
            .collect()
    }
    #[test]
    fn reversible_math_and_multistep_refinement() {
        for r in [0, 1, 127, 254, 255] {
            for g in 0..=255 {
                for b in [0, 1, 127, 254, 255] {
                    let rgb = [r, g, b];
                    assert_eq!(inverse_ycocg(forward_ycocg(rgb)), rgb);
                }
            }
        }
        let rgb = pixels();
        let coeff = rgb_to_wavelet(&rgb).unwrap();
        assert_eq!(wavelet_to_rgb(&coeff).unwrap(), rgb);
        let init = encode_wavelet_init(&coeff, 3, 5).unwrap();
        let mut state = decode_wavelet_init(&init, 3, 5).unwrap();
        let before = state.clone();
        let _ = state.reconstruct().unwrap();
        assert_eq!(state, before);
        for c in 0..3 {
            for y in 0..8 {
                for x in 0..8 {
                    assert_eq!(
                        state.known_mag[c][y * 64 + x],
                        coeff.planes[c][y * 64 + x].unsigned_abs()
                    );
                }
            }
        }
        let refine = encode_wavelet_refine(&coeff, 3, 5, 1, 2).unwrap();
        decode_wavelet_refine(&mut state, &refine, 1, 2).unwrap();
        let refine = encode_wavelet_refine(&coeff, 1, 2, 0, 0).unwrap();
        decode_wavelet_refine(&mut state, &refine, 0, 0).unwrap();
        assert_eq!(state.reconstruct().unwrap(), rgb);
    }
    #[test]
    fn edge_padding_formats_and_fast_modes() {
        let image = vec![9, 8, 7, 6, 5, 4];
        let tile = extract_tile(&image, 2, 1, 6, PixelFormat::Rgb, 0).unwrap();
        assert_eq!(&tile[3..6], &[6, 5, 4]);
        assert_eq!(&tile[TILE_RGB_BYTES - 3..], &[6, 5, 4]);
        let mut bgra = vec![0; 8];
        write_tile(&tile, &mut bgra, 2, 1, 8, PixelFormat::Bgra, 0).unwrap();
        assert_eq!(bgra, [7, 8, 9, 255, 4, 5, 6, 255]);
        assert_eq!(
            extract_tile(&bgra, 2, 1, 8, PixelFormat::Bgra, 0).unwrap(),
            tile
        );
        let (colors, mask) = detect_two_color(&tile).unwrap();
        assert_eq!(restore_two_color(colors, &mask), tile);
        assert_eq!(detect_solid(&restore_solid([255, 1, 0])), Some([255, 1, 0]));
    }
    #[test]
    fn malformed_bits_fail_without_mutating_refinement() {
        let coeff = rgb_to_wavelet(&pixels()).unwrap();
        let init = encode_wavelet_init(&coeff, 3, 5).unwrap();
        assert!(decode_wavelet_init(&init[..init.len() - 1], 3, 5).is_err());
        let mut state = decode_wavelet_init(&init, 3, 5).unwrap();
        let old = state.clone();
        assert!(decode_wavelet_refine(&mut state, &[0], 0, 0).is_err());
        assert_eq!(state, old);
    }
}
