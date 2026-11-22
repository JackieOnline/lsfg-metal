// scene brightness probe for the day/night domain switch.
//
// one scaled blit per sample (64x36, ~2304 evenly spread points) reads the
// raw per-channel PQ codes of the freshly imported frame - recorded before
// any domain conversion, so the metric means the same thing in either
// mode. the cpu averages the luma one iteration later, when the acquire
// chain has already proven the sample complete: no blocking, no extra
// sync objects, one blit of cost every ~second.

use std::sync::Arc;

use ash::vk;

use crate::generator::Instance;
use crate::vkutil::{self, check};

const PROBE_W: u32 = 64;
const PROBE_H: u32 = 36;

pub struct Probe {
    inst: Arc<Instance>,
    image: vk::Image,
    memory: vk::DeviceMemory,
    buffer: vk::Buffer,
    buffer_memory: vk::DeviceMemory,
}

impl Probe {
    pub fn new(inst: Arc<Instance>) -> Result<Probe, String> {
        let d = &inst.device;
        let image = vkutil::create_image(
            d,
            vk::Format::R8G8B8A8_UNORM,
            (PROBE_W, PROBE_H),
            1,
            vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC,
        )?;
        let req = unsafe { d.get_image_memory_requirements(image) };
        let memory = vkutil::allocate(
            &inst.instance,
            inst.physical_device,
            d,
            req.size,
            req.memory_type_bits,
            false,
            Some(image),
        )?;
        check(unsafe { d.bind_image_memory(image, memory, 0) }, "vkBindImageMemory")?;
        let bytes = vec![0u8; (PROBE_W * PROBE_H * 4) as usize];
        let (buffer, buffer_memory) = vkutil::create_buffer(
            &inst.instance,
            inst.physical_device,
            d,
            vk::BufferUsageFlags::TRANSFER_DST,
            &bytes,
        )?;
        Ok(Probe {
            inst,
            image,
            memory,
            buffer,
            buffer_memory,
        })
    }

    // record the sample into the import command buffer; runs right after
    // the import blit so it measures the raw PQ codes of the new frame.
    // the caller must transition source from transfer-read onward.
    pub fn record(
        &self,
        d: &ash::Device,
        cb: vk::CommandBuffer,
        source: vk::Image,
        layer: u32,
        extent: (u32, u32),
    ) {
        use vk::{AccessFlags as A, ImageLayout as L, PipelineStageFlags as P};
        unsafe {
            let pre = [
                vkutil::image_barrier(
                    A::TRANSFER_WRITE,
                    A::TRANSFER_READ,
                    L::GENERAL,
                    L::GENERAL,
                    source,
                    layer,
                    1,
                ),
                vkutil::image_barrier(
                    A::NONE,
                    A::TRANSFER_WRITE,
                    L::UNDEFINED,
                    L::GENERAL,
                    self.image,
                    0,
                    1,
                ),
            ];
            d.cmd_pipeline_barrier(
                cb,
                P::TRANSFER,
                P::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &pre,
            );
            let blit = vk::ImageBlit {
                src_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: layer,
                    layer_count: 1,
                },
                src_offsets: [
                    vk::Offset3D { x: 0, y: 0, z: 0 },
                    vk::Offset3D {
                        x: extent.0 as i32,
                        y: extent.1 as i32,
                        z: 1,
                    },
                ],
                dst_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                dst_offsets: [
                    vk::Offset3D { x: 0, y: 0, z: 0 },
                    vk::Offset3D {
                        x: PROBE_W as i32,
                        y: PROBE_H as i32,
                        z: 1,
                    },
                ],
                ..Default::default()
            };
            d.cmd_blit_image(
                cb,
                source,
                L::GENERAL,
                self.image,
                L::GENERAL,
                &[blit],
                vk::Filter::LINEAR,
            );
            let post = [vkutil::image_barrier(
                A::TRANSFER_WRITE,
                A::TRANSFER_READ,
                L::GENERAL,
                L::GENERAL,
                self.image,
                0,
                1,
            )];
            d.cmd_pipeline_barrier(
                cb,
                P::TRANSFER,
                P::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &post,
            );
            let region = vk::BufferImageCopy {
                buffer_offset: 0,
                buffer_row_length: 0,
                buffer_image_height: 0,
                image_subresource: vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                },
                image_offset: vk::Offset3D { x: 0, y: 0, z: 0 },
                image_extent: vk::Extent3D {
                    width: PROBE_W,
                    height: PROBE_H,
                    depth: 1,
                },
                ..Default::default()
            };
            d.cmd_copy_image_to_buffer(cb, self.image, L::GENERAL, self.buffer, &[region]);
        }
    }

    // average scene luma as an R8 byte (PQ code * 255) and the fraction of
    // samples above the bright band. this game grades day and night with
    // overlapping frame averages, so the bright coverage is the day/night
    // discriminator: day skies and glints cover tens of percent, torches
    // well under five. call one iteration after record; the acquire chain
    // has proven the sample complete
    pub fn read(&self) -> Result<(f32, f32), String> {
        let d = &self.inst.device;
        let len = (PROBE_W * PROBE_H * 4) as u64;
        let p = check(
            unsafe { d.map_memory(self.buffer_memory, 0, len, vk::MemoryMapFlags::empty()) },
            "vkMapMemory (probe)",
        )? as *mut u8;
        let bytes = unsafe { std::slice::from_raw_parts(p, len as usize) };
        let n = (PROBE_W * PROBE_H) as u32;
        let (mut sum, mut bright) = (0f32, 0u32);
        for px in bytes.chunks_exact(4) {
            let l = 0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32;
            sum += l;
            if l > 110.0 {
                bright += 1;
            }
        }
        unsafe { d.unmap_memory(self.buffer_memory) };
        Ok((sum / n as f32, bright as f32 / n as f32))
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let d = &self.inst.device;
        unsafe {
            d.destroy_image(self.image, None);
            d.free_memory(self.memory, None);
            d.destroy_buffer(self.buffer, None);
            d.free_memory(self.buffer_memory, None);
        }
    }
}
