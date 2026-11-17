use super::{GpuContext, GpuEncoder};
use crate::{
    math::{self, WaveletCoefficients},
    Error, Result,
};
use std::{
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
};

// Bound staging and coefficient buffers independently of desktop resolution.
pub(crate) const BATCH_TILES: usize = 64;
const ATLAS_SIDE: usize = 512;

pub(crate) struct ProductionGpu {
    pub ctx: Arc<GpuContext>,
    encoder: GpuEncoder,
    prepared: Vec<Option<WaveletCoefficients>>,
    pub dispatches: u64,
}

impl ProductionGpu {
    // shortcut: panic=abort cannot unwind; isolate the GPU worker if panic recovery is required.
    pub fn new() -> Result<Self> {
        catch_unwind(AssertUnwindSafe(|| {
            let ctx = GpuContext::new_hardware()?;
            let mut encoder = GpuEncoder::new(ctx.clone(), ATLAS_SIDE as u16, ATLAS_SIDE as u16)?;
            encoder.set_quality(0, 0, true);
            if ctx.is_lost() {
                return Err(Error("GPU pipeline initialization failed"));
            }
            Ok(Self {
                ctx,
                encoder,
                prepared: Vec::new(),
                dispatches: 0,
            })
        }))
        .map_err(|_| Error("GPU initialization failed"))?
    }

    pub fn backend(&self) -> &'static str {
        if self.ctx.is_lost() {
            return "CpuFallback";
        }
        match self.ctx.backend.as_str() {
            "Vulkan" => "GpuVulkan",
            "Dx12" => "GpuDx12",
            "Metal" => "GpuMetal",
            _ => "Unknown",
        }
    }

    pub fn begin_frame(&mut self, count: usize) {
        self.prepared = vec![None; count];
    }

    pub fn prepare_batch(&mut self, batch: &[(usize, Vec<u8>)]) -> Result<()> {
        let coeffs = self.transform(batch)?;
        for ((index, _), coeff) in batch.iter().zip(coeffs) {
            self.prepared[*index] = Some(coeff);
        }
        Ok(())
    }

    pub fn coefficients(&mut self, index: usize, rgb: &[u8]) -> Result<WaveletCoefficients> {
        if let Some(coeff) = self.prepared.get_mut(index).and_then(Option::take) {
            return Ok(coeff);
        }
        // A UDP retransmission can need full coefficients after a lossless send.
        self.transform(&[(index, rgb.to_vec())])?
            .pop()
            .ok_or(Error("missing GPU coefficients"))
    }

    fn transform(&mut self, tiles: &[(usize, Vec<u8>)]) -> Result<Vec<WaveletCoefficients>> {
        if self.ctx.is_lost() {
            return Err(Error("GPU device lost"));
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut rgba = vec![0; ATLAS_SIDE * ATLAS_SIDE * 4];
            for (slot, (_, rgb)) in tiles.iter().enumerate() {
                if rgb.len() != math::TILE_RGB_BYTES {
                    return Err(Error("invalid GPU tile"));
                }
                for (pixel, color) in rgb.chunks_exact(3).enumerate() {
                    let x = slot % 8 * 64 + pixel % 64;
                    let y = slot / 8 * 64 + pixel / 64;
                    rgba[(y * ATLAS_SIDE + x) * 4..(y * ATLAS_SIDE + x) * 4 + 3]
                        .copy_from_slice(color);
                }
            }
            self.encoder.upload_frame_slot(0, &rgba)?;
            self.encoder.last_ring_slot = 0;
            let out = &self.encoder.outs[0];
            // CPU equality/classification is authoritative; bypass the prototype's sampled scan.
            let list: Vec<u32> = (0..tiles.len() as u32).flat_map(|i| [i, 0]).collect();
            self.ctx
                .queue
                .write_buffer(&out.list, 0, bytemuck::cast_slice(&list));
            let mut cmd = self.ctx.device.create_command_encoder(&Default::default());
            {
                let mut pass = cmd.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("gdesk_protocol_wavelet"),
                    timestamp_writes: None,
                });
                pass.set_pipeline(&self.encoder.pack);
                pass.set_bind_group(0, &self.encoder.slots[0].pack_binds[0], &[]);
                pass.dispatch_workgroups(tiles.len() as u32, 1, 1);
            }
            self.ctx.queue.submit([cmd.finish()]);
            self.dispatches += 1;
            let gpu_tiles = self.encoder.read_tiles(tiles.len() as u32)?;
            let mut output = vec![None; tiles.len()];
            for tile in gpu_tiles {
                let index = tile.tile as usize;
                if index >= tiles.len() {
                    continue;
                }
                let coeff = {
                    let mut planes = std::array::from_fn(|_| Vec::with_capacity(math::TILE_PIXELS));
                    for (dst, src) in planes.iter_mut().zip(tile.planes) {
                        for value in src {
                            dst.push(
                                i16::try_from(value)
                                    .map_err(|_| Error("GPU coefficient overflow"))?,
                            );
                        }
                    }
                    WaveletCoefficients { planes }
                };
                output[index] = Some(coeff);
            }
            output
                .into_iter()
                .map(|c| c.ok_or(Error("missing GPU tile")))
                .collect()
        }))
        .unwrap_or(Err(Error("GPU operation panicked")));
        if result.is_err() {
            self.ctx
                .failed
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if self.ctx.is_lost() {
            return Err(Error("GPU device lost"));
        }
        result
    }
}
