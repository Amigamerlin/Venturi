use super::*;

fn gpu(name: &str, vendor: u32, device_type: wgpu::DeviceType) -> wgpu::AdapterInfo {
    wgpu::AdapterInfo {
        name: name.into(),
        vendor,
        ..wgpu::AdapterInfo::new(device_type, wgpu::Backend::Vulkan)
    }
}

fn intel() -> wgpu::AdapterInfo {
    gpu(
        "Intel(R) UHD Graphics 620",
        0x8086,
        wgpu::DeviceType::IntegratedGpu,
    )
}

fn nvidia() -> wgpu::AdapterInfo {
    gpu(
        "NVIDIA GeForce RTX 2070 SUPER",
        NVIDIA_VENDOR_ID,
        wgpu::DeviceType::DiscreteGpu,
    )
}

fn lavapipe() -> wgpu::AdapterInfo {
    gpu(
        "llvmpipe (LLVM 20.1.8, 256 bits)",
        0x10005,
        wgpu::DeviceType::Cpu,
    )
}

fn vulkan(gpu: &wgpu::AdapterInfo) -> HwDevice {
    HwDevice::Vulkan(Some(gpu.name.clone()))
}

#[test]
fn auto_on_a_hybrid_laptop_decodes_on_the_integrated_gpu() {
    let devices = resolve(
        HwDecodeMode::Auto,
        &[nvidia(), intel(), lavapipe()],
        Os::Linux,
    );
    assert_eq!(devices, [vulkan(&intel())]);
}

#[test]
fn auto_on_an_nvidia_desktop_prefers_nvdec_then_vulkan() {
    let devices = resolve(HwDecodeMode::Auto, &[lavapipe(), nvidia()], Os::Linux);
    assert_eq!(devices, [HwDevice::Cuda, vulkan(&nvidia())]);
}

#[test]
fn auto_without_nvidia_goes_through_vulkan_only() {
    let amd = gpu(
        "AMD Radeon RX 7600 (RADV NAVI33)",
        0x1002,
        wgpu::DeviceType::DiscreteGpu,
    );
    let devices = resolve(HwDecodeMode::Auto, std::slice::from_ref(&amd), Os::Linux);
    assert_eq!(devices, [vulkan(&amd)]);
}

#[test]
fn a_software_vulkan_driver_is_never_a_decoder() {
    assert!(resolve(HwDecodeMode::Auto, &[lavapipe()], Os::Linux).is_empty());
    assert!(resolve(HwDecodeMode::Vulkan, &[lavapipe()], Os::Linux).is_empty());
}

#[test]
fn explicit_choices_override_the_hybrid_rule() {
    let gpus = [intel(), nvidia()];
    assert_eq!(
        resolve(HwDecodeMode::Nvdec, &gpus, Os::Linux),
        [HwDevice::Cuda]
    );
    assert_eq!(
        resolve(HwDecodeMode::Vulkan, &gpus, Os::Linux),
        [vulkan(&nvidia())]
    );
    assert!(resolve(HwDecodeMode::Off, &gpus, Os::Linux).is_empty());
}

#[test]
fn macos_decodes_on_videotoolbox_unless_off() {
    assert_eq!(
        resolve(HwDecodeMode::Auto, &[], Os::Mac),
        [HwDevice::VideoToolbox]
    );
    assert!(resolve(HwDecodeMode::Off, &[], Os::Mac).is_empty());
}

#[test]
fn modes_round_trip_through_their_ids() {
    for mode in [
        HwDecodeMode::Auto,
        HwDecodeMode::Off,
        HwDecodeMode::Nvdec,
        HwDecodeMode::Vulkan,
    ] {
        assert_eq!(HwDecodeMode::from_id(mode.id()), Some(mode));
    }
}

#[test]
fn device_labels_name_the_backend_and_the_gpu() {
    assert_eq!(device_label(&HwDevice::VideoToolbox), "VideoToolbox");
    assert_eq!(device_label(&HwDevice::Cuda), "NVDEC");
    assert_eq!(
        device_label(&vulkan(&intel())),
        "Vulkan, Intel(R) UHD Graphics 620"
    );
}

#[test]
fn windows_auto_tries_nvdec_then_d3d11va_then_vulkan() {
    let devices = resolve(HwDecodeMode::Auto, &[nvidia()], Os::Windows);
    assert_eq!(devices[..2], [HwDevice::Cuda, HwDevice::D3d11va]);
    assert!(matches!(devices[2], HwDevice::Vulkan(Some(_))));
}

#[test]
fn windows_without_vulkan_still_decodes_on_d3d11va() {
    assert_eq!(
        resolve(HwDecodeMode::Auto, &[], Os::Windows),
        [HwDevice::D3d11va]
    );
    assert_eq!(
        resolve(HwDecodeMode::D3d11va, &[], Os::Windows),
        [HwDevice::D3d11va]
    );
}

#[test]
fn d3d11va_mode_round_trips_its_id() {
    assert_eq!(
        HwDecodeMode::from_id(HwDecodeMode::D3d11va.id()),
        Some(HwDecodeMode::D3d11va)
    );
}
