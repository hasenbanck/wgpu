/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::{
    command::{
        bind::{Binder, LayoutChange},
        PassComponent, PhantomSlice, RawPass, RawRenderPassColorAttachmentDescriptor,
        RawRenderPassDepthStencilAttachmentDescriptor, RawRenderTargets,
    },
    conv,
    device::{
        AttachmentData, FramebufferKey, RenderPassContext, RenderPassKey, MAX_COLOR_TARGETS,
        MAX_VERTEX_BUFFERS,
    },
    hub::{GfxBackend, Global, GlobalIdentityHandlerFactory, Token},
    id,
    pipeline::PipelineFlags,
    resource::{BufferUse, TextureUse, TextureViewInner},
    track::TrackerSet,
    Stored,
};

use arrayvec::ArrayVec;
use hal::command::CommandBuffer as _;
use peek_poke::{Peek, PeekPoke, Poke};
use wgt::{
    BufferAddress, BufferSize, BufferUsage, Color, DynamicOffset, IndexFormat, InputStepMode,
    LoadOp, RenderPassColorAttachmentDescriptorBase,
    RenderPassDepthStencilAttachmentDescriptorBase, StoreOp, TextureUsage, BIND_BUFFER_ALIGNMENT,
};

use std::{borrow::Borrow, collections::hash_map::Entry, fmt, iter, mem, ops::Range, slice, str};

pub type RenderPassColorAttachmentDescriptor =
    RenderPassColorAttachmentDescriptorBase<id::TextureViewId>;
pub type RenderPassDepthStencilAttachmentDescriptor =
    RenderPassDepthStencilAttachmentDescriptorBase<id::TextureViewId>;

fn is_depth_stencil_read_only(
    desc: &RenderPassDepthStencilAttachmentDescriptor,
    aspects: hal::format::Aspects,
) -> bool {
    if aspects.contains(hal::format::Aspects::DEPTH) && !desc.depth_read_only {
        return false;
    }
    assert_eq!(
        (desc.depth_load_op, desc.depth_store_op),
        (LoadOp::Load, StoreOp::Store),
        "Unable to clear non-present/read-only depth"
    );
    if aspects.contains(hal::format::Aspects::STENCIL) && !desc.stencil_read_only {
        return false;
    }
    assert_eq!(
        (desc.stencil_load_op, desc.stencil_store_op),
        (LoadOp::Load, StoreOp::Store),
        "Unable to clear non-present/read-only stencil"
    );
    true
}

#[repr(C)]
#[derive(Debug)]
pub struct RenderPassDescriptor<'a> {
    pub color_attachments: *const RenderPassColorAttachmentDescriptor,
    pub color_attachments_length: usize,
    pub depth_stencil_attachment: Option<&'a RenderPassDepthStencilAttachmentDescriptor>,
}

#[derive(Clone, Copy, Debug, Default, PeekPoke)]
#[cfg_attr(feature = "trace", derive(serde::Serialize))]
#[cfg_attr(feature = "replay", derive(serde::Deserialize))]
pub struct Rect<T> {
    pub x: T,
    pub y: T,
    pub w: T,
    pub h: T,
}

#[derive(Clone, Copy, Debug, PeekPoke)]
#[cfg_attr(feature = "trace", derive(serde::Serialize))]
#[cfg_attr(feature = "replay", derive(serde::Deserialize))]
pub enum RenderCommand {
    SetBindGroup {
        index: u8,
        num_dynamic_offsets: u8,
        bind_group_id: id::BindGroupId,
        #[cfg_attr(any(feature = "trace", feature = "replay"), serde(skip))]
        phantom_offsets: PhantomSlice<DynamicOffset>,
    },
    SetPipeline(id::RenderPipelineId),
    SetIndexBuffer {
        buffer_id: id::BufferId,
        offset: BufferAddress,
        size: BufferSize,
    },
    SetVertexBuffer {
        slot: u32,
        buffer_id: id::BufferId,
        offset: BufferAddress,
        size: BufferSize,
    },
    SetBlendColor(Color),
    SetStencilReference(u32),
    SetViewport {
        rect: Rect<f32>,
        //TODO: use half-float to reduce the size?
        depth_min: f32,
        depth_max: f32,
    },
    SetScissor(Rect<u32>),
    Draw {
        vertex_count: u32,
        instance_count: u32,
        first_vertex: u32,
        first_instance: u32,
    },
    DrawIndexed {
        index_count: u32,
        instance_count: u32,
        first_index: u32,
        base_vertex: i32,
        first_instance: u32,
    },
    DrawIndirect {
        buffer_id: id::BufferId,
        offset: BufferAddress,
    },
    DrawIndexedIndirect {
        buffer_id: id::BufferId,
        offset: BufferAddress,
    },
    PushDebugGroup {
        color: u32,
        len: usize,
        #[cfg_attr(any(feature = "trace", feature = "replay"), serde(skip))]
        phantom_marker: PhantomSlice<u8>,
    },
    PopDebugGroup,
    InsertDebugMarker {
        color: u32,
        len: usize,
        #[cfg_attr(any(feature = "trace", feature = "replay"), serde(skip))]
        phantom_marker: PhantomSlice<u8>,
    },
    ExecuteBundle(id::RenderBundleId),
    End,
}

// required for PeekPoke
impl Default for RenderCommand {
    fn default() -> Self {
        RenderCommand::End
    }
}

impl RawPass<id::CommandEncoderId> {
    pub unsafe fn new_render(parent_id: id::CommandEncoderId, desc: &RenderPassDescriptor) -> Self {
        let mut pass = Self::new::<RenderCommand>(parent_id);

        let mut targets: RawRenderTargets = mem::zeroed();
        if let Some(ds) = desc.depth_stencil_attachment {
            targets.depth_stencil = RawRenderPassDepthStencilAttachmentDescriptor {
                attachment: ds.attachment.into_raw(),
                depth: PassComponent {
                    load_op: ds.depth_load_op,
                    store_op: ds.depth_store_op,
                    clear_value: ds.clear_depth,
                    read_only: ds.depth_read_only,
                },
                stencil: PassComponent {
                    load_op: ds.stencil_load_op,
                    store_op: ds.stencil_store_op,
                    clear_value: ds.clear_stencil,
                    read_only: ds.stencil_read_only,
                },
            };
        }

        for (color, at) in targets.colors.iter_mut().zip(slice::from_raw_parts(
            desc.color_attachments,
            desc.color_attachments_length,
        )) {
            *color = RawRenderPassColorAttachmentDescriptor {
                attachment: at.attachment.into_raw(),
                resolve_target: at.resolve_target.map_or(0, |id| id.into_raw()),
                component: PassComponent {
                    load_op: at.load_op,
                    store_op: at.store_op,
                    clear_value: at.clear_color,
                    read_only: false,
                },
            };
        }

        pass.encode(&targets);
        pass
    }
}

impl<P: Copy> RawPass<P> {
    pub unsafe fn fill_render_commands(
        &mut self,
        commands: &[RenderCommand],
        mut offsets: &[DynamicOffset],
    ) {
        for com in commands {
            self.encode(com);
            if let RenderCommand::SetBindGroup {
                num_dynamic_offsets,
                ..
            } = *com
            {
                self.encode_slice(&offsets[..num_dynamic_offsets as usize]);
                offsets = &offsets[num_dynamic_offsets as usize..];
            }
        }
    }

    pub unsafe fn finish_render(mut self) -> (Vec<u8>, P) {
        self.finish(RenderCommand::End);
        self.into_vec()
    }
}

#[derive(Debug, PartialEq)]
enum OptionalState {
    Unused,
    Required,
    Set,
}

impl OptionalState {
    fn require(&mut self, require: bool) {
        if require && *self == OptionalState::Unused {
            *self = OptionalState::Required;
        }
    }
}

#[derive(PartialEq)]
enum DrawError {
    MissingBlendColor,
    MissingStencilReference,
    MissingPipeline,
    IncompatibleBindGroup {
        index: u32,
        //expected: BindGroupLayoutId,
        //provided: Option<(BindGroupLayoutId, BindGroupId)>,
    },
}

impl fmt::Debug for DrawError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DrawError::MissingBlendColor => write!(f, "MissingBlendColor. A blend color is required to be set using RenderPass::set_blend_color."),
            DrawError::MissingStencilReference => write!(f, "MissingStencilReference. A stencil reference is required to be set using RenderPass::set_stencil_reference."),
            DrawError::MissingPipeline => write!(f, "MissingPipeline. You must first set the render pipeline using RenderPass::set_pipeline."),
            DrawError::IncompatibleBindGroup { index } => write!(f, "IncompatibleBindGroup. The current render pipeline has a layout which is incompatible with a currently set bind group. They first differ at entry index {}.", index),
        }
    }
}

#[derive(Debug, Default)]
struct IndexState {
    bound_buffer_view: Option<(id::BufferId, Range<BufferAddress>)>,
    format: IndexFormat,
    limit: u32,
}

impl IndexState {
    fn update_limit(&mut self) {
        self.limit = match self.bound_buffer_view {
            Some((_, ref range)) => {
                let shift = match self.format {
                    IndexFormat::Uint16 => 1,
                    IndexFormat::Uint32 => 2,
                };
                ((range.end - range.start) >> shift) as u32
            }
            None => 0,
        }
    }

    fn reset(&mut self) {
        self.bound_buffer_view = None;
        self.limit = 0;
    }
}

#[derive(Clone, Copy, Debug)]
struct VertexBufferState {
    total_size: BufferAddress,
    stride: BufferAddress,
    rate: InputStepMode,
}

impl VertexBufferState {
    const EMPTY: Self = VertexBufferState {
        total_size: 0,
        stride: 0,
        rate: InputStepMode::Vertex,
    };
}

#[derive(Debug, Default)]
struct VertexState {
    inputs: ArrayVec<[VertexBufferState; MAX_VERTEX_BUFFERS]>,
    vertex_limit: u32,
    instance_limit: u32,
}

impl VertexState {
    fn update_limits(&mut self) {
        self.vertex_limit = !0;
        self.instance_limit = !0;
        for vbs in &self.inputs {
            if vbs.stride == 0 {
                continue;
            }
            let limit = (vbs.total_size / vbs.stride) as u32;
            match vbs.rate {
                InputStepMode::Vertex => self.vertex_limit = self.vertex_limit.min(limit),
                InputStepMode::Instance => self.instance_limit = self.instance_limit.min(limit),
            }
        }
    }

    fn reset(&mut self) {
        self.inputs.clear();
        self.vertex_limit = 0;
        self.instance_limit = 0;
    }
}

#[derive(Debug)]
struct State {
    binder: Binder,
    blend_color: OptionalState,
    stencil_reference: OptionalState,
    pipeline: OptionalState,
    index: IndexState,
    vertex: VertexState,
    debug_scope_depth: u32,
}

impl State {
    fn is_ready(&self) -> Result<(), DrawError> {
        //TODO: vertex buffers
        let bind_mask = self.binder.invalid_mask();
        if bind_mask != 0 {
            //let (expected, provided) = self.binder.entries[index as usize].info();
            return Err(DrawError::IncompatibleBindGroup {
                index: bind_mask.trailing_zeros(),
            });
        }
        if self.pipeline == OptionalState::Required {
            return Err(DrawError::MissingPipeline);
        }
        if self.blend_color == OptionalState::Required {
            return Err(DrawError::MissingBlendColor);
        }
        if self.stencil_reference == OptionalState::Required {
            return Err(DrawError::MissingStencilReference);
        }
        Ok(())
    }

    /// Reset the `RenderBundle`-related states.
    fn reset_bundle(&mut self) {
        self.binder.reset();
        self.pipeline = OptionalState::Required;
        self.index.reset();
        self.vertex.reset();
    }
}

// Common routines between render/compute

impl<G: GlobalIdentityHandlerFactory> Global<G> {
    pub fn command_encoder_run_render_pass<B: GfxBackend>(
        &self,
        encoder_id: id::CommandEncoderId,
        raw_data: &[u8],
    ) {
        let hub = B::hub(self);
        let mut token = Token::root();

        let (device_guard, mut token) = hub.devices.read(&mut token);
        let (mut cmb_guard, mut token) = hub.command_buffers.write(&mut token);

        let mut trackers = TrackerSet::new(B::VARIANT);
        let cmb = &mut cmb_guard[encoder_id];
        let device = &device_guard[cmb.device_id.value];
        let mut raw = device.com_allocator.extend(cmb);

        unsafe {
            raw.begin_primary(hal::command::CommandBufferFlags::ONE_TIME_SUBMIT);
        }

        let (bundle_guard, mut token) = hub.render_bundles.read(&mut token);
        let (pipeline_layout_guard, mut token) = hub.pipeline_layouts.read(&mut token);
        let (bind_group_guard, mut token) = hub.bind_groups.read(&mut token);
        let (pipeline_guard, mut token) = hub.render_pipelines.read(&mut token);
        let (buffer_guard, mut token) = hub.buffers.read(&mut token);
        let (texture_guard, mut token) = hub.textures.read(&mut token);
        let (view_guard, _) = hub.texture_views.read(&mut token);

        let mut peeker = raw_data.as_ptr();
        let raw_data_end = unsafe { raw_data.as_ptr().add(raw_data.len()) };

        let mut targets: RawRenderTargets = unsafe { mem::zeroed() };
        assert!(
            unsafe { peeker.add(RawRenderTargets::max_size()) <= raw_data_end },
            "RawRenderTargets (size {}) is too big to fit within raw_data (size {})",
            RawRenderTargets::max_size(),
            raw_data.len()
        );
        peeker = unsafe { RawRenderTargets::peek_from(peeker, &mut targets) };
        #[cfg(feature = "trace")]
        let command_peeker_base = peeker;

        let color_attachments = targets
            .colors
            .iter()
            .take_while(|at| at.attachment != 0)
            .map(|at| RenderPassColorAttachmentDescriptor {
                attachment: id::TextureViewId::from_raw(at.attachment).unwrap(),
                resolve_target: id::TextureViewId::from_raw(at.resolve_target),
                load_op: at.component.load_op,
                store_op: at.component.store_op,
                clear_color: at.component.clear_value,
            })
            .collect::<ArrayVec<[_; MAX_COLOR_TARGETS]>>();
        let depth_stencil_attachment_body;
        let depth_stencil_attachment = if targets.depth_stencil.attachment == 0 {
            None
        } else {
            let at = &targets.depth_stencil;
            depth_stencil_attachment_body = RenderPassDepthStencilAttachmentDescriptor {
                attachment: id::TextureViewId::from_raw(at.attachment).unwrap(),
                depth_load_op: at.depth.load_op,
                depth_store_op: at.depth.store_op,
                depth_read_only: at.depth.read_only,
                clear_depth: at.depth.clear_value,
                stencil_load_op: at.stencil.load_op,
                stencil_store_op: at.stencil.store_op,
                clear_stencil: at.stencil.clear_value,
                stencil_read_only: at.stencil.read_only,
            };
            Some(&depth_stencil_attachment_body)
        };
        // We default to false intentionally, even if depth-stencil isn't used at all.
        // This allows us to use the primary raw pipeline in `RenderPipeline`,
        // instead of the special read-only one, which would be `None`.
        let mut is_ds_read_only = false;

        struct OutputAttachment<'a> {
            texture_id: &'a Stored<id::TextureId>,
            range: &'a hal::image::SubresourceRange,
            previous_use: Option<TextureUse>,
            new_use: TextureUse,
        }
        const MAX_TOTAL_ATTACHMENTS: usize = 2 * MAX_COLOR_TARGETS + 1;
        let mut output_attachments = ArrayVec::<[OutputAttachment; MAX_TOTAL_ATTACHMENTS]>::new();

        let context = {
            use hal::device::Device as _;

            let samples_count_limit = device.hal_limits.framebuffer_color_sample_counts;
            let base_trackers = &cmb.trackers;

            let mut extent = None;
            let mut used_swap_chain = None::<Stored<id::SwapChainId>>;

            let sample_count = color_attachments
                .get(0)
                .map(|at| view_guard[at.attachment].samples)
                .unwrap_or(1);
            assert!(
                sample_count & samples_count_limit != 0,
                "Attachment sample_count must be supported by physical device limits"
            );

            log::trace!(
                "Encoding render pass begin in command buffer {:?}",
                encoder_id
            );
            let rp_key = {
                let depth_stencil = match depth_stencil_attachment {
                    Some(at) => {
                        let view = trackers
                            .views
                            .use_extend(&*view_guard, at.attachment, (), ())
                            .unwrap();
                        if let Some(ex) = extent {
                            assert_eq!(ex, view.extent, "Extent state must match extent from view");
                        } else {
                            extent = Some(view.extent);
                        }
                        let source_id = match view.inner {
                            TextureViewInner::Native { ref source_id, .. } => source_id,
                            TextureViewInner::SwapChain { .. } => {
                                panic!("Unexpected depth/stencil use of swapchain image!")
                            }
                        };

                        // Using render pass for transition.
                        let previous_use = base_trackers
                            .textures
                            .query(source_id.value, view.range.clone());
                        let new_use = if is_depth_stencil_read_only(at, view.range.aspects) {
                            is_ds_read_only = true;
                            TextureUse::ATTACHMENT_READ
                        } else {
                            TextureUse::ATTACHMENT_WRITE
                        };
                        output_attachments.push(OutputAttachment {
                            texture_id: source_id,
                            range: &view.range,
                            previous_use,
                            new_use,
                        });

                        let new_layout = conv::map_texture_state(new_use, view.range.aspects).1;
                        let old_layout = match previous_use {
                            Some(usage) => conv::map_texture_state(usage, view.range.aspects).1,
                            None => new_layout,
                        };

                        let ds_at = hal::pass::Attachment {
                            format: Some(conv::map_texture_format(
                                view.format,
                                device.private_features,
                            )),
                            samples: view.samples,
                            ops: conv::map_load_store_ops(at.depth_load_op, at.depth_store_op),
                            stencil_ops: conv::map_load_store_ops(
                                at.stencil_load_op,
                                at.stencil_store_op,
                            ),
                            layouts: old_layout..new_layout,
                        };
                        Some((ds_at, new_layout))
                    }
                    None => None,
                };

                let mut colors = ArrayVec::new();
                let mut resolves = ArrayVec::new();

                for at in &color_attachments {
                    let view = trackers
                        .views
                        .use_extend(&*view_guard, at.attachment, (), ())
                        .unwrap();
                    if let Some(ex) = extent {
                        assert_eq!(ex, view.extent, "Extent state must match extent from view");
                    } else {
                        extent = Some(view.extent);
                    }
                    assert_eq!(
                        view.samples, sample_count,
                        "All attachments must have the same sample_count"
                    );

                    let layouts = match view.inner {
                        TextureViewInner::Native { ref source_id, .. } => {
                            let previous_use = base_trackers
                                .textures
                                .query(source_id.value, view.range.clone());
                            let new_use = TextureUse::ATTACHMENT_WRITE;
                            output_attachments.push(OutputAttachment {
                                texture_id: source_id,
                                range: &view.range,
                                previous_use,
                                new_use,
                            });

                            let new_layout =
                                conv::map_texture_state(new_use, hal::format::Aspects::COLOR).1;
                            let old_layout = match previous_use {
                                Some(usage) => {
                                    conv::map_texture_state(usage, hal::format::Aspects::COLOR).1
                                }
                                None => new_layout,
                            };
                            old_layout..new_layout
                        }
                        TextureViewInner::SwapChain { ref source_id, .. } => {
                            if let Some((ref sc_id, _)) = cmb.used_swap_chain {
                                assert_eq!(
                                    source_id.value, sc_id.value,
                                    "Texture view's swap chain must match swap chain in use"
                                );
                            } else {
                                assert!(used_swap_chain.is_none());
                                used_swap_chain = Some(source_id.clone());
                            }

                            let end = hal::image::Layout::Present;
                            let start = match at.load_op {
                                LoadOp::Clear => hal::image::Layout::Undefined,
                                LoadOp::Load => end,
                            };
                            start..end
                        }
                    };

                    let color_at = hal::pass::Attachment {
                        format: Some(conv::map_texture_format(
                            view.format,
                            device.private_features,
                        )),
                        samples: view.samples,
                        ops: conv::map_load_store_ops(at.load_op, at.store_op),
                        stencil_ops: hal::pass::AttachmentOps::DONT_CARE,
                        layouts,
                    };
                    colors.push((color_at, hal::image::Layout::ColorAttachmentOptimal));
                }

                for resolve_target in color_attachments.iter().flat_map(|at| at.resolve_target) {
                    let view = trackers
                        .views
                        .use_extend(&*view_guard, resolve_target, (), ())
                        .unwrap();
                    assert_eq!(
                        extent,
                        Some(view.extent),
                        "Extent state must match extent from view"
                    );
                    assert_eq!(
                        view.samples, 1,
                        "All resolve_targets must have a sample_count of 1"
                    );

                    let layouts = match view.inner {
                        TextureViewInner::Native { ref source_id, .. } => {
                            let previous_use = base_trackers
                                .textures
                                .query(source_id.value, view.range.clone());
                            let new_use = TextureUse::ATTACHMENT_WRITE;
                            output_attachments.push(OutputAttachment {
                                texture_id: source_id,
                                range: &view.range,
                                previous_use,
                                new_use,
                            });

                            let new_layout =
                                conv::map_texture_state(new_use, hal::format::Aspects::COLOR).1;
                            let old_layout = match previous_use {
                                Some(usage) => {
                                    conv::map_texture_state(usage, hal::format::Aspects::COLOR).1
                                }
                                None => new_layout,
                            };
                            old_layout..new_layout
                        }
                        TextureViewInner::SwapChain { ref source_id, .. } => {
                            if let Some((ref sc_id, _)) = cmb.used_swap_chain {
                                assert_eq!(
                                    source_id.value, sc_id.value,
                                    "Texture view's swap chain must match swap chain in use"
                                );
                            } else {
                                assert!(used_swap_chain.is_none());
                                used_swap_chain = Some(source_id.clone());
                            }
                            hal::image::Layout::Undefined..hal::image::Layout::Present
                        }
                    };

                    let resolve_at = hal::pass::Attachment {
                        format: Some(conv::map_texture_format(
                            view.format,
                            device.private_features,
                        )),
                        samples: view.samples,
                        ops: hal::pass::AttachmentOps::new(
                            hal::pass::AttachmentLoadOp::DontCare,
                            hal::pass::AttachmentStoreOp::Store,
                        ),
                        stencil_ops: hal::pass::AttachmentOps::DONT_CARE,
                        layouts,
                    };
                    resolves.push((resolve_at, hal::image::Layout::ColorAttachmentOptimal));
                }

                RenderPassKey {
                    colors,
                    resolves,
                    depth_stencil,
                }
            };

            let mut render_pass_cache = device.render_passes.lock();
            let render_pass = match render_pass_cache.entry(rp_key.clone()) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(entry) => {
                    let color_ids: [hal::pass::AttachmentRef; MAX_COLOR_TARGETS] = [
                        (0, hal::image::Layout::ColorAttachmentOptimal),
                        (1, hal::image::Layout::ColorAttachmentOptimal),
                        (2, hal::image::Layout::ColorAttachmentOptimal),
                        (3, hal::image::Layout::ColorAttachmentOptimal),
                    ];

                    let mut resolve_ids = ArrayVec::<[_; MAX_COLOR_TARGETS]>::new();
                    let mut attachment_index = color_attachments.len();
                    if color_attachments
                        .iter()
                        .any(|at| at.resolve_target.is_some())
                    {
                        for ((i, at), &(_, layout)) in color_attachments
                            .iter()
                            .enumerate()
                            .zip(entry.key().resolves.iter())
                        {
                            let real_attachment_index = match at.resolve_target {
                                Some(resolve_attachment) => {
                                    assert_ne!(
                                        view_guard[at.attachment].samples,
                                        1,
                                        "RenderPassColorAttachmentDescriptor's attachment with a resolve_target must be multi-sampled",
                                    );
                                    assert_eq!(
                                        view_guard[resolve_attachment].samples,
                                        1,
                                        "RenderPassColorAttachmentDescriptor's resolve_target must not be multi-sampled",
                                    );
                                    attachment_index + i
                                }
                                None => hal::pass::ATTACHMENT_UNUSED,
                            };
                            resolve_ids.push((real_attachment_index, layout));
                        }
                        attachment_index += color_attachments.len();
                    }

                    let depth_id = depth_stencil_attachment.map(|at| {
                        let aspects = view_guard[at.attachment].range.aspects;
                        let usage = if is_ds_read_only {
                            TextureUse::ATTACHMENT_READ
                        } else {
                            TextureUse::ATTACHMENT_WRITE
                        };
                        (attachment_index, conv::map_texture_state(usage, aspects).1)
                    });

                    let subpass = hal::pass::SubpassDesc {
                        colors: &color_ids[..color_attachments.len()],
                        resolves: &resolve_ids,
                        depth_stencil: depth_id.as_ref(),
                        inputs: &[],
                        preserves: &[],
                    };
                    let all = entry.key().all().map(|(at, _)| at);

                    let pass =
                        unsafe { device.raw.create_render_pass(all, iter::once(subpass), &[]) }
                            .unwrap();
                    entry.insert(pass)
                }
            };

            let mut framebuffer_cache;
            let fb_key = FramebufferKey {
                colors: color_attachments.iter().map(|at| at.attachment).collect(),
                resolves: color_attachments
                    .iter()
                    .filter_map(|at| at.resolve_target)
                    .collect(),
                depth_stencil: depth_stencil_attachment.map(|at| at.attachment),
            };

            let framebuffer = match used_swap_chain.take() {
                Some(sc_id) => {
                    assert!(cmb.used_swap_chain.is_none());
                    // Always create a new framebuffer and delete it after presentation.
                    let attachments = fb_key.all().map(|&id| match view_guard[id].inner {
                        TextureViewInner::Native { ref raw, .. } => raw,
                        TextureViewInner::SwapChain { ref image, .. } => Borrow::borrow(image),
                    });
                    let framebuffer = unsafe {
                        device
                            .raw
                            .create_framebuffer(&render_pass, attachments, extent.unwrap())
                            .unwrap()
                    };
                    cmb.used_swap_chain = Some((sc_id, framebuffer));
                    &mut cmb.used_swap_chain.as_mut().unwrap().1
                }
                None => {
                    // Cache framebuffers by the device.
                    framebuffer_cache = device.framebuffers.lock();
                    match framebuffer_cache.entry(fb_key) {
                        Entry::Occupied(e) => e.into_mut(),
                        Entry::Vacant(e) => {
                            let fb = {
                                let attachments =
                                    e.key().all().map(|&id| match view_guard[id].inner {
                                        TextureViewInner::Native { ref raw, .. } => raw,
                                        TextureViewInner::SwapChain { ref image, .. } => {
                                            Borrow::borrow(image)
                                        }
                                    });
                                unsafe {
                                    device.raw.create_framebuffer(
                                        &render_pass,
                                        attachments,
                                        extent.unwrap(),
                                    )
                                }
                                .unwrap()
                            };
                            e.insert(fb)
                        }
                    }
                }
            };

            let rect = {
                let ex = extent.unwrap();
                hal::pso::Rect {
                    x: 0,
                    y: 0,
                    w: ex.width as _,
                    h: ex.height as _,
                }
            };

            let clear_values = color_attachments
                .iter()
                .zip(&rp_key.colors)
                .flat_map(|(at, (rat, _layout))| {
                    match at.load_op {
                        LoadOp::Load => None,
                        LoadOp::Clear => {
                            use hal::format::ChannelType;
                            //TODO: validate sign/unsign and normalized ranges of the color values
                            let value = match rat.format.unwrap().base_format().1 {
                                ChannelType::Unorm
                                | ChannelType::Snorm
                                | ChannelType::Ufloat
                                | ChannelType::Sfloat
                                | ChannelType::Uscaled
                                | ChannelType::Sscaled
                                | ChannelType::Srgb => hal::command::ClearColor {
                                    float32: conv::map_color_f32(&at.clear_color),
                                },
                                ChannelType::Sint => hal::command::ClearColor {
                                    sint32: conv::map_color_i32(&at.clear_color),
                                },
                                ChannelType::Uint => hal::command::ClearColor {
                                    uint32: conv::map_color_u32(&at.clear_color),
                                },
                            };
                            Some(hal::command::ClearValue { color: value })
                        }
                    }
                })
                .chain(depth_stencil_attachment.and_then(|at| {
                    match (at.depth_load_op, at.stencil_load_op) {
                        (LoadOp::Load, LoadOp::Load) => None,
                        (LoadOp::Clear, _) | (_, LoadOp::Clear) => {
                            let value = hal::command::ClearDepthStencil {
                                depth: at.clear_depth,
                                stencil: at.clear_stencil,
                            };
                            Some(hal::command::ClearValue {
                                depth_stencil: value,
                            })
                        }
                    }
                }));

            unsafe {
                raw.begin_render_pass(
                    render_pass,
                    framebuffer,
                    rect,
                    clear_values,
                    hal::command::SubpassContents::Inline,
                );
                raw.set_scissors(0, iter::once(&rect));
                raw.set_viewports(
                    0,
                    iter::once(hal::pso::Viewport {
                        rect,
                        depth: 0.0..1.0,
                    }),
                );
            }

            RenderPassContext {
                attachments: AttachmentData {
                    colors: color_attachments
                        .iter()
                        .map(|at| view_guard[at.attachment].format)
                        .collect(),
                    resolves: color_attachments
                        .iter()
                        .filter_map(|at| at.resolve_target)
                        .map(|resolve| view_guard[resolve].format)
                        .collect(),
                    depth_stencil: depth_stencil_attachment
                        .map(|at| view_guard[at.attachment].format),
                },
                sample_count,
            }
        };

        let mut state = State {
            binder: Binder::new(cmb.limits.max_bind_groups),
            blend_color: OptionalState::Unused,
            stencil_reference: OptionalState::Unused,
            pipeline: OptionalState::Required,
            index: IndexState::default(),
            vertex: VertexState::default(),
            debug_scope_depth: 0,
        };

        let mut command = RenderCommand::Draw {
            vertex_count: 0,
            instance_count: 0,
            first_vertex: 0,
            first_instance: 0,
        };
        loop {
            assert!(
                unsafe { peeker.add(RenderCommand::max_size()) <= raw_data_end },
                "RenderCommand (size {}) is too big to fit within raw_data (size {})",
                RenderCommand::max_size(),
                raw_data.len()
            );
            peeker = unsafe { RenderCommand::peek_from(peeker, &mut command) };
            match command {
                RenderCommand::SetBindGroup {
                    index,
                    num_dynamic_offsets,
                    bind_group_id,
                    phantom_offsets,
                } => {
                    let (new_peeker, offsets) = unsafe {
                        phantom_offsets.decode_unaligned(
                            peeker,
                            num_dynamic_offsets as usize,
                            raw_data_end,
                        )
                    };
                    peeker = new_peeker;

                    if cfg!(debug_assertions) {
                        for off in offsets {
                            assert_eq!(
                                *off as BufferAddress % BIND_BUFFER_ALIGNMENT,
                                0,
                                "Misaligned dynamic buffer offset: {} does not align with {}",
                                off,
                                BIND_BUFFER_ALIGNMENT
                            );
                        }
                    }

                    let bind_group = trackers
                        .bind_groups
                        .use_extend(&*bind_group_guard, bind_group_id, (), ())
                        .unwrap();
                    assert_eq!(bind_group.dynamic_count, offsets.len());

                    trackers.merge_extend(&bind_group.used);

                    if let Some((pipeline_layout_id, follow_ups)) = state.binder.provide_entry(
                        index as usize,
                        bind_group_id,
                        bind_group,
                        offsets,
                    ) {
                        let bind_groups = iter::once(bind_group.raw.raw()).chain(
                            follow_ups
                                .clone()
                                .map(|(bg_id, _)| bind_group_guard[bg_id].raw.raw()),
                        );
                        unsafe {
                            raw.bind_graphics_descriptor_sets(
                                &pipeline_layout_guard[pipeline_layout_id].raw,
                                index as usize,
                                bind_groups,
                                offsets
                                    .iter()
                                    .chain(follow_ups.flat_map(|(_, offsets)| offsets))
                                    .cloned(),
                            );
                        }
                    };
                }
                RenderCommand::SetPipeline(pipeline_id) => {
                    state.pipeline = OptionalState::Set;
                    let pipeline = trackers
                        .render_pipes
                        .use_extend(&*pipeline_guard, pipeline_id, (), ())
                        .unwrap();

                    assert!(
                        context.compatible(&pipeline.pass_context),
                        "The render pipeline output formats and sample count do not match render pass attachment formats!"
                    );
                    assert!(
                        !is_ds_read_only || pipeline.flags.contains(PipelineFlags::DEPTH_STENCIL_READ_ONLY),
                        "Pipeline {:?} is not compatible with the depth-stencil read-only render pass",
                        pipeline_id
                    );

                    state
                        .blend_color
                        .require(pipeline.flags.contains(PipelineFlags::BLEND_COLOR));
                    state
                        .stencil_reference
                        .require(pipeline.flags.contains(PipelineFlags::STENCIL_REFERENCE));

                    unsafe {
                        raw.bind_graphics_pipeline(&pipeline.raw);
                    }

                    // Rebind resource
                    if state.binder.pipeline_layout_id != Some(pipeline.layout_id.value) {
                        let pipeline_layout = &pipeline_layout_guard[pipeline.layout_id.value];
                        state.binder.pipeline_layout_id = Some(pipeline.layout_id.value);
                        state
                            .binder
                            .reset_expectations(pipeline_layout.bind_group_layout_ids.len());
                        let mut is_compatible = true;

                        for (index, (entry, bgl_id)) in state
                            .binder
                            .entries
                            .iter_mut()
                            .zip(&pipeline_layout.bind_group_layout_ids)
                            .enumerate()
                        {
                            match entry.expect_layout(bgl_id.value) {
                                LayoutChange::Match(bg_id, offsets) if is_compatible => {
                                    let desc_set = bind_group_guard[bg_id].raw.raw();
                                    unsafe {
                                        raw.bind_graphics_descriptor_sets(
                                            &pipeline_layout.raw,
                                            index,
                                            iter::once(desc_set),
                                            offsets.iter().cloned(),
                                        );
                                    }
                                }
                                LayoutChange::Match(..) | LayoutChange::Unchanged => {}
                                LayoutChange::Mismatch => {
                                    is_compatible = false;
                                }
                            }
                        }
                    }

                    // Rebind index buffer if the index format has changed with the pipeline switch
                    if state.index.format != pipeline.index_format {
                        state.index.format = pipeline.index_format;
                        state.index.update_limit();

                        if let Some((buffer_id, ref range)) = state.index.bound_buffer_view {
                            let buffer = trackers
                                .buffers
                                .use_extend(&*buffer_guard, buffer_id, (), BufferUse::INDEX)
                                .unwrap();

                            let view = hal::buffer::IndexBufferView {
                                buffer: &buffer.raw,
                                range: hal::buffer::SubRange {
                                    offset: range.start,
                                    size: Some(range.end - range.start),
                                },
                                index_type: conv::map_index_format(state.index.format),
                            };

                            unsafe {
                                raw.bind_index_buffer(view);
                            }
                        }
                    }
                    // Update vertex buffer limits
                    for (vbs, &(stride, rate)) in
                        state.vertex.inputs.iter_mut().zip(&pipeline.vertex_strides)
                    {
                        vbs.stride = stride;
                        vbs.rate = rate;
                    }
                    let vertex_strides_len = pipeline.vertex_strides.len();
                    for vbs in state.vertex.inputs.iter_mut().skip(vertex_strides_len) {
                        vbs.stride = 0;
                        vbs.rate = InputStepMode::Vertex;
                    }
                    state.vertex.update_limits();
                }
                RenderCommand::SetIndexBuffer {
                    buffer_id,
                    offset,
                    size,
                } => {
                    let buffer = trackers
                        .buffers
                        .use_extend(&*buffer_guard, buffer_id, (), BufferUse::INDEX)
                        .unwrap();
                    assert!(buffer.usage.contains(BufferUsage::INDEX), "An invalid setIndexBuffer call has been made. The buffer usage is {:?} which does not contain required usage INDEX", buffer.usage);

                    let end = if size != BufferSize::WHOLE {
                        offset + size.0
                    } else {
                        buffer.size
                    };
                    state.index.bound_buffer_view = Some((buffer_id, offset..end));
                    state.index.update_limit();

                    let view = hal::buffer::IndexBufferView {
                        buffer: &buffer.raw,
                        range: hal::buffer::SubRange {
                            offset,
                            size: Some(end - offset),
                        },
                        index_type: conv::map_index_format(state.index.format),
                    };

                    unsafe {
                        raw.bind_index_buffer(view);
                    }
                }
                RenderCommand::SetVertexBuffer {
                    slot,
                    buffer_id,
                    offset,
                    size,
                } => {
                    let buffer = trackers
                        .buffers
                        .use_extend(&*buffer_guard, buffer_id, (), BufferUse::VERTEX)
                        .unwrap();
                    assert!(buffer.usage.contains(BufferUsage::VERTEX), "An invalid setVertexBuffer call has been made. The buffer usage is {:?} which does not contain required usage VERTEX", buffer.usage);
                    let empty_slots = (1 + slot as usize).saturating_sub(state.vertex.inputs.len());
                    state
                        .vertex
                        .inputs
                        .extend(iter::repeat(VertexBufferState::EMPTY).take(empty_slots));
                    state.vertex.inputs[slot as usize].total_size = if size != BufferSize::WHOLE {
                        size.0
                    } else {
                        buffer.size - offset
                    };

                    let range = hal::buffer::SubRange {
                        offset,
                        size: if size != BufferSize::WHOLE {
                            Some(size.0)
                        } else {
                            None
                        },
                    };
                    unsafe {
                        raw.bind_vertex_buffers(slot, iter::once((&buffer.raw, range)));
                    }
                    state.vertex.update_limits();
                }
                RenderCommand::SetBlendColor(ref color) => {
                    state.blend_color = OptionalState::Set;
                    unsafe {
                        raw.set_blend_constants(conv::map_color_f32(color));
                    }
                }
                RenderCommand::SetStencilReference(value) => {
                    state.stencil_reference = OptionalState::Set;
                    unsafe {
                        raw.set_stencil_reference(hal::pso::Face::all(), value);
                    }
                }
                RenderCommand::SetViewport {
                    ref rect,
                    depth_min,
                    depth_max,
                } => {
                    use std::{convert::TryFrom, i16};
                    let r = hal::pso::Rect {
                        x: i16::try_from(rect.x.round() as i64).unwrap_or(0),
                        y: i16::try_from(rect.y.round() as i64).unwrap_or(0),
                        w: i16::try_from(rect.w.round() as i64).unwrap_or(i16::MAX),
                        h: i16::try_from(rect.h.round() as i64).unwrap_or(i16::MAX),
                    };
                    unsafe {
                        raw.set_viewports(
                            0,
                            iter::once(hal::pso::Viewport {
                                rect: r,
                                depth: depth_min..depth_max,
                            }),
                        );
                    }
                }
                RenderCommand::SetScissor(ref rect) => {
                    use std::{convert::TryFrom, i16};
                    let r = hal::pso::Rect {
                        x: i16::try_from(rect.x).unwrap_or(0),
                        y: i16::try_from(rect.y).unwrap_or(0),
                        w: i16::try_from(rect.w).unwrap_or(i16::MAX),
                        h: i16::try_from(rect.h).unwrap_or(i16::MAX),
                    };
                    unsafe {
                        raw.set_scissors(0, iter::once(r));
                    }
                }
                RenderCommand::Draw {
                    vertex_count,
                    instance_count,
                    first_vertex,
                    first_instance,
                } => {
                    state.is_ready().unwrap();
                    assert!(
                        first_vertex + vertex_count <= state.vertex.vertex_limit,
                        "Vertex {} extends beyond limit {}",
                        first_vertex + vertex_count,
                        state.vertex.vertex_limit
                    );
                    assert!(
                        first_instance + instance_count <= state.vertex.instance_limit,
                        "Instance {} extends beyond limit {}",
                        first_instance + instance_count,
                        state.vertex.instance_limit
                    );

                    unsafe {
                        raw.draw(
                            first_vertex..first_vertex + vertex_count,
                            first_instance..first_instance + instance_count,
                        );
                    }
                }
                RenderCommand::DrawIndexed {
                    index_count,
                    instance_count,
                    first_index,
                    base_vertex,
                    first_instance,
                } => {
                    state.is_ready().unwrap();

                    //TODO: validate that base_vertex + max_index() is within the provided range
                    assert!(
                        first_index + index_count <= state.index.limit,
                        "Index {} extends beyond limit {}",
                        first_index + index_count,
                        state.index.limit
                    );
                    assert!(
                        first_instance + instance_count <= state.vertex.instance_limit,
                        "Instance {} extends beyond limit {}",
                        first_instance + instance_count,
                        state.vertex.instance_limit
                    );

                    unsafe {
                        raw.draw_indexed(
                            first_index..first_index + index_count,
                            base_vertex,
                            first_instance..first_instance + instance_count,
                        );
                    }
                }
                RenderCommand::DrawIndirect { buffer_id, offset } => {
                    state.is_ready().unwrap();

                    let buffer = trackers
                        .buffers
                        .use_extend(&*buffer_guard, buffer_id, (), BufferUse::INDIRECT)
                        .unwrap();
                    assert!(buffer.usage.contains(BufferUsage::INDIRECT), "An invalid drawIndirect call has been made. The buffer usage is {:?} which does not contain required usage INDIRECT", buffer.usage);

                    unsafe {
                        raw.draw_indirect(&buffer.raw, offset, 1, 0);
                    }
                }
                RenderCommand::DrawIndexedIndirect { buffer_id, offset } => {
                    state.is_ready().unwrap();

                    let buffer = trackers
                        .buffers
                        .use_extend(&*buffer_guard, buffer_id, (), BufferUse::INDIRECT)
                        .unwrap();
                    assert!(buffer.usage.contains(BufferUsage::INDIRECT), "An invalid drawIndexedIndirect call has been made. The buffer usage is {:?} which does not contain required usage INDIRECT", buffer.usage);

                    unsafe {
                        raw.draw_indexed_indirect(&buffer.raw, offset, 1, 0);
                    }
                }
                RenderCommand::PushDebugGroup {
                    color,
                    len,
                    phantom_marker,
                } => unsafe {
                    state.debug_scope_depth += 1;

                    let (new_peeker, label) =
                        { phantom_marker.decode_unaligned(peeker, len, raw_data_end) };
                    peeker = new_peeker;

                    raw.begin_debug_marker(str::from_utf8(label).unwrap(), color)
                },
                RenderCommand::PopDebugGroup => unsafe {
                    assert_ne!(
                        state.debug_scope_depth, 0,
                        "Can't pop debug group, because number of pushed debug groups is zero!"
                    );
                    state.debug_scope_depth -= 1;

                    raw.end_debug_marker()
                },
                RenderCommand::InsertDebugMarker {
                    color,
                    len,
                    phantom_marker,
                } => unsafe {
                    let (new_peeker, label) =
                        { phantom_marker.decode_unaligned(peeker, len, raw_data_end) };
                    peeker = new_peeker;

                    raw.insert_debug_marker(str::from_utf8(label).unwrap(), color)
                },
                RenderCommand::ExecuteBundle(bundle_id) => {
                    let bundle = trackers
                        .bundles
                        .use_extend(&*bundle_guard, bundle_id, (), ())
                        .unwrap();

                    assert!(
                        context.compatible(&bundle.context),
                        "The render bundle output formats do not match render pass attachment formats!"
                    );

                    unsafe {
                        bundle.execute(
                            &mut raw,
                            &*pipeline_layout_guard,
                            &*bind_group_guard,
                            &*pipeline_guard,
                            &*buffer_guard,
                        )
                    };

                    trackers.merge_extend(&bundle.used);
                    state.reset_bundle();
                }
                RenderCommand::End => break,
            }
        }

        #[cfg(feature = "trace")]
        match cmb.commands {
            Some(ref mut list) => {
                let mut pass_commands = Vec::new();
                let mut pass_dynamic_offsets = Vec::new();
                peeker = command_peeker_base;
                loop {
                    peeker = unsafe { RenderCommand::peek_from(peeker, &mut command) };
                    match command {
                        RenderCommand::SetBindGroup {
                            num_dynamic_offsets,
                            phantom_offsets,
                            ..
                        } => {
                            let (new_peeker, offsets) = unsafe {
                                phantom_offsets.decode_unaligned(
                                    peeker,
                                    num_dynamic_offsets as usize,
                                    raw_data_end,
                                )
                            };
                            peeker = new_peeker;
                            pass_dynamic_offsets.extend_from_slice(offsets);
                        }
                        RenderCommand::End => break,
                        _ => {}
                    }
                    pass_commands.push(command);
                }
                list.push(crate::device::trace::Command::RunRenderPass {
                    target_colors: color_attachments.into_iter().collect(),
                    target_depth_stencil: depth_stencil_attachment.cloned(),
                    commands: pass_commands,
                    dynamic_offsets: pass_dynamic_offsets,
                });
            }
            None => {}
        }

        log::trace!("Merging {:?} with the render pass", encoder_id);
        unsafe {
            raw.end_render_pass();
        }

        for ot in output_attachments {
            let texture = &texture_guard[ot.texture_id.value];
            assert!(
                texture.usage.contains(TextureUsage::OUTPUT_ATTACHMENT),
                "Texture usage {:?} must contain the usage flag OUTPUT_ATTACHMENT",
                texture.usage
            );

            // the tracker set of the pass is always in "extend" mode
            trackers
                .textures
                .change_extend(
                    ot.texture_id.value,
                    &ot.texture_id.ref_count,
                    ot.range.clone(),
                    ot.new_use,
                )
                .unwrap();

            if let Some(usage) = ot.previous_use {
                // Make the attachment tracks to be aware of the internal
                // transition done by the render pass, by registering the
                // previous usage as the initial state.
                trackers
                    .textures
                    .prepend(
                        ot.texture_id.value,
                        &ot.texture_id.ref_count,
                        ot.range.clone(),
                        usage,
                    )
                    .unwrap();
            }
        }

        super::CommandBuffer::insert_barriers(
            cmb.raw.last_mut().unwrap(),
            &mut cmb.trackers,
            &trackers,
            &*buffer_guard,
            &*texture_guard,
        );
        unsafe {
            cmb.raw.last_mut().unwrap().finish();
        }
        cmb.raw.push(raw);
    }
}

pub mod render_ffi {
    use super::{
        super::{PhantomSlice, Rect},
        RenderCommand,
    };
    use crate::{id, RawString};
    use std::{convert::TryInto, ffi, slice};
    use wgt::{BufferAddress, BufferSize, Color, DynamicOffset};

    type RawPass = super::super::RawPass<id::CommandEncoderId>;

    /// # Safety
    ///
    /// This function is unsafe as there is no guarantee that the given pointer is
    /// valid for `offset_length` elements.
    // TODO: There might be other safety issues, such as using the unsafe
    // `RawPass::encode` and `RawPass::encode_slice`.
    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_set_bind_group(
        pass: &mut RawPass,
        index: u32,
        bind_group_id: id::BindGroupId,
        offsets: *const DynamicOffset,
        offset_length: usize,
    ) {
        pass.encode(&RenderCommand::SetBindGroup {
            index: index.try_into().unwrap(),
            num_dynamic_offsets: offset_length.try_into().unwrap(),
            bind_group_id,
            phantom_offsets: PhantomSlice::default(),
        });
        pass.encode_slice(slice::from_raw_parts(offsets, offset_length));
    }

    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_set_pipeline(
        pass: &mut RawPass,
        pipeline_id: id::RenderPipelineId,
    ) {
        pass.encode(&RenderCommand::SetPipeline(pipeline_id));
    }

    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_set_index_buffer(
        pass: &mut RawPass,
        buffer_id: id::BufferId,
        offset: BufferAddress,
        size: BufferSize,
    ) {
        pass.encode(&RenderCommand::SetIndexBuffer {
            buffer_id,
            offset,
            size,
        });
    }

    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_set_vertex_buffer(
        pass: &mut RawPass,
        slot: u32,
        buffer_id: id::BufferId,
        offset: BufferAddress,
        size: BufferSize,
    ) {
        pass.encode(&RenderCommand::SetVertexBuffer {
            slot,
            buffer_id,
            offset,
            size,
        });
    }

    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_set_blend_color(pass: &mut RawPass, color: &Color) {
        pass.encode(&RenderCommand::SetBlendColor(*color));
    }

    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_set_stencil_reference(
        pass: &mut RawPass,
        value: u32,
    ) {
        pass.encode(&RenderCommand::SetStencilReference(value));
    }

    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_set_viewport(
        pass: &mut RawPass,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        depth_min: f32,
        depth_max: f32,
    ) {
        pass.encode(&RenderCommand::SetViewport {
            rect: Rect { x, y, w, h },
            depth_min,
            depth_max,
        });
    }

    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_set_scissor_rect(
        pass: &mut RawPass,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) {
        pass.encode(&RenderCommand::SetScissor(Rect { x, y, w, h }));
    }

    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_draw(
        pass: &mut RawPass,
        vertex_count: u32,
        instance_count: u32,
        first_vertex: u32,
        first_instance: u32,
    ) {
        pass.encode(&RenderCommand::Draw {
            vertex_count,
            instance_count,
            first_vertex,
            first_instance,
        });
    }

    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_draw_indexed(
        pass: &mut RawPass,
        index_count: u32,
        instance_count: u32,
        first_index: u32,
        base_vertex: i32,
        first_instance: u32,
    ) {
        pass.encode(&RenderCommand::DrawIndexed {
            index_count,
            instance_count,
            first_index,
            base_vertex,
            first_instance,
        });
    }

    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_draw_indirect(
        pass: &mut RawPass,
        buffer_id: id::BufferId,
        offset: BufferAddress,
    ) {
        pass.encode(&RenderCommand::DrawIndirect { buffer_id, offset });
    }

    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_draw_indexed_indirect(
        pass: &mut RawPass,
        buffer_id: id::BufferId,
        offset: BufferAddress,
    ) {
        pass.encode(&RenderCommand::DrawIndexedIndirect { buffer_id, offset });
    }

    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_push_debug_group(
        pass: &mut RawPass,
        label: RawString,
        color: u32,
    ) {
        let bytes = ffi::CStr::from_ptr(label).to_bytes();

        pass.encode(&RenderCommand::PushDebugGroup {
            color,
            len: bytes.len(),
            phantom_marker: PhantomSlice::default(),
        });
        pass.encode_slice(bytes);
    }

    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_pop_debug_group(pass: &mut RawPass) {
        pass.encode(&RenderCommand::PopDebugGroup);
    }

    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_insert_debug_marker(
        pass: &mut RawPass,
        label: RawString,
        color: u32,
    ) {
        let bytes = ffi::CStr::from_ptr(label).to_bytes();

        pass.encode(&RenderCommand::InsertDebugMarker {
            color,
            len: bytes.len(),
            phantom_marker: PhantomSlice::default(),
        });
        pass.encode_slice(bytes);
    }

    #[no_mangle]
    pub unsafe fn wgpu_render_pass_execute_bundles(
        pass: &mut RawPass,
        render_bundle_ids: *const id::RenderBundleId,
        render_bundle_ids_length: usize,
    ) {
        for &bundle_id in slice::from_raw_parts(render_bundle_ids, render_bundle_ids_length) {
            pass.encode(&RenderCommand::ExecuteBundle(bundle_id));
        }
    }

    #[no_mangle]
    pub unsafe extern "C" fn wgpu_render_pass_finish(
        pass: &mut RawPass,
        length: &mut usize,
    ) -> *const u8 {
        pass.finish(RenderCommand::End);
        *length = pass.size();
        pass.base
    }
}
