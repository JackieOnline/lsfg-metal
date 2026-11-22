// PQ <-> domain conversion compute passes for the 16F route (day mode).
//
// Day converts codes to the gamma-2.6 domain after the import blit and
// back before the export blit; night skips the passes entirely and keeps
// raw PQ codes, where losslessness and dark amplification live. The 8-bit
// route through generate_8bit was tried and colour-failed on this game
// (parked; would need the frame dump to diagnose); the transfer here is
// per channel and invertible, real frames never pass through these
// images, and generated frames round-trip within half a 10-bit step.

use std::sync::Arc;

use ash::vk;

use super::Instance;
use crate::vkutil::{self, check};

const TO_DOM: &[u8] = include_bytes!("pq2dom.spv");
const TO_PQ: &[u8] = include_bytes!("dom2pq.spv");

fn words(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

// storage view with an explicit base layer; the import image alternates
fn create_view(d: &ash::Device, image: vk::Image, base_layer: u32) -> Result<vk::ImageView, String> {
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(vk::Format::R16G16B16A16_SFLOAT)
        .subresource_range(vkutil::color_range(base_layer, 1));
    check(unsafe { d.create_image_view(&info, None) }, "vkCreateImageView (pq)")
}

fn pipeline(d: &ash::Device, layout: vk::PipelineLayout, bytes: &[u8]) -> Result<vk::Pipeline, String> {
    let code = words(bytes);
    let info = vk::ShaderModuleCreateInfo::default().code(&code);
    let module = check(
        unsafe { d.create_shader_module(&info, None) },
        "vkCreateShaderModule (pq)",
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
    Ok(pipelines[0])
}

pub struct Convert {
    inst: Arc<Instance>,
    extent: (u32, u32),
    layout: vk::PipelineLayout,
    to_dom: vk::Pipeline,
    to_pq: vk::Pipeline,
    pool: vk::DescriptorPool,
    sets: [vk::DescriptorSet; 3],
    views: Vec<vk::ImageView>,
}

impl Convert {
    // source is the 2-layer import image, dest the single-layer output image;
    // both are rgba16f, the pipeline this route has been validated on.
    // note: dest holds one generated frame per real frame (multiplier 2).
    pub fn new(
        inst: Arc<Instance>,
        source: vk::Image,
        dest: vk::Image,
        extent: (u32, u32),
    ) -> Result<Convert, String> {
        let d = &inst.device;
        // views: source layer 0, source layer 1, dest
        let views = [(source, 0u32), (source, 1), (dest, 0)]
            .iter()
            .map(|&(image, layer)| create_view(d, image, layer))
            .collect::<Result<Vec<_>, _>>()?;

        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::COMPUTE)];
        let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        let set_layout = check(
            unsafe { d.create_descriptor_set_layout(&info, None) },
            "vkCreateDescriptorSetLayout (pq)",
        )?;

        let layouts = [set_layout];
        let info = vk::PipelineLayoutCreateInfo::default().set_layouts(&layouts);
        let layout = check(
            unsafe { d.create_pipeline_layout(&info, None) },
            "vkCreatePipelineLayout (pq)",
        )?;

        let sizes = [vk::DescriptorPoolSize {
            ty: vk::DescriptorType::STORAGE_IMAGE,
            descriptor_count: 3,
        }];
        let info = vk::DescriptorPoolCreateInfo::default()
            .pool_sizes(&sizes)
            .max_sets(3);
        let pool = check(
            unsafe { d.create_descriptor_pool(&info, None) },
            "vkCreateDescriptorPool (pq)",
        )?;

        let layouts = [set_layout, set_layout, set_layout];
        let info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);
        let sets = check(
            unsafe { d.allocate_descriptor_sets(&info) },
            "vkAllocateDescriptorSets (pq)",
        )?;
        unsafe { d.destroy_descriptor_set_layout(set_layout, None) };

        let infos: Vec<_> = views
            .iter()
            .map(|&v| {
                vk::DescriptorImageInfo::default()
                    .image_view(v)
                    .image_layout(vk::ImageLayout::GENERAL)
            })
            .collect();
        let ws: Vec<_> = sets
            .iter()
            .zip(infos.iter())
            .map(|(&s, i)| {
                vk::WriteDescriptorSet::default()
                    .dst_set(s)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                    .image_info(std::slice::from_ref(i))
            })
            .collect();
        unsafe { d.update_descriptor_sets(&ws, &[]) };

        let to_dom = pipeline(d, layout, TO_DOM)?;
        let to_pq = pipeline(d, layout, TO_PQ)?;

        let mut set_arr = [vk::DescriptorSet::null(); 3];
        set_arr.copy_from_slice(&sets);

        Ok(Convert {
            inst,
            extent,
            layout,
            to_dom,
            to_pq,
            pool,
            sets: set_arr,
            views,
        })
    }

    // record source[layer]: PQ codes -> hybrid domain. the caller owns the
    // image barriers; the image must be in GENERAL for compute read/write
    pub fn to_dom(&self, cb: vk::CommandBuffer, layer: u32) {
        let d = &self.inst.device;
        unsafe {
            d.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, self.to_dom);
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

    // record dest: hybrid domain -> PQ codes. the caller owns the image barriers
    pub fn to_pq(&self, cb: vk::CommandBuffer) {
        let d = &self.inst.device;
        unsafe {
            d.cmd_bind_pipeline(cb, vk::PipelineBindPoint::COMPUTE, self.to_pq);
            d.cmd_bind_descriptor_sets(
                cb,
                vk::PipelineBindPoint::COMPUTE,
                self.layout,
                0,
                &[self.sets[2]],
                &[],
            );
            d.cmd_dispatch(cb, (self.extent.0 + 7) / 8, (self.extent.1 + 7) / 8, 1);
        }
    }
}

impl Drop for Convert {
    fn drop(&mut self) {
        // the Arc<Instance> keeps the shared device alive through field drops
        let d = &self.inst.device;
        unsafe {
            d.destroy_pipeline(self.to_dom, None);
            d.destroy_pipeline(self.to_pq, None);
            d.destroy_pipeline_layout(self.layout, None);
            d.destroy_descriptor_pool(self.pool, None);
            for &v in &self.views {
                d.destroy_image_view(v, None);
            }
        }
    }
}
