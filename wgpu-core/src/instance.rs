/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::{
    backend,
    device::Device,
    hub::{GfxBackend, Global, GlobalIdentityHandlerFactory, Input, Token},
    id::{AdapterId, DeviceId, SurfaceId},
    power, LifeGuard, PrivateFeatures, Stored, MAX_BIND_GROUPS,
};

use wgt::{Backend, BackendBit, DeviceDescriptor, PowerPreference, BIND_BUFFER_ALIGNMENT};

#[cfg(feature = "replay")]
use serde::Deserialize;
#[cfg(feature = "trace")]
use serde::Serialize;

use hal::{
    adapter::{AdapterInfo as HalAdapterInfo, DeviceType as HalDeviceType, PhysicalDevice as _},
    queue::QueueFamily as _,
    window::Surface as _,
    Instance as _,
};

#[repr(C)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "trace", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
pub struct RequestAdapterOptions {
    pub power_preference: PowerPreference,
    pub compatible_surface: Option<SurfaceId>,
}

impl Default for RequestAdapterOptions {
    fn default() -> Self {
        RequestAdapterOptions {
            power_preference: PowerPreference::Default,
            compatible_surface: None,
        }
    }
}

#[derive(Debug)]
pub struct Instance {
    #[cfg(any(
        not(any(target_os = "ios", target_os = "macos")),
        feature = "gfx-backend-vulkan"
    ))]
    pub vulkan: Option<gfx_backend_vulkan::Instance>,
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    pub metal: Option<gfx_backend_metal::Instance>,
    #[cfg(windows)]
    pub dx12: Option<gfx_backend_dx12::Instance>,
    #[cfg(windows)]
    pub dx11: Option<gfx_backend_dx11::Instance>,
}

impl Instance {
    pub fn new(name: &str, version: u32, backends: BackendBit) -> Self {
        Instance {
            #[cfg(any(
                not(any(target_os = "ios", target_os = "macos")),
                feature = "gfx-backend-vulkan"
            ))]
            vulkan: if backends.contains(Backend::Vulkan.into()) {
                gfx_backend_vulkan::Instance::create(name, version).ok()
            } else {
                None
            },
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            metal: if backends.contains(Backend::Metal.into()) {
                Some(gfx_backend_metal::Instance::create(name, version).unwrap())
            } else {
                None
            },
            #[cfg(windows)]
            dx12: if backends.contains(Backend::Dx12.into()) {
                gfx_backend_dx12::Instance::create(name, version).ok()
            } else {
                None
            },
            #[cfg(windows)]
            dx11: if backends.contains(Backend::Dx11.into()) {
                Some(gfx_backend_dx11::Instance::create(name, version).unwrap())
            } else {
                None
            },
        }
    }

    pub(crate) fn destroy_surface(&mut self, surface: Surface) {
        #[cfg(any(
            not(any(target_os = "ios", target_os = "macos")),
            feature = "gfx-backend-vulkan"
        ))]
        unsafe {
            if let Some(suf) = surface.vulkan {
                self.vulkan.as_mut().unwrap().destroy_surface(suf);
            }
        }
        #[cfg(any(target_os = "ios", target_os = "macos"))]
        unsafe {
            if let Some(suf) = surface.metal {
                self.metal.as_mut().unwrap().destroy_surface(suf);
            }
        }
        #[cfg(windows)]
        unsafe {
            if let Some(suf) = surface.dx12 {
                self.dx12.as_mut().unwrap().destroy_surface(suf);
            }
            if let Some(suf) = surface.dx11 {
                self.dx11.as_mut().unwrap().destroy_surface(suf);
            }
        }
    }
}

type GfxSurface<B> = <B as hal::Backend>::Surface;

#[derive(Debug)]
pub struct Surface {
    #[cfg(any(
        not(any(target_os = "ios", target_os = "macos")),
        feature = "gfx-backend-vulkan"
    ))]
    pub vulkan: Option<GfxSurface<backend::Vulkan>>,
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    pub metal: Option<GfxSurface<backend::Metal>>,
    #[cfg(windows)]
    pub dx12: Option<GfxSurface<backend::Dx12>>,
    #[cfg(windows)]
    pub dx11: Option<GfxSurface<backend::Dx11>>,
}

#[derive(Debug)]
pub struct Adapter<B: hal::Backend> {
    pub(crate) raw: hal::adapter::Adapter<B>,
    extensions: wgt::Extensions,
    limits: wgt::Limits,
    capabilities: wgt::Capabilities,
    unsafe_extensions: wgt::UnsafeExtensions,
    life_guard: LifeGuard,
}

impl<B: hal::Backend> Adapter<B> {
    fn new(raw: hal::adapter::Adapter<B>, unsafe_extensions: wgt::UnsafeExtensions) -> Self {
        let adapter_features = raw.physical_device.features();

        let mut extensions = wgt::Extensions::default() | wgt::Extensions::MAPPABLE_PRIMARY_BUFFERS;
        extensions.set(
            wgt::Extensions::ANISOTROPIC_FILTERING,
            adapter_features.contains(hal::Features::SAMPLER_ANISOTROPY),
        );
        extensions.set(
            wgt::Extensions::BINDING_INDEXING,
            adapter_features.intersects(
                hal::Features::SAMPLED_TEXTURE_DESCRIPTOR_INDEXING
                    | hal::Features::UNSIZED_DESCRIPTOR_ARRAY,
            ),
        );
        if unsafe_extensions.allowed() {
            // Unsafe extensions go here
        }

        let adapter_limits = raw.physical_device.limits();

        let limits = wgt::Limits {
            max_bind_groups: (adapter_limits.max_bound_descriptor_sets as u32)
                .min(MAX_BIND_GROUPS as u32),
            _non_exhaustive: unsafe { wgt::NonExhaustive::new() },
        };

        let mut capabilities = wgt::Capabilities::empty();

        capabilities.set(
            wgt::Capabilities::SAMPLED_TEXTURE_BINDING_ARRAY,
            adapter_features.contains(hal::Features::TEXTURE_DESCRIPTOR_ARRAY),
        );
        capabilities.set(
            wgt::Capabilities::SAMPLED_TEXTURE_ARRAY_DYNAMIC_INDEXING,
            adapter_features.contains(hal::Features::SHADER_SAMPLED_IMAGE_ARRAY_DYNAMIC_INDEXING),
        );
        capabilities.set(
            wgt::Capabilities::SAMPLED_TEXTURE_ARRAY_NON_UNIFORM_INDEXING,
            adapter_features.contains(hal::Features::SAMPLED_TEXTURE_DESCRIPTOR_INDEXING),
        );
        capabilities.set(
            wgt::Capabilities::UNSIZED_BINDING_ARRAY,
            adapter_features.contains(hal::Features::UNSIZED_DESCRIPTOR_ARRAY),
        );

        Adapter {
            raw,
            extensions,
            limits,
            capabilities,
            unsafe_extensions,
            life_guard: LifeGuard::new(),
        }
    }
}

/// Metadata about a backend adapter.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "trace", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
pub struct AdapterInfo {
    /// Adapter name
    pub name: String,
    /// Vendor PCI id of the adapter
    pub vendor: usize,
    /// PCI id of the adapter
    pub device: usize,
    /// Type of device
    pub device_type: DeviceType,
    /// Backend used for device
    pub backend: Backend,
}

impl AdapterInfo {
    fn from_gfx(adapter_info: HalAdapterInfo, backend: Backend) -> Self {
        let HalAdapterInfo {
            name,
            vendor,
            device,
            device_type,
        } = adapter_info;

        AdapterInfo {
            name,
            vendor,
            device,
            device_type: device_type.into(),
            backend,
        }
    }
}

/// Supported physical device types.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "trace", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
pub enum DeviceType {
    /// Other.
    Other,
    /// Integrated GPU with shared CPU/GPU memory.
    IntegratedGpu,
    /// Discrete GPU with separate CPU/GPU memory.
    DiscreteGpu,
    /// Virtual / Hosted.
    VirtualGpu,
    /// Cpu / Software Rendering.
    Cpu,
}

impl From<HalDeviceType> for DeviceType {
    fn from(device_type: HalDeviceType) -> Self {
        match device_type {
            HalDeviceType::Other => Self::Other,
            HalDeviceType::IntegratedGpu => Self::IntegratedGpu,
            HalDeviceType::DiscreteGpu => Self::DiscreteGpu,
            HalDeviceType::VirtualGpu => Self::VirtualGpu,
            HalDeviceType::Cpu => Self::Cpu,
        }
    }
}

pub enum AdapterInputs<'a, I> {
    IdSet(&'a [I], fn(&I) -> Backend),
    Mask(BackendBit, fn(Backend) -> I),
}

impl<I: Clone> AdapterInputs<'_, I> {
    fn find(&self, b: Backend) -> Option<I> {
        match *self {
            AdapterInputs::IdSet(ids, ref fun) => ids.iter().find(|id| fun(id) == b).cloned(),
            AdapterInputs::Mask(bits, ref fun) => {
                if bits.contains(b.into()) {
                    Some(fun(b))
                } else {
                    None
                }
            }
        }
    }
}

impl<G: GlobalIdentityHandlerFactory> Global<G> {
    #[cfg(feature = "raw-window-handle")]
    pub fn instance_create_surface(
        &self,
        handle: &impl raw_window_handle::HasRawWindowHandle,
        id_in: Input<G, SurfaceId>,
    ) -> SurfaceId {
        let surface = unsafe {
            Surface {
                #[cfg(any(
                    windows,
                    all(unix, not(any(target_os = "ios", target_os = "macos"))),
                    feature = "gfx-backend-vulkan",
                ))]
                vulkan: self
                    .instance
                    .vulkan
                    .as_ref()
                    .and_then(|inst| inst.create_surface(handle).ok()),
                #[cfg(any(target_os = "ios", target_os = "macos"))]
                metal: self
                    .instance
                    .metal
                    .as_ref()
                    .and_then(|inst| inst.create_surface(handle).ok()),
                #[cfg(windows)]
                dx12: self
                    .instance
                    .dx12
                    .as_ref()
                    .and_then(|inst| inst.create_surface(handle).ok()),
                #[cfg(windows)]
                dx11: self
                    .instance
                    .dx11
                    .as_ref()
                    .and_then(|inst| inst.create_surface(handle).ok()),
            }
        };

        let mut token = Token::root();
        self.surfaces.register_identity(id_in, surface, &mut token)
    }

    pub fn enumerate_adapters(
        &self,
        unsafe_extensions: wgt::UnsafeExtensions,
        inputs: AdapterInputs<Input<G, AdapterId>>,
    ) -> Vec<AdapterId> {
        let instance = &self.instance;
        let mut token = Token::root();
        let mut adapters = Vec::new();

        #[cfg(any(
            not(any(target_os = "ios", target_os = "macos")),
            feature = "gfx-backend-vulkan"
        ))]
        {
            if let Some(ref inst) = instance.vulkan {
                if let Some(id_vulkan) = inputs.find(Backend::Vulkan) {
                    for raw in inst.enumerate_adapters() {
                        let adapter = Adapter::new(raw, unsafe_extensions);
                        log::info!("Adapter Vulkan {:?}", adapter.raw.info);
                        adapters.push(backend::Vulkan::hub(self).adapters.register_identity(
                            id_vulkan.clone(),
                            adapter,
                            &mut token,
                        ));
                    }
                }
            }
        }
        #[cfg(any(target_os = "ios", target_os = "macos"))]
        {
            if let Some(ref inst) = instance.metal {
                if let Some(id_metal) = inputs.find(Backend::Metal) {
                    for raw in inst.enumerate_adapters() {
                        let adapter = Adapter::new(raw, unsafe_extensions);
                        log::info!("Adapter Metal {:?}", adapter.raw.info);
                        adapters.push(backend::Metal::hub(self).adapters.register_identity(
                            id_metal.clone(),
                            adapter,
                            &mut token,
                        ));
                    }
                }
            }
        }
        #[cfg(windows)]
        {
            if let Some(ref inst) = instance.dx12 {
                if let Some(id_dx12) = inputs.find(Backend::Dx12) {
                    for raw in inst.enumerate_adapters() {
                        let adapter = Adapter::new(raw, unsafe_extensions);
                        log::info!("Adapter Dx12 {:?}", adapter.raw.info);
                        adapters.push(backend::Dx12::hub(self).adapters.register_identity(
                            id_dx12.clone(),
                            adapter,
                            &mut token,
                        ));
                    }
                }
            }
            if let Some(ref inst) = instance.dx11 {
                if let Some(id_dx11) = inputs.find(Backend::Dx11) {
                    for raw in inst.enumerate_adapters() {
                        let adapter = Adapter::new(raw, unsafe_extensions);
                        log::info!("Adapter Dx11 {:?}", adapter.raw.info);
                        adapters.push(backend::Dx11::hub(self).adapters.register_identity(
                            id_dx11.clone(),
                            adapter,
                            &mut token,
                        ));
                    }
                }
            }
        }

        adapters
    }

    pub fn pick_adapter(
        &self,
        desc: &RequestAdapterOptions,
        unsafe_extensions: wgt::UnsafeExtensions,
        inputs: AdapterInputs<Input<G, AdapterId>>,
    ) -> Option<AdapterId> {
        let instance = &self.instance;
        let mut token = Token::root();
        let (surface_guard, mut token) = self.surfaces.read(&mut token);
        let compatible_surface = desc.compatible_surface.map(|id| &surface_guard[id]);
        let mut device_types = Vec::new();

        let id_vulkan = inputs.find(Backend::Vulkan);
        let id_metal = inputs.find(Backend::Metal);
        let id_dx12 = inputs.find(Backend::Dx12);
        let id_dx11 = inputs.find(Backend::Dx11);

        #[cfg(any(
            not(any(target_os = "ios", target_os = "macos")),
            feature = "gfx-backend-vulkan"
        ))]
        let mut adapters_vk = match instance.vulkan {
            Some(ref inst) if id_vulkan.is_some() => {
                let mut adapters = inst.enumerate_adapters();
                if let Some(&Surface {
                    vulkan: Some(ref surface),
                    ..
                }) = compatible_surface
                {
                    adapters.retain(|a| {
                        a.queue_families
                            .iter()
                            .find(|qf| qf.queue_type().supports_graphics())
                            .map_or(false, |qf| surface.supports_queue_family(qf))
                    });
                }
                device_types.extend(adapters.iter().map(|ad| ad.info.device_type.clone()));
                adapters
            }
            _ => Vec::new(),
        };
        #[cfg(any(target_os = "ios", target_os = "macos"))]
        let mut adapters_mtl = match instance.metal {
            Some(ref inst) if id_metal.is_some() => {
                let mut adapters = inst.enumerate_adapters();
                if let Some(&Surface {
                    metal: Some(ref surface),
                    ..
                }) = compatible_surface
                {
                    adapters.retain(|a| {
                        a.queue_families
                            .iter()
                            .find(|qf| qf.queue_type().supports_graphics())
                            .map_or(false, |qf| surface.supports_queue_family(qf))
                    });
                }
                device_types.extend(adapters.iter().map(|ad| ad.info.device_type.clone()));
                adapters
            }
            _ => Vec::new(),
        };
        #[cfg(windows)]
        let mut adapters_dx12 = match instance.dx12 {
            Some(ref inst) if id_dx12.is_some() => {
                let mut adapters = inst.enumerate_adapters();
                if let Some(&Surface {
                    dx12: Some(ref surface),
                    ..
                }) = compatible_surface
                {
                    adapters.retain(|a| {
                        a.queue_families
                            .iter()
                            .find(|qf| qf.queue_type().supports_graphics())
                            .map_or(false, |qf| surface.supports_queue_family(qf))
                    });
                }
                device_types.extend(adapters.iter().map(|ad| ad.info.device_type.clone()));
                adapters
            }
            _ => Vec::new(),
        };
        #[cfg(windows)]
        let mut adapters_dx11 = match instance.dx11 {
            Some(ref inst) if id_dx11.is_some() => {
                let mut adapters = inst.enumerate_adapters();
                if let Some(&Surface {
                    dx11: Some(ref surface),
                    ..
                }) = compatible_surface
                {
                    adapters.retain(|a| {
                        a.queue_families
                            .iter()
                            .find(|qf| qf.queue_type().supports_graphics())
                            .map_or(false, |qf| surface.supports_queue_family(qf))
                    });
                }
                device_types.extend(adapters.iter().map(|ad| ad.info.device_type.clone()));
                adapters
            }
            _ => Vec::new(),
        };

        if device_types.is_empty() {
            log::warn!("No adapters are available!");
            return None;
        }

        let (mut integrated, mut discrete, mut virt, mut other) = (None, None, None, None);

        for (i, ty) in device_types.into_iter().enumerate() {
            match ty {
                hal::adapter::DeviceType::IntegratedGpu => {
                    integrated = integrated.or(Some(i));
                }
                hal::adapter::DeviceType::DiscreteGpu => {
                    discrete = discrete.or(Some(i));
                }
                hal::adapter::DeviceType::VirtualGpu => {
                    virt = virt.or(Some(i));
                }
                _ => {
                    other = other.or(Some(i));
                }
            }
        }

        let preferred_gpu = match desc.power_preference {
            PowerPreference::Default => match power::is_battery_discharging() {
                Ok(false) => discrete.or(integrated).or(other).or(virt),
                Ok(true) => integrated.or(discrete).or(other).or(virt),
                Err(err) => {
                    log::debug!(
                        "Power info unavailable, preferring integrated gpu ({})",
                        err
                    );
                    integrated.or(discrete).or(other).or(virt)
                }
            },
            PowerPreference::LowPower => integrated.or(other).or(discrete).or(virt),
            PowerPreference::HighPerformance => discrete.or(other).or(integrated).or(virt),
        };

        let mut selected = preferred_gpu.unwrap_or(0);
        #[cfg(any(
            not(any(target_os = "ios", target_os = "macos")),
            feature = "gfx-backend-vulkan"
        ))]
        {
            if selected < adapters_vk.len() {
                let adapter = Adapter::new(adapters_vk.swap_remove(selected), unsafe_extensions);
                log::info!("Adapter Vulkan {:?}", adapter.raw.info);
                let id = backend::Vulkan::hub(self).adapters.register_identity(
                    id_vulkan.unwrap(),
                    adapter,
                    &mut token,
                );
                return Some(id);
            }
            selected -= adapters_vk.len();
        }
        #[cfg(any(target_os = "ios", target_os = "macos"))]
        {
            if selected < adapters_mtl.len() {
                let adapter = Adapter::new(adapters_mtl.swap_remove(selected), unsafe_extensions);
                log::info!("Adapter Metal {:?}", adapter.raw.info);
                let id = backend::Metal::hub(self).adapters.register_identity(
                    id_metal.unwrap(),
                    adapter,
                    &mut token,
                );
                return Some(id);
            }
            selected -= adapters_mtl.len();
        }
        #[cfg(windows)]
        {
            if selected < adapters_dx12.len() {
                let adapter = Adapter::new(adapters_dx12.swap_remove(selected), unsafe_extensions);
                log::info!("Adapter Dx12 {:?}", adapter.raw.info);
                let id = backend::Dx12::hub(self).adapters.register_identity(
                    id_dx12.unwrap(),
                    adapter,
                    &mut token,
                );
                return Some(id);
            }
            selected -= adapters_dx12.len();
            if selected < adapters_dx11.len() {
                let adapter = Adapter::new(adapters_dx11.swap_remove(selected), unsafe_extensions);
                log::info!("Adapter Dx11 {:?}", adapter.raw.info);
                let id = backend::Dx11::hub(self).adapters.register_identity(
                    id_dx11.unwrap(),
                    adapter,
                    &mut token,
                );
                return Some(id);
            }
            selected -= adapters_dx11.len();
        }

        let _ = (selected, id_vulkan, id_metal, id_dx12, id_dx11);
        log::warn!("Some adapters are present, but enumerating them failed!");
        None
    }

    pub fn adapter_get_info<B: GfxBackend>(&self, adapter_id: AdapterId) -> AdapterInfo {
        let hub = B::hub(self);
        let mut token = Token::root();
        let (adapter_guard, _) = hub.adapters.read(&mut token);
        let adapter = &adapter_guard[adapter_id];
        AdapterInfo::from_gfx(adapter.raw.info.clone(), adapter_id.backend())
    }

    pub fn adapter_extensions<B: GfxBackend>(&self, adapter_id: AdapterId) -> wgt::Extensions {
        let hub = B::hub(self);
        let mut token = Token::root();
        let (adapter_guard, _) = hub.adapters.read(&mut token);
        let adapter = &adapter_guard[adapter_id];

        adapter.extensions
    }

    pub fn adapter_limits<B: GfxBackend>(&self, adapter_id: AdapterId) -> wgt::Limits {
        let hub = B::hub(self);
        let mut token = Token::root();
        let (adapter_guard, _) = hub.adapters.read(&mut token);
        let adapter = &adapter_guard[adapter_id];

        adapter.limits.clone()
    }

    pub fn adapter_capabilities<B: GfxBackend>(&self, adapter_id: AdapterId) -> wgt::Capabilities {
        let hub = B::hub(self);
        let mut token = Token::root();
        let (adapter_guard, _) = hub.adapters.read(&mut token);
        let adapter = &adapter_guard[adapter_id];

        adapter.capabilities
    }

    pub fn adapter_destroy<B: GfxBackend>(&self, adapter_id: AdapterId) {
        let hub = B::hub(self);
        let mut token = Token::root();
        let (mut guard, _) = hub.adapters.write(&mut token);

        if guard[adapter_id]
            .life_guard
            .ref_count
            .take()
            .unwrap()
            .load()
            == 1
        {
            hub.adapters.free_id(adapter_id);
            let _adapter = guard.remove(adapter_id).unwrap();
        }
    }
}

impl<G: GlobalIdentityHandlerFactory> Global<G> {
    pub fn adapter_request_device<B: GfxBackend>(
        &self,
        adapter_id: AdapterId,
        desc: &DeviceDescriptor,
        trace_path: Option<&std::path::Path>,
        id_in: Input<G, DeviceId>,
    ) -> DeviceId {
        let hub = B::hub(self);
        let mut token = Token::root();
        let device = {
            let (adapter_guard, _) = hub.adapters.read(&mut token);
            let adapter = &adapter_guard[adapter_id];
            let phd = &adapter.raw.physical_device;

            // Verify all extensions were exposed by the adapter
            if !adapter.unsafe_extensions.allowed() {
                assert!(
                    !desc.extensions.intersects(wgt::Extensions::ALL_UNSAFE),
                    "Cannot enable unsafe extensions without passing UnsafeExtensions::allow() when getting an adapter. Enabled unsafe extensions: {:?}",
                    desc.extensions & wgt::Extensions::ALL_UNSAFE
                )
            }
            assert!(
                adapter.extensions.contains(desc.extensions),
                "Cannot enable extensions that adapter doesn't support. Unsupported extensions: {:?}",
                desc.extensions - adapter.extensions
            );

            // Verify extension preconditions
            if desc
                .extensions
                .contains(wgt::Extensions::MAPPABLE_PRIMARY_BUFFERS)
                && adapter.raw.info.device_type == hal::adapter::DeviceType::DiscreteGpu
            {
                log::warn!("Extension MAPPABLE_PRIMARY_BUFFERS enabled on a discrete gpu. This is a massive performance footgun and likely not what you wanted");
            }

            let available_features = adapter.raw.physical_device.features();

            // Check features that are always needed
            let wishful_features = hal::Features::VERTEX_STORES_AND_ATOMICS
                | hal::Features::FRAGMENT_STORES_AND_ATOMICS
                | hal::Features::NDC_Y_UP;
            let mut enabled_features = available_features & wishful_features;
            if enabled_features != wishful_features {
                log::warn!(
                    "Missing features: {:?}",
                    wishful_features - enabled_features
                );
            }

            // Extensions
            enabled_features.set(
                hal::Features::SAMPLER_ANISOTROPY,
                desc.extensions
                    .contains(wgt::Extensions::ANISOTROPIC_FILTERING),
            );

            let mut enabled_capabilities = adapter.capabilities & wgt::Capabilities::ALL_BUILT_IN;

            // Capabilities without extension gates
            enabled_features.set(
                hal::Features::TEXTURE_DESCRIPTOR_ARRAY,
                adapter
                    .capabilities
                    .contains(wgt::Capabilities::SAMPLED_TEXTURE_BINDING_ARRAY),
            );
            enabled_features.set(
                hal::Features::SHADER_SAMPLED_IMAGE_ARRAY_DYNAMIC_INDEXING,
                adapter
                    .capabilities
                    .contains(wgt::Capabilities::SAMPLED_TEXTURE_ARRAY_DYNAMIC_INDEXING),
            );

            // Capabilities behind BINDING_INDEXING
            if desc.extensions.contains(wgt::Extensions::BINDING_INDEXING) {
                enabled_capabilities
                    .insert(adapter.capabilities & wgt::Capabilities::ALL_BINDING_INDEXING);
                enabled_features.set(
                    hal::Features::SAMPLED_TEXTURE_DESCRIPTOR_INDEXING,
                    adapter
                        .capabilities
                        .contains(wgt::Capabilities::SAMPLED_TEXTURE_ARRAY_NON_UNIFORM_INDEXING),
                );
                enabled_features.set(
                    hal::Features::UNSIZED_DESCRIPTOR_ARRAY,
                    adapter
                        .capabilities
                        .contains(wgt::Capabilities::UNSIZED_BINDING_ARRAY),
                );
            }

            let family = adapter
                .raw
                .queue_families
                .iter()
                .find(|family| family.queue_type().supports_graphics())
                .unwrap();
            let mut gpu = unsafe { phd.open(&[(family, &[1.0])], enabled_features).unwrap() };

            let limits = phd.limits();
            assert_eq!(
                0,
                BIND_BUFFER_ALIGNMENT % limits.min_storage_buffer_offset_alignment,
                "Adapter storage buffer offset alignment not compatible with WGPU"
            );
            assert_eq!(
                0,
                BIND_BUFFER_ALIGNMENT % limits.min_uniform_buffer_offset_alignment,
                "Adapter uniform buffer offset alignment not compatible with WGPU"
            );
            if limits.max_bound_descriptor_sets == 0 {
                log::warn!("max_bind_groups limit is missing");
            } else {
                assert!(
                    u32::from(limits.max_bound_descriptor_sets) >= desc.limits.max_bind_groups,
                    "Adapter does not support the requested max_bind_groups"
                );
            }

            let mem_props = phd.memory_properties();
            if !desc.shader_validation {
                log::warn!("Shader validation is disabled");
            }
            let private_features = PrivateFeatures {
                shader_validation: desc.shader_validation,
                texture_d24_s8: phd
                    .format_properties(Some(hal::format::Format::D24UnormS8Uint))
                    .optimal_tiling
                    .contains(hal::format::ImageFeature::DEPTH_STENCIL_ATTACHMENT),
            };

            Device::new(
                gpu.device,
                Stored {
                    value: adapter_id,
                    ref_count: adapter.life_guard.add_ref(),
                },
                gpu.queue_groups.swap_remove(0),
                mem_props,
                limits,
                private_features,
                desc,
                enabled_capabilities,
                trace_path,
            )
        };

        hub.devices.register_identity(id_in, device, &mut token)
    }
}
