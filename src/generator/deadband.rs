// temporal deadband pass for 10-bit PQ layers.
//
// The game's HDR pipeline dithers each frame; the flow matcher reads the
// dither as motion and warps static content - straight lines bend in
// generated frames, measured identically across every domain route, so
// the disease lives upstream of any curve. The pass runs right after the
// import blit: the freshly imported layer is compared against the other
// source layer (the previous frame), and pixels that changed less than
// the threshold snap to the previous value. Static content becomes
// exactly equal between the two frames, the flow estimates zero motion
// there, and generated frames become geometrically exact copies of
// stillness. Real motion exceeds the threshold and passes through.

use std::sync::Arc;

use ash::vk;

use super::Instance;
use crate::vkutil::{self, check};

const DEADBAND: &[u8] = include_bytes!("deadband.spv");

fn words(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

// storage view of one source layer; rgba16f holding raw pq codes
fn view(d: &ash::Device, image: vk::Image, base_layer: u32) -> Result<vk::ImageView, String> {
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(vk::Format::R16G16B16A16_SFLOAT)
        .subresource_range(vkutil::color_range(base_layer, 1));
    check(unsafe { d.create_image_view(&info, None) }, "vkCreateImageView (deadband)")
}

pub struct Deadband {
    inst: Arc<Instance>,
    extent: (u32, u32),
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    pool: vk::DescriptorPool,
    sets: [vk::DescriptorSet; 2],
    views: Vec<vk::ImageView>,
}

impl Deadband {
    // source is the 2-layer import image; cur/prev alternate per frame
    pub fn new(
        inst: Arc<Instance>,
        source: vk::Image,
        extent: (u32, u32),
    ) -> Result<Deadband, String> {
        let d = &inst.device;
        let views = [0u32, 1]
            .iter()
            .map(|&l| view(d, source, l))
            .collect::<Result<Vec<_>, _>>()?;

        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];
        let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        let set_layout = check(
            unsafe { d.create_descriptor_set_layout(&info, None) },
            "vkCreateDescriptorSetLayout (deadband)",
        )?;

        let layouts = [set_layout];
        let info = vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts);
        let layout = check(
            unsafe { d.create_pipeline_layout(&info, None) },
            "vkCreatePipelineLayout (deadband)",
        )?;

        let sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::STORAGE_IMAGE,
            descriptor_count: 4,
        }];
        let info = vk::DescriptorPoolCreateInfo::default()
            .pool_sizes(&sizes)
            .max_sets(2);
        let pool = check(
            unsafe { d.create_descriptor_pool(&info, None) },
            "vkCreateDescriptorPool (deadband)",
        )?;

        let layouts = [set_layout, set_layout];
        let info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);
        let sets = check(
            unsafe { d.allocate_descriptor_sets(&info) },
            "vkAllocateDescriptorSets (deadband)",
        )?;
        unsafe { d.destroy_descriptor_set_layout(set_layout, None) };

        // set 0: cur = layer 0, prev = layer 1; set 1: cur = layer 1, prev = layer 0
        let infos: Vec<Vec<vk::DescriptorImageInfo>> = [(0usize, 1usize), (1usize, 0usize)]
            .iter()
            .map(|&(cur, prev)| {
                vec![
                    vk::DescriptorImageInfo::default()
                        .image_view(views[cur])
                        .image_layout(vk::ImageLayout::GENERAL),
                    vk::DescriptorImageInfo::default()
                        .image_view(views[prev])
                        .image_layout(vk::ImageLayout::GENERAL),
                ]
            })
            .collect();
        let mut writes: Vec<vk::WriteDescriptorSet> = Vec::new();
        for (si, &s) in sets.iter().enumerate() {
            for b in 0..2usize {
                let slice = std::slice::from_ref(&infos[si][b]);
                writes.push(
                    vk::WriteDescriptorSet::default()
                        .dst_set(s)
                        .dst_binding(b as u32)
                        .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                        .image_info(slice),
                );
            }
        }
        unsafe { d.update_descriptor_sets(&writes, &[]) };

        // pipeline
        let code = words(DEADBAND);
        let info = vk::ShaderModuleCreateInfo::default().code(&code);
        let module = check(
            unsafe { d.create_shader_module(&info, None) },
            "vkCreateShaderModule (deadband)",
        )?;
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(module)
            .name(c"main");
        let info = vk::ComputePipelineCreateInfo::default().stage(stage).layout(layout);
        let pipelines = match unsafe {
            d.create_compute_pipelines(vk::PipelineCache::null(), &[info], None)
        } {
            Ok(ps) => ps,
            Err((_, e)) => return Err(format!("vkCreateComputePipelines failed: {e:?}")),
        };
        unsafe { d.destroy_shader_module(module, None) };
        let pipeline = pipelines[0];

        let mut set_arr = [vk::DescriptorSet::null(); 2];
        set_arr.copy_from_slice(&sets);

        Ok(Deadband {
            inst,
            extent,
            layout,
            pipeline,
            pool,
            sets: set_arr,
            views,
        })
    }

    // record the pass on the freshly imported layer; the caller owns the
    // barriers (cur: transfer_write -> shader read/write, prev: none ->
    // shader read) and the image must be in general layout
    pub fn apply(&self, cb: vk::CommandBuffer, layer: u32) {
        let d = &self.inst.device;
        unsafe {
            d.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, self.pipeline);
            d.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::COMPUTE,
                self.layout,
                0,
                &[self.sets[layer as usize]],
                &[],
            );
            d.cmd_dispatch(cb, (self.extent.0 + 7) / 8, (self.extent.1 + 7) / 8, 1);
        }
    }
}

impl Drop for Deadband {
    fn drop(&mut self) {
        let d = &self.inst.device;
        unsafe {
            d.destroy_pipeline(self.pipeline, None);
            d.destroy_pipeline_layout(self.layout, None);
            d.destroy_descriptor_pool(self.pool, None);
            for &v in &self.views {
                d.destroy_image_view(v, None);
            }
        }
    }
}
