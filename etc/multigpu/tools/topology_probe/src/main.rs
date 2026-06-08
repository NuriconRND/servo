use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TopologyReport {
    gpus: Vec<GpuInfo>,
    dxgi_adapters: Vec<DxgiAdapterInfo>,
    monitors: Vec<MonitorInfo>,
    layout_validation: Option<LayoutValidation>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GpuInfo {
    index: usize,
    name: String,
    backend: String,
    device_type: String,
    vendor_id: u32,
    device_id: u32,
    pci_bus_id: String,
    driver: String,
    driver_info: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MonitorInfo {
    index: usize,
    device_name: String,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    width: i32,
    height: i32,
    primary: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DxgiAdapterInfo {
    index: usize,
    description: String,
    vendor_id: u32,
    device_id: u32,
    dedicated_video_memory: usize,
    dedicated_system_memory: usize,
    shared_system_memory: usize,
    luid: String,
    flags: u32,
    outputs: Vec<DxgiOutputInfo>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DxgiOutputInfo {
    index: usize,
    device_name: String,
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    width: i32,
    height: i32,
    attached_to_desktop: bool,
    rotation: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WallLayout {
    virtual_viewport: ViewportSize,
    tiles: Vec<TileConfig>,
    overlap_px: u32,
}

#[derive(Debug, Deserialize)]
struct ViewportSize {
    width: i32,
    height: i32,
}

#[derive(Debug, Deserialize)]
struct TileConfig {
    monitor: usize,
    gpu: usize,
    rect: [i32; 4],
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LayoutValidation {
    layout_path: String,
    valid: bool,
    errors: Vec<String>,
    warnings: Vec<String>,
}

fn main() -> Result<()> {
    let layout_path = parse_layout_arg()?;
    let gpus = enumerate_gpus();
    let dxgi_adapters = enumerate_dxgi_adapters();
    let monitors = enumerate_monitors()?;
    let render_gpu_count = if dxgi_adapters.is_empty() {
        gpus.len()
    } else {
        dxgi_adapters.len()
    };
    let layout_validation = match layout_path {
        Some(path) => Some(validate_layout_file(&path, render_gpu_count, monitors.len())?),
        None => None,
    };

    let report = TopologyReport {
        gpus,
        dxgi_adapters,
        monitors,
        layout_validation,
    };

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn parse_layout_arg() -> Result<Option<PathBuf>> {
    let mut args = std::env::args().skip(1);
    let mut layout_path = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--layout" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow!("--layout requires a file path"))?;
                layout_path = Some(PathBuf::from(value));
            },
            "--help" | "-h" => {
                println!("Usage: topology_probe [--layout <wall-layout.json>]");
                std::process::exit(0);
            },
            other => return Err(anyhow!("unknown argument: {other}")),
        }
    }

    Ok(layout_path)
}

fn enumerate_gpus() -> Vec<GpuInfo> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));

    adapters
        .into_iter()
        .enumerate()
        .map(|(index, adapter)| {
            let info = adapter.get_info();
            GpuInfo {
                index,
                name: info.name,
                backend: format!("{:?}", info.backend),
                device_type: format!("{:?}", info.device_type),
                vendor_id: info.vendor,
                device_id: info.device,
                pci_bus_id: info.device_pci_bus_id,
                driver: info.driver,
                driver_info: info.driver_info,
            }
        })
        .collect()
}

#[cfg(target_os = "windows")]
fn enumerate_dxgi_adapters() -> Vec<DxgiAdapterInfo> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

    let mut adapters = Vec::new();

    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return adapters;
        };

        let mut adapter_index = 0;
        while let Ok(adapter) = factory.EnumAdapters1(adapter_index) {
            let Ok(desc) = adapter.GetDesc1() else {
                adapter_index += 1;
                continue;
            };

            let mut outputs = Vec::new();
            let mut output_index = 0;
            while let Ok(output) = adapter.EnumOutputs(output_index) {
                if let Ok(output_desc) = output.GetDesc() {
                    let rect = output_desc.DesktopCoordinates;
                    outputs.push(DxgiOutputInfo {
                        index: output_index as usize,
                        device_name: utf16_z_to_string(&output_desc.DeviceName),
                        left: rect.left,
                        top: rect.top,
                        right: rect.right,
                        bottom: rect.bottom,
                        width: rect.right - rect.left,
                        height: rect.bottom - rect.top,
                        attached_to_desktop: output_desc.AttachedToDesktop.as_bool(),
                        rotation: format!("{:?}", output_desc.Rotation),
                    });
                }
                output_index += 1;
            }

            adapters.push(DxgiAdapterInfo {
                index: adapter_index as usize,
                description: utf16_z_to_string(&desc.Description),
                vendor_id: desc.VendorId,
                device_id: desc.DeviceId,
                dedicated_video_memory: desc.DedicatedVideoMemory,
                dedicated_system_memory: desc.DedicatedSystemMemory,
                shared_system_memory: desc.SharedSystemMemory,
                luid: format!("{:08x}:{:08x}", desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart),
                flags: desc.Flags,
                outputs,
            });

            adapter_index += 1;
        }
    }

    adapters
}

#[cfg(not(target_os = "windows"))]
fn enumerate_dxgi_adapters() -> Vec<DxgiAdapterInfo> {
    Vec::new()
}

#[cfg(target_os = "windows")]
fn enumerate_monitors() -> Result<Vec<MonitorInfo>> {
    use std::mem::size_of;
    use windows::core::BOOL;
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    };
    use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

    unsafe extern "system" fn enum_proc(
        monitor: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let monitors = &mut *(data.0 as *mut Vec<MonitorInfo>);
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;

        let ok = GetMonitorInfoW(
            monitor,
            &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
        )
        .as_bool();

        if ok {
            let rect = info.monitorInfo.rcMonitor;
            let device_name = utf16_z_to_string(&info.szDevice);
            monitors.push(MonitorInfo {
                index: monitors.len(),
                device_name,
                left: rect.left,
                top: rect.top,
                right: rect.right,
                bottom: rect.bottom,
                width: rect.right - rect.left,
                height: rect.bottom - rect.top,
                primary: (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0,
            });
        }

        BOOL(1)
    }

    let mut monitors = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(enum_proc),
            LPARAM(&mut monitors as *mut Vec<MonitorInfo> as isize),
        )
        .ok()
        .context("EnumDisplayMonitors failed")?;
    }

    Ok(monitors)
}

#[cfg(not(target_os = "windows"))]
fn enumerate_monitors() -> Result<Vec<MonitorInfo>> {
    Ok(Vec::new())
}

fn utf16_z_to_string(value: &[u16]) -> String {
    let len = value.iter().position(|code| *code == 0).unwrap_or(value.len());
    String::from_utf16_lossy(&value[..len])
}

fn validate_layout_file(path: &PathBuf, gpu_count: usize, monitor_count: usize) -> Result<LayoutValidation> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read layout file {}", path.display()))?;
    let layout: WallLayout = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse layout file {}", path.display()))?;

    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut seen_monitors = HashSet::new();

    if layout.virtual_viewport.width <= 0 || layout.virtual_viewport.height <= 0 {
        errors.push("virtualViewport width and height must be positive".to_string());
    }

    if layout.tiles.is_empty() {
        errors.push("tiles must contain at least one tile".to_string());
    }

    for (index, tile) in layout.tiles.iter().enumerate() {
        let [x, y, width, height] = tile.rect;

        if width <= 0 || height <= 0 {
            errors.push(format!("tile {index} rect width and height must be positive"));
            continue;
        }

        if x < 0
            || y < 0
            || x + width > layout.virtual_viewport.width
            || y + height > layout.virtual_viewport.height
        {
            errors.push(format!(
                "tile {index} rect [{x}, {y}, {width}, {height}] exceeds virtualViewport"
            ));
        }

        if tile.monitor >= monitor_count {
            errors.push(format!(
                "tile {index} monitor {} is out of range; detected monitor count is {monitor_count}",
                tile.monitor
            ));
        }

        if tile.gpu >= gpu_count {
            errors.push(format!(
                "tile {index} gpu {} is out of range; detected gpu count is {gpu_count}",
                tile.gpu
            ));
        }

        if !seen_monitors.insert(tile.monitor) {
            warnings.push(format!("monitor {} is assigned by more than one tile", tile.monitor));
        }

        let shortest_edge = width.min(height) as u32;
        if layout.overlap_px.saturating_mul(2) >= shortest_edge {
            warnings.push(format!(
                "tile {index} overlapPx {} is large relative to tile size {width}x{height}",
                layout.overlap_px
            ));
        }
    }

    Ok(LayoutValidation {
        layout_path: path.display().to_string(),
        valid: errors.is_empty(),
        errors,
        warnings,
    })
}
