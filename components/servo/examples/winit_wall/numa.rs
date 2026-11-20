/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! 벽 NUMA 표 (2026-11-18 추가) -- 기동 때 CPU 와 GPU 의 NUMA 노드를 알아내 타일마다 기억한다.
//!
//! 표 자체, 핀, 영상 배치는 `servo_config::wall_numa` 에 있다. 여기는 그 표를 채우고, 렌더 스레드를
//! 어떻게 핀할지 정한다.
//!
//! GPU 노드는 어댑터 LUID 로 찾는다. 벽의 GPU 넷은 같은 RX 580 이라 Vendor/Device ID 로는 가를 수
//! 없다. 순서는 이렇다:
//! 1. `D3DKMTOpenAdapterFromLuid` -> `KMTQAITYPE_PHYSICALADAPTERPNPKEY` 로 PnP 키를 얻는다.
//!    레지스트리 경로라 인스턴스 ID 만 떼어 낸다(`instance_id_from_pnp_key`).
//! 2. `CM_Locate_DevNodeW` -> `DEVPKEY_Device_Numa_Node`.
//! 3. 1 이 안 되면 `KMTQAITYPE_ADAPTERADDRESS`(PCI bus/device/function)를 디스플레이 클래스 장치의
//!    `DEVPKEY_Device_BusNumber`/`Address` 와 맞춘다.
//!
//! 노드를 모르면(속성이 없음, `0xFFFFFFFF`, 프로세서가 없는 노드) 그 타일은 핀하지 않는다. 0 으로
//! 치지 않는다 -- 노드가 하나인 장비도 GPU 에 `Numa_Node=0` 을 준다(2026-11-18 개발 장비 실측).

use servo::dxgi_luid_for_gpu_index;
use servo_config::wall_numa::{self, CpuNode, GpuNode, TileNode, Topology};

/// 렌더 스레드를 어떻게 핀할지.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RenderPin {
    /// 핀하지 않는다(이유).
    Off(String),
    /// 타일마다 그 렌더 스레드를 그 타일의 노드에(병렬 타일: 타일 0 은 메인, 나머지는 자기 스레드).
    PerTile,
    /// 직렬: 메인 스레드가 모든 타일을 그리고, 그 GPU 들이 모두 이 노드에 있다.
    Main(u16),
}

/// 표를 만들어 [`wall_numa::publish`] 하고 `WALLNUMA` 줄을 찍는다. `tiles` 는 (레이아웃 타일 번호,
/// 최종 GPU 인덱스) 이고, 창 계획(`TilePlan`)에서 온다 -- 레이아웃의 `gpu` 덮어쓰기가 반영된 값이다.
pub(crate) fn build(tiles: &[(usize, Option<usize>)]) -> Option<&'static Topology> {
    let (os_nodes, cores) = wall_numa::enumerate_cpu();
    if os_nodes.is_empty() {
        eprintln!(
            "WALLNUMA warning: no CPU NUMA node could be enumerated (GetLogicalProcessorInformationEx); \
             thread placement is off"
        );
    }
    let fake_split = servo_config::pref!(gfx_wall_numa_fake_split).clamp(0, 8) as u32;
    let nodes: Vec<CpuNode> = if fake_split > 1 {
        wall_numa::split_nodes(&os_nodes, &cores, fake_split)
    } else {
        os_nodes.clone()
    };

    let mut gpus: Vec<GpuNode> = Vec::new();
    for &(_, gpu_index) in tiles {
        let Some(gpu_index) = gpu_index else {
            continue;
        };
        if gpus.iter().any(|gpu| gpu.gpu_index == gpu_index) {
            continue;
        }
        let luid = dxgi_luid_for_gpu_index(gpu_index);
        let (pci, os_node, how) = match luid {
            Some(luid) => gpu_location(luid),
            None => (None, None, "no DXGI adapter at this index".to_owned()),
        };
        // 프로세서가 있는 노드만 받는다(메모리만 있는 노드도 있다).
        let (os_node, how) = match os_node {
            Some(_) if os_nodes.is_empty() => (None, format!("{how}; no CPU nodes enumerated")),
            Some(node) if os_nodes.iter().any(|entry| entry.os_node == node) => (Some(node), how),
            Some(node) => (None, format!("{how}; node {node} has no processors")),
            None => (None, how),
        };
        gpus.push(GpuNode {
            gpu_index,
            luid,
            pci,
            os_node,
            how,
        });
    }

    let spec = servo_config::pref!(gfx_wall_numa_tile_nodes);
    let overrides = match wall_numa::parse_tile_nodes(&spec) {
        Ok(overrides) => overrides,
        Err(error) => {
            eprintln!("WALLNUMA warning: ignoring gfx_wall_numa_tile_nodes={spec:?}: {error}");
            Vec::new()
        },
    };
    let lookup = Topology {
        nodes: nodes.clone(),
        ..Default::default()
    };
    let tile_nodes: Vec<TileNode> = tiles
        .iter()
        .map(|&(tile_index, gpu_index)| {
            let detected = gpu_index
                .and_then(|index| gpus.iter().find(|gpu| gpu.gpu_index == index))
                .and_then(|gpu| gpu.os_node)
                .and_then(|node| lookup.table_node_for_os(node));
            let forced = overrides.get(tile_index).copied().flatten();
            let forced = match forced {
                Some(node) if lookup.node(node).is_some() => Some(node),
                Some(node) => {
                    eprintln!(
                        "WALLNUMA warning: tile {tile_index}: override node {node} is not in the \
                         table ({} node(s)); using the detected node",
                        nodes.len()
                    );
                    None
                },
                None => None,
            };
            TileNode {
                tile_index,
                gpu_index,
                node: forced.or(detected),
                overridden: forced.is_some(),
            }
        })
        .collect();

    let groups = {
        let mut groups: Vec<u16> = os_nodes.iter().map(|entry| entry.group).collect();
        groups.sort_unstable();
        groups.dedup();
        groups.len()
    };
    let mut line = format!(
        "WALLNUMA cpu os_nodes={} groups={groups} fake_split={fake_split} table_nodes={}",
        os_nodes.len(),
        nodes.len()
    );
    for entry in &nodes {
        line.push_str(&format!(
            " | n{}(os{}) g{} mask=0x{:x} lp={} cores={}",
            entry.node, entry.os_node, entry.group, entry.mask, entry.logical, entry.cores
        ));
        if entry.groups_spanned > 1 {
            line.push_str(&format!(
                " SPANS {} groups (only g{} is used)",
                entry.groups_spanned, entry.group
            ));
        }
    }
    eprintln!("{line}");
    for gpu in &gpus {
        eprintln!(
            "WALLNUMA gpu a{} luid={} pci={} os_node={} via={}",
            gpu.gpu_index,
            gpu.luid
                .map(|(high, low)| format!("{high:08x}:{low:08x}"))
                .unwrap_or_else(|| "-".to_owned()),
            gpu.pci
                .map(|(bus, device, function)| format!("{bus}:{device}.{function}"))
                .unwrap_or_else(|| "-".to_owned()),
            gpu.os_node
                .map(|node| node.to_string())
                .unwrap_or_else(|| "unknown".to_owned()),
            gpu.how,
        );
    }
    for tile in &tile_nodes {
        eprintln!(
            "WALLNUMA tile {} gpu={} node={}{}",
            tile.tile_index,
            tile.gpu_index
                .map(|index| format!("a{index}"))
                .unwrap_or_else(|| "default".to_owned()),
            tile.node
                .map(|node| node.to_string())
                .unwrap_or_else(|| "unknown".to_owned()),
            if tile.overridden { " (override)" } else { "" },
        );
    }

    wall_numa::publish(Topology {
        nodes,
        gpus,
        tiles: tile_nodes,
        fake_split,
    });
    wall_numa::topology()
}

/// 렌더 스레드 핀 계획. `tile_indices` 는 이 프로세스가 그리는 타일(슬롯 순서)이다.
pub(crate) fn render_pin_plan(
    topology: Option<&Topology>,
    parallel_tiles: bool,
    tile_indices: &[usize],
) -> RenderPin {
    if !servo_config::pref!(gfx_wall_numa_render_pin) {
        return RenderPin::Off("gfx_wall_numa_render_pin=false".to_owned());
    }
    let Some(topology) = topology else {
        return RenderPin::Off("no NUMA table".to_owned());
    };
    if topology.nodes.is_empty() {
        return RenderPin::Off("no CPU NUMA nodes enumerated".to_owned());
    }
    if !topology.multi_node() {
        return RenderPin::Off("one NUMA node".to_owned());
    }
    if parallel_tiles {
        return RenderPin::PerTile;
    }
    let nodes: Vec<Option<u16>> = tile_indices
        .iter()
        .map(|&tile_index| topology.tile(tile_index).and_then(|tile| tile.node))
        .collect();
    match nodes.first().copied().flatten() {
        Some(first) if nodes.iter().all(|&node| node == Some(first)) => RenderPin::Main(first),
        _ => RenderPin::Off(format!(
            "serial tiles (no -ParallelTiles): the main thread renders tiles on nodes {nodes:?}"
        )),
    }
}

/// 타일 번호 -> 그 타일의 노드.
pub(crate) fn tile_node(tile_index: usize) -> Option<u16> {
    wall_numa::topology()
        .and_then(|topology| topology.tile(tile_index))
        .and_then(|tile| tile.node)
}

/// 이 스레드를 `node` 에 핀하고 한 줄 남긴다(`who` 는 스레드 설명).
pub(crate) fn pin_and_report(node: u16, who: &str) {
    match wall_numa::pin_current_thread(node) {
        Ok(report) => eprintln!("WALLNUMA pin {who}: {report}"),
        Err(error) => eprintln!("WALLNUMA pin {who}: node={node} FAILED: {error}"),
    }
}

#[cfg(target_os = "windows")]
fn gpu_location(luid: (i32, u32)) -> (Option<(u32, u32, u32)>, Option<u16>, String) {
    use windows_sys::Wdk::Graphics::Direct3D::{
        D3DKMT_ADAPTERADDRESS, D3DKMT_CLOSEADAPTER, D3DKMT_OPENADAPTERFROMLUID,
        D3DKMT_PNP_KEY_HARDWARE, D3DKMT_QUERY_PHYSICAL_ADAPTER_PNP_KEY, D3DKMT_QUERYADAPTERINFO,
        D3DKMTCloseAdapter, D3DKMTOpenAdapterFromLuid, D3DKMTQueryAdapterInfo,
        KMTQAITYPE_ADAPTERADDRESS, KMTQAITYPE_PHYSICALADAPTERPNPKEY,
    };
    use windows_sys::Win32::Foundation::LUID;

    let mut open = D3DKMT_OPENADAPTERFROMLUID {
        AdapterLuid: LUID {
            LowPart: luid.1,
            HighPart: luid.0,
        },
        hAdapter: 0,
    };
    // SAFETY: 입력 구조체는 LUID 만 채워져 있고, 성공하면 hAdapter 를 받는다.
    let status = unsafe { D3DKMTOpenAdapterFromLuid(&mut open) };
    if status != 0 {
        return (
            None,
            None,
            format!("D3DKMTOpenAdapterFromLuid status 0x{status:08x}"),
        );
    }

    let mut address = D3DKMT_ADAPTERADDRESS::default();
    let mut query = D3DKMT_QUERYADAPTERINFO {
        hAdapter: open.hAdapter,
        Type: KMTQAITYPE_ADAPTERADDRESS,
        pPrivateDriverData: &mut address as *mut D3DKMT_ADAPTERADDRESS as *mut std::ffi::c_void,
        PrivateDriverDataSize: std::mem::size_of::<D3DKMT_ADAPTERADDRESS>() as u32,
    };
    // SAFETY: 출력 버퍼는 위 구조체이고 크기를 함께 준다.
    let pci = (unsafe { D3DKMTQueryAdapterInfo(&mut query) } == 0 && address.BusNumber != u32::MAX)
        .then_some((
            address.BusNumber,
            address.DeviceNumber,
            address.FunctionNumber,
        ));

    let mut buffer = vec![0u16; 1024];
    let mut chars = buffer.len() as u32;
    let mut pnp = D3DKMT_QUERY_PHYSICAL_ADAPTER_PNP_KEY {
        PhysicalAdapterIndex: 0,
        PnPKeyType: D3DKMT_PNP_KEY_HARDWARE,
        pDest: buffer.as_mut_ptr(),
        pCchDest: &mut chars,
    };
    let mut query = D3DKMT_QUERYADAPTERINFO {
        hAdapter: open.hAdapter,
        Type: KMTQAITYPE_PHYSICALADAPTERPNPKEY,
        pPrivateDriverData: &mut pnp as *mut D3DKMT_QUERY_PHYSICAL_ADAPTER_PNP_KEY
            as *mut std::ffi::c_void,
        PrivateDriverDataSize: std::mem::size_of::<D3DKMT_QUERY_PHYSICAL_ADAPTER_PNP_KEY>() as u32,
    };
    // SAFETY: pDest/pCchDest 는 위 버퍼와 그 길이(문자 수)를 가리킨다.
    let pnp_status = unsafe { D3DKMTQueryAdapterInfo(&mut query) };
    // SAFETY: 위에서 연 핸들을 한 번 닫는다.
    unsafe {
        D3DKMTCloseAdapter(&D3DKMT_CLOSEADAPTER {
            hAdapter: open.hAdapter,
        });
    }

    let mut how = String::new();
    if pnp_status == 0 {
        let length = buffer
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(buffer.len());
        let key = String::from_utf16_lossy(&buffer[..length]);
        match wall_numa::instance_id_from_pnp_key(&key) {
            Some(instance) => match cfgmgr::locate(&instance) {
                Some(devnode) => match cfgmgr::numa_node(devnode) {
                    Some(node) => return (pci, Some(node), "pnpkey".to_owned()),
                    None => how.push_str("pnpkey: no DEVPKEY_Device_Numa_Node"),
                },
                None => how.push_str("pnpkey: CM_Locate_DevNodeW failed"),
            },
            None => how.push_str("pnpkey: unexpected format"),
        }
    } else {
        how.push_str(&format!("pnpkey: status 0x{pnp_status:08x}"));
    }

    if let Some((bus, device, function)) = pci {
        for instance in cfgmgr::display_device_ids() {
            let Some(devnode) = cfgmgr::locate(&instance) else {
                continue;
            };
            if cfgmgr::u32_property(devnode, cfgmgr::BUS_NUMBER) == Some(bus)
                && cfgmgr::u32_property(devnode, cfgmgr::ADDRESS) == Some((device << 16) | function)
            {
                return match cfgmgr::numa_node(devnode) {
                    Some(node) => (pci, Some(node), format!("{how}; bus match")),
                    None => (
                        pci,
                        None,
                        format!("{how}; bus match, no DEVPKEY_Device_Numa_Node"),
                    ),
                };
            }
        }
        how.push_str("; no display device on that PCI address");
    } else {
        how.push_str("; no PCI address (software adapter?)");
    }
    (pci, None, how)
}

#[cfg(not(target_os = "windows"))]
fn gpu_location(_: (i32, u32)) -> (Option<(u32, u32, u32)>, Option<u16>, String) {
    (None, None, "not Windows".to_owned())
}

#[cfg(target_os = "windows")]
mod cfgmgr {
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
        CM_GETIDLIST_FILTER_CLASS, CM_GETIDLIST_FILTER_PRESENT, CM_Get_DevNode_PropertyW,
        CM_Get_Device_ID_List_SizeW, CM_Get_Device_ID_ListW, CM_LOCATE_DEVNODE_NORMAL,
        CM_Locate_DevNodeW, CR_SUCCESS,
    };
    use windows_sys::Win32::Devices::Properties::{
        DEVPKEY_Device_Address, DEVPKEY_Device_BusNumber, DEVPKEY_Device_Numa_Node,
        DEVPROP_TYPE_UINT32, DEVPROPTYPE,
    };
    use windows_sys::Win32::Foundation::DEVPROPKEY;

    pub(super) const BUS_NUMBER: &DEVPROPKEY = &DEVPKEY_Device_BusNumber;
    pub(super) const ADDRESS: &DEVPROPKEY = &DEVPKEY_Device_Address;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(super) fn locate(instance: &str) -> Option<u32> {
        let id = wide(instance);
        let mut devnode = 0u32;
        // SAFETY: `id` 는 NUL 로 끝나고 호출 동안 살아 있다.
        (unsafe { CM_Locate_DevNodeW(&mut devnode, id.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) }
            == CR_SUCCESS)
            .then_some(devnode)
    }

    pub(super) fn u32_property(devnode: u32, key: &DEVPROPKEY) -> Option<u32> {
        let mut kind: DEVPROPTYPE = 0;
        let mut value = 0u32;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: 출력 버퍼는 4 바이트이고 그 크기를 준다.
        let result = unsafe {
            CM_Get_DevNode_PropertyW(
                devnode,
                key,
                &mut kind,
                &mut value as *mut u32 as *mut u8,
                &mut size,
                0,
            )
        };
        (result == CR_SUCCESS && kind == DEVPROP_TYPE_UINT32 && size == 4).then_some(value)
    }

    pub(super) fn numa_node(devnode: u32) -> Option<u16> {
        u32_property(devnode, &DEVPKEY_Device_Numa_Node)
            .filter(|&node| node != u32::MAX && node < super::wall_numa::MAX_NODES as u32)
            .map(|node| node as u16)
    }

    /// 지금 있는 디스플레이 클래스 장치의 인스턴스 ID.
    pub(super) fn display_device_ids() -> Vec<String> {
        let filter = wide("{4d36e968-e325-11ce-bfc1-08002be10318}");
        let flags = CM_GETIDLIST_FILTER_CLASS | CM_GETIDLIST_FILTER_PRESENT;
        let mut length = 0u32;
        // SAFETY: 길이만 묻는다.
        if unsafe { CM_Get_Device_ID_List_SizeW(&mut length, filter.as_ptr(), flags) } != CR_SUCCESS
            || length == 0
        {
            return Vec::new();
        }
        let mut buffer = vec![0u16; length as usize];
        // SAFETY: 버퍼는 위에서 받은 길이(문자 수)다.
        if unsafe { CM_Get_Device_ID_ListW(filter.as_ptr(), buffer.as_mut_ptr(), length, flags) }
            != CR_SUCCESS
        {
            return Vec::new();
        }
        buffer
            .split(|&unit| unit == 0)
            .filter(|id| !id.is_empty())
            .map(String::from_utf16_lossy)
            .collect()
    }
}
