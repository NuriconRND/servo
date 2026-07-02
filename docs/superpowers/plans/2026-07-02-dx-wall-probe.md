# dx_wall_probe Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a standalone C++/Win32/Direct3D11 + GStreamer program that reproduces winit_wall's multi-GPU video-wall test but renders each video directly to a per-tile swapchain backbuffer (no WebRender), to isolate WebRender's compositing as the bottleneck.

**Architecture:** One process. Parse a wall_layout JSON; map each tile's spatial `display` index to the driving GPU via DXGI topology; open one borderless window per tile on its display, each with a D3D11 device + flip swapchain on that tile's adapter; run one GStreamer `playbin`+`appsink` pipeline per grid cell delivering CPU YUV frames; per vsync each tile's render thread pulls the latest frame per cell, uploads YUV, converts to RGB in a pixel shader, draws into the cell's sub-rect, and Presents (dropping late frames).

**Tech Stack:** C++17, Win32, Direct3D 11 (d3d11/dxgi/d3dcompiler), GStreamer C API (gstreamer-1.0/gstapp-1.0/gstvideo-1.0/gobject-2.0/glib-2.0), nlohmann/json (single header), VS2022 / MSBuild.

## Global Constraints

- Platform: Windows x64 only. Toolset: VS2022 (v143), C++17, x64.
- Location: repo-root `tools/dx_wall_probe/` (sibling of `tools/topology_probe`), completely separate from `servo/` — no servo/cargo dependency.
- Build: VS2022 solution `dx_wall_probe.sln` + `dx_wall_probe.vcxproj` + `dx_wall_probe.props`. Buildable via VS (F5) and `msbuild dx_wall_probe.sln /p:Configuration=Release /p:Platform=x64`.
- GStreamer devel via env var `$(GSTREAMER_1_0_ROOT_MSVC_X86_64)`. Runtime needs that install's `bin` on PATH (plugins).
- Decode path: `appsink` delivering CPU YUV frames (NV12 preferred caps, I420 fallback). No hardware zero-copy (matches Servo's CPU-frame path).
- Latest-frame / drop-late: `appsink` with `max-buffers=1, drop=true, sync=false`; render loop uses `gst_app_sink_try_pull_sample(sink, 0)` (non-blocking, returns newest).
- Present: `IDXGISwapChain::Present(1, 0)` when `--vsync on` (default); `Present(0, 0)` when off.
- DX11 only. DX12 is out of scope.
- Verification is MANUAL (build + run + observe), not unit tests. Each task ends with a concrete build+run check.
- wall_layout JSON schema (same as winit_wall): `{ "virtualViewport": {"width":u32,"height":u32}, "tiles":[ {"display":usize (or legacy "monitor"), "rect":[x,y,w,h] (i32)} ], "overlapPx": u32 (optional) }`. Example files: `servo/etc/multigpu/config/wall_layout.*.json`.
- Spatial order rule (winit_wall `spatial_order`): sort displays by desktop-top then desktop-left; index 0 = top-left; tile's `display` indexes into that sorted list.

---

### Task 1: VS2022 project skeleton that builds and runs

**Files:**
- Create: `tools/dx_wall_probe/dx_wall_probe.sln`
- Create: `tools/dx_wall_probe/dx_wall_probe.vcxproj`
- Create: `tools/dx_wall_probe/dx_wall_probe.props`
- Create: `tools/dx_wall_probe/src/main.cpp`
- Create: `tools/dx_wall_probe/.gitignore`

**Interfaces:**
- Produces: a buildable console exe at `tools/dx_wall_probe/x64/Release/dx_wall_probe.exe`.

- [ ] **Step 1: Create the git repo + .gitignore**

```bash
mkdir -p tools/dx_wall_probe/src
cd tools/dx_wall_probe && git init
```
Create `tools/dx_wall_probe/.gitignore`:
```
x64/
.vs/
*.user
```

- [ ] **Step 2: Create `dx_wall_probe.props`** (shared include/lib settings)

`tools/dx_wall_probe/dx_wall_probe.props`:
```xml
<?xml version="1.0" encoding="utf-8"?>
<Project ToolsVersion="4.0" xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <ItemDefinitionGroup>
    <ClCompile>
      <LanguageStandard>stdcpp17</LanguageStandard>
      <AdditionalIncludeDirectories>$(ProjectDir)third_party;$(GSTREAMER_1_0_ROOT_MSVC_X86_64)include\gstreamer-1.0;$(GSTREAMER_1_0_ROOT_MSVC_X86_64)include\glib-2.0;$(GSTREAMER_1_0_ROOT_MSVC_X86_64)lib\glib-2.0\include;%(AdditionalIncludeDirectories)</AdditionalIncludeDirectories>
      <PreprocessorDefinitions>UNICODE;_UNICODE;NOMINMAX;WIN32_LEAN_AND_MEAN;%(PreprocessorDefinitions)</PreprocessorDefinitions>
    </ClCompile>
    <Link>
      <AdditionalLibraryDirectories>$(GSTREAMER_1_0_ROOT_MSVC_X86_64)lib;%(AdditionalLibraryDirectories)</AdditionalLibraryDirectories>
      <AdditionalDependencies>d3d11.lib;dxgi.lib;d3dcompiler.lib;gstreamer-1.0.lib;gstapp-1.0.lib;gstvideo-1.0.lib;gobject-2.0.lib;glib-2.0.lib;%(AdditionalDependencies)</AdditionalDependencies>
    </Link>
  </ItemDefinitionGroup>
</Project>
```
Note: `$(GSTREAMER_1_0_ROOT_MSVC_X86_64)` already ends with a backslash on this machine (e.g. `C:\Program Files\gstreamer\1.0\msvc_x86_64\`).

- [ ] **Step 3: Create `dx_wall_probe.vcxproj`** (x64 Debug/Release, imports the props)

`tools/dx_wall_probe/dx_wall_probe.vcxproj`:
```xml
<?xml version="1.0" encoding="utf-8"?>
<Project DefaultTargets="Build" ToolsVersion="17.0" xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <ItemGroup Label="ProjectConfigurations">
    <ProjectConfiguration Include="Debug|x64"><Configuration>Debug</Configuration><Platform>x64</Platform></ProjectConfiguration>
    <ProjectConfiguration Include="Release|x64"><Configuration>Release</Configuration><Platform>x64</Platform></ProjectConfiguration>
  </ItemGroup>
  <PropertyGroup Label="Globals">
    <VCProjectVersion>17.0</VCProjectVersion>
    <ProjectGuid>{D8B3F3A1-0000-4000-8000-DX11WALLPROB}</ProjectGuid>
    <RootNamespace>dx_wall_probe</RootNamespace>
    <WindowsTargetPlatformVersion>10.0</WindowsTargetPlatformVersion>
  </PropertyGroup>
  <Import Project="$(VCTargetsPath)\Microsoft.Cpp.Default.props" />
  <PropertyGroup Label="Configuration">
    <ConfigurationType>Application</ConfigurationType>
    <PlatformToolset>v143</PlatformToolset>
    <CharacterSet>Unicode</CharacterSet>
    <UseDebugLibraries Condition="'$(Configuration)'=='Debug'">true</UseDebugLibraries>
    <UseDebugLibraries Condition="'$(Configuration)'=='Release'">false</UseDebugLibraries>
    <WholeProgramOptimization Condition="'$(Configuration)'=='Release'">true</WholeProgramOptimization>
  </PropertyGroup>
  <Import Project="$(VCTargetsPath)\Microsoft.Cpp.props" />
  <ImportGroup Label="PropertySheets">
    <Import Project="dx_wall_probe.props" />
  </ImportGroup>
  <ItemDefinitionGroup Condition="'$(Configuration)'=='Release'">
    <ClCompile><Optimization>MaxSpeed</Optimization><RuntimeLibrary>MultiThreadedDLL</RuntimeLibrary></ClCompile>
  </ItemDefinitionGroup>
  <ItemDefinitionGroup Condition="'$(Configuration)'=='Debug'">
    <ClCompile><Optimization>Disabled</Optimization><RuntimeLibrary>MultiThreadedDebugDLL</RuntimeLibrary></ClCompile>
  </ItemDefinitionGroup>
  <ItemGroup>
    <ClCompile Include="src\main.cpp" />
  </ItemGroup>
  <Import Project="$(VCTargetsPath)\Microsoft.Cpp.targets" />
</Project>
```

- [ ] **Step 4: Create `dx_wall_probe.sln`**

`tools/dx_wall_probe/dx_wall_probe.sln`:
```
Microsoft Visual Studio Solution File, Format Version 12.00
# Visual Studio Version 17
Project("{8BC9CEB8-8B4A-11D0-8D11-00A0C91BC942}") = "dx_wall_probe", "dx_wall_probe.vcxproj", "{D8B3F3A1-0000-4000-8000-DX11WALLPROB}"
EndProject
Global
	GlobalSection(SolutionConfigurationPlatforms) = preSolution
		Debug|x64 = Debug|x64
		Release|x64 = Release|x64
	EndGlobalSection
	GlobalSection(ProjectConfigurationPlatforms) = postSolution
		{D8B3F3A1-0000-4000-8000-DX11WALLPROB}.Debug|x64.ActiveCfg = Debug|x64
		{D8B3F3A1-0000-4000-8000-DX11WALLPROB}.Debug|x64.Build.0 = Debug|x64
		{D8B3F3A1-0000-4000-8000-DX11WALLPROB}.Release|x64.ActiveCfg = Release|x64
		{D8B3F3A1-0000-4000-8000-DX11WALLPROB}.Release|x64.Build.0 = Release|x64
	EndGlobalSection
EndGlobal
```

- [ ] **Step 5: Create `src/main.cpp`** (minimal)

```cpp
#include <cstdio>
int main(int argc, char** argv) {
    std::printf("dx_wall_probe: %d args\n", argc);
    return 0;
}
```

- [ ] **Step 6: Build via msbuild and run**

Run (from a *VS2022 x64 Native Tools* prompt, or after importing vcvars — the servo env's vcvars works):
```
msbuild tools\dx_wall_probe\dx_wall_probe.sln /p:Configuration=Release /p:Platform=x64
tools\dx_wall_probe\x64\Release\dx_wall_probe.exe a b
```
Expected: build succeeds (0 errors); run prints `dx_wall_probe: 3 args`.

- [ ] **Step 7: Commit**

```bash
cd tools/dx_wall_probe && git add -A && git commit -m "task1: VS2022 skeleton builds and runs"
```

---

### Task 2: wall_layout JSON parser

**Files:**
- Create: `tools/dx_wall_probe/third_party/json.hpp` (nlohmann/json single header)
- Create: `tools/dx_wall_probe/src/wall_layout.h`
- Create: `tools/dx_wall_probe/src/wall_layout.cpp`
- Modify: `tools/dx_wall_probe/dx_wall_probe.vcxproj` (add wall_layout.cpp)
- Modify: `tools/dx_wall_probe/src/main.cpp` (temporary `--dump-layout`)

**Interfaces:**
- Produces:
```cpp
struct WallTile { int display; int x, y, w, h; };            // rect = [x,y,w,h]
struct WallLayout { unsigned vv_width, vv_height; unsigned overlap_px;
                    std::vector<WallTile> tiles; };
// throws std::runtime_error on invalid input:
WallLayout load_wall_layout(const std::string& path);
```

- [ ] **Step 1: Vendor nlohmann/json single header**

```bash
curl -L -o tools/dx_wall_probe/third_party/json.hpp https://raw.githubusercontent.com/nlohmann/json/v3.11.3/single_include/nlohmann/json.hpp
```
(If offline: copy `json.hpp` v3.11.x into `tools/dx_wall_probe/third_party/` by hand.) Verify the file exists and is ~900KB:
```bash
ls -la tools/dx_wall_probe/third_party/json.hpp
```

- [ ] **Step 2: Create `src/wall_layout.h`**

```cpp
#pragma once
#include <string>
#include <vector>

struct WallTile { int display; int x, y, w, h; };
struct WallLayout {
    unsigned vv_width = 0, vv_height = 0;
    unsigned overlap_px = 0;
    std::vector<WallTile> tiles;
};
// Parses the wall_layout JSON (winit_wall schema). Throws std::runtime_error on error.
WallLayout load_wall_layout(const std::string& path);
```

- [ ] **Step 3: Create `src/wall_layout.cpp`**

```cpp
#include "wall_layout.h"
#include "json.hpp"
#include <fstream>
#include <stdexcept>

using nlohmann::json;

WallLayout load_wall_layout(const std::string& path) {
    std::ifstream f(path);
    if (!f) throw std::runtime_error("cannot open wall_layout: " + path);
    json j; f >> j;

    WallLayout out;
    const auto& vv = j.at("virtualViewport");
    out.vv_width = vv.at("width").get<unsigned>();
    out.vv_height = vv.at("height").get<unsigned>();
    out.overlap_px = j.value("overlapPx", 0u);

    for (const auto& t : j.at("tiles")) {
        WallTile tile{};
        if (t.contains("display")) tile.display = t.at("display").get<int>();
        else if (t.contains("monitor")) tile.display = t.at("monitor").get<int>(); // legacy alias
        else throw std::runtime_error("tile missing 'display'");
        const auto& r = t.at("rect");
        if (r.size() != 4) throw std::runtime_error("tile rect must be [x,y,w,h]");
        tile.x = r[0].get<int>(); tile.y = r[1].get<int>();
        tile.w = r[2].get<int>(); tile.h = r[3].get<int>();
        if (tile.w <= 0 || tile.h <= 0) throw std::runtime_error("tile rect w/h must be positive");
        out.tiles.push_back(tile);
    }
    if (out.tiles.empty()) throw std::runtime_error("tiles must not be empty");
    return out;
}
```

- [ ] **Step 4: Add `wall_layout.cpp` to the vcxproj**

In `dx_wall_probe.vcxproj`, change the `<ItemGroup>` with ClCompile to:
```xml
  <ItemGroup>
    <ClCompile Include="src\main.cpp" />
    <ClCompile Include="src\wall_layout.cpp" />
  </ItemGroup>
```

- [ ] **Step 5: Add a temporary `--dump-layout` to `src/main.cpp`**

```cpp
#include <cstdio>
#include <cstring>
#include <string>
#include "wall_layout.h"

int main(int argc, char** argv) {
    for (int i = 1; i + 1 < argc; ++i) {
        if (std::strcmp(argv[i], "--dump-layout") == 0) {
            WallLayout l = load_wall_layout(argv[i + 1]);
            std::printf("virtualViewport=%ux%u overlapPx=%u tiles=%zu\n",
                        l.vv_width, l.vv_height, l.overlap_px, l.tiles.size());
            for (size_t k = 0; k < l.tiles.size(); ++k) {
                const auto& t = l.tiles[k];
                std::printf("  tile %zu: display=%d rect=[%d,%d %dx%d]\n",
                            k, t.display, t.x, t.y, t.w, t.h);
            }
            return 0;
        }
    }
    std::printf("dx_wall_probe: %d args\n", argc);
    return 0;
}
```

- [ ] **Step 6: Build and run against a real layout**

```
msbuild tools\dx_wall_probe\dx_wall_probe.sln /p:Configuration=Release /p:Platform=x64
tools\dx_wall_probe\x64\Release\dx_wall_probe.exe --dump-layout ..\..\servo\etc\multigpu\config\wall_layout.example_2x2.json
```
Expected: prints `virtualViewport=3840x2160 ... tiles=4` and four tile rects matching the JSON.

- [ ] **Step 7: Commit**

```bash
cd tools/dx_wall_probe && git add -A && git commit -m "task2: wall_layout JSON parser (--dump-layout)"
```

---

### Task 3: DXGI topology → GPU assignment

**Files:**
- Create: `tools/dx_wall_probe/src/dxgi_topology.h`
- Create: `tools/dx_wall_probe/src/dxgi_topology.cpp`
- Modify: `dx_wall_probe.vcxproj` (add dxgi_topology.cpp)
- Modify: `src/main.cpp` (temporary `--dump-topology`)

**Interfaces:**
- Produces:
```cpp
struct DisplayInfo {
    int spatial_index;             // 0 = top-left, sorted by (top, then left)
    int left, top, width, height;  // desktop coordinates
    unsigned adapter_index;        // IDXGIFactory1 EnumAdapters1 index that drives this display
    LUID adapter_luid;
    std::wstring device_name;      // e.g. \\.\DISPLAY1
};
// Enumerate all outputs across all adapters, sorted into spatial order.
std::vector<DisplayInfo> enumerate_display_topology();
// The adapter by EnumAdapters1 index (DisplayInfo::adapter_index), or throws.
Microsoft::WRL::ComPtr<IDXGIAdapter1> adapter_for_index(unsigned adapter_index);
```

- [ ] **Step 1: Create `src/dxgi_topology.h`**

```cpp
#pragma once
#include <dxgi1_2.h>
#include <wrl/client.h>
#include <string>
#include <vector>

struct DisplayInfo {
    int spatial_index = 0;
    int left = 0, top = 0, width = 0, height = 0;
    unsigned adapter_index = 0;
    LUID adapter_luid{};
    std::wstring device_name;
};

std::vector<DisplayInfo> enumerate_display_topology();
Microsoft::WRL::ComPtr<IDXGIAdapter1> adapter_for_index(unsigned adapter_index);
```

- [ ] **Step 2: Create `src/dxgi_topology.cpp`**

```cpp
#include "dxgi_topology.h"
#include <algorithm>
#include <stdexcept>

using Microsoft::WRL::ComPtr;

std::vector<DisplayInfo> enumerate_display_topology() {
    ComPtr<IDXGIFactory1> factory;
    if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&factory))))
        throw std::runtime_error("CreateDXGIFactory1 failed");

    std::vector<DisplayInfo> displays;
    ComPtr<IDXGIAdapter1> adapter;
    for (UINT ai = 0; factory->EnumAdapters1(ai, &adapter) != DXGI_ERROR_NOT_FOUND; ++ai) {
        DXGI_ADAPTER_DESC1 adesc{};
        adapter->GetDesc1(&adesc);
        ComPtr<IDXGIOutput> output;
        for (UINT oi = 0; adapter->EnumOutputs(oi, &output) != DXGI_ERROR_NOT_FOUND; ++oi) {
            DXGI_OUTPUT_DESC odesc{};
            output->GetDesc(&odesc);
            const RECT& r = odesc.DesktopCoordinates;
            DisplayInfo d;
            d.left = r.left; d.top = r.top;
            d.width = r.right - r.left; d.height = r.bottom - r.top;
            d.adapter_index = ai;
            d.adapter_luid = adesc.AdapterLuid;
            d.device_name = odesc.DeviceName;
            displays.push_back(d);
            output.Reset();
        }
        adapter.Reset();
    }
    // spatial order: top first, then left (matches winit_wall spatial_order)
    std::sort(displays.begin(), displays.end(), [](const DisplayInfo& a, const DisplayInfo& b) {
        if (a.top != b.top) return a.top < b.top;
        return a.left < b.left;
    });
    for (int i = 0; i < (int)displays.size(); ++i) displays[i].spatial_index = i;
    return displays;
}

ComPtr<IDXGIAdapter1> adapter_for_index(unsigned adapter_index) {
    ComPtr<IDXGIFactory1> factory;
    if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&factory))))
        throw std::runtime_error("CreateDXGIFactory1 failed");
    ComPtr<IDXGIAdapter1> adapter;
    if (factory->EnumAdapters1(adapter_index, &adapter) == DXGI_ERROR_NOT_FOUND)
        throw std::runtime_error("adapter index out of range");
    return adapter;
}
```

- [ ] **Step 3: Add `dxgi_topology.cpp` to the vcxproj** (add `<ClCompile Include="src\dxgi_topology.cpp" />` to the ItemGroup).

- [ ] **Step 4: Add `--dump-topology` to `src/main.cpp`** (add near the `--dump-layout` handler; add `#include "dxgi_topology.h"`):

```cpp
        if (std::strcmp(argv[i], "--dump-topology") == 0 || std::strcmp(argv[1], "--dump-topology") == 0) {
            auto disp = enumerate_display_topology();
            std::printf("displays=%zu\n", disp.size());
            for (const auto& d : disp)
                std::wprintf(L"  display %d: %s rect[%d,%d %dx%d] adapter %u luid %08x:%08x\n",
                             d.spatial_index, d.device_name.c_str(), d.left, d.top, d.width, d.height,
                             d.adapter_index, (unsigned)d.adapter_luid.HighPart, (unsigned)d.adapter_luid.LowPart);
            return 0;
        }
```
(Place this as its own top-level check: if `argc >= 2 && strcmp(argv[1],"--dump-topology")==0`.)

- [ ] **Step 5: Build and run**

```
msbuild tools\dx_wall_probe\dx_wall_probe.sln /p:Configuration=Release /p:Platform=x64
tools\dx_wall_probe\x64\Release\dx_wall_probe.exe --dump-topology
```
Expected: lists each display with desktop rect, adapter index, LUID — the `display N: ... rect[...] adapter M luid ...` lines should match what winit_wall prints ("Wall display topology ...") on the same machine.

- [ ] **Step 6: Commit**

```bash
cd tools/dx_wall_probe && git add -A && git commit -m "task3: DXGI topology enumeration + spatial order (--dump-topology)"
```

---

### Task 4: Borderless window per tile

**Files:**
- Create: `tools/dx_wall_probe/src/win32_window.h`
- Create: `tools/dx_wall_probe/src/win32_window.cpp`
- Modify: `dx_wall_probe.vcxproj`; `src/main.cpp`

**Interfaces:**
- Produces:
```cpp
class Win32Window {
public:
    Win32Window(const wchar_t* title, int x, int y, int w, int h); // borderless, positioned
    HWND hwnd() const;
    bool closed() const;                 // set true on WM_CLOSE/WM_DESTROY
};
void pump_messages();                    // non-blocking PeekMessage drain for all windows
```

- [ ] **Step 1: Create `src/win32_window.h`**

```cpp
#pragma once
#include <windows.h>

class Win32Window {
public:
    Win32Window(const wchar_t* title, int x, int y, int w, int h);
    ~Win32Window();
    HWND hwnd() const { return hwnd_; }
    bool closed() const { return closed_; }
private:
    HWND hwnd_ = nullptr;
    bool closed_ = false;
    static LRESULT CALLBACK WndProc(HWND, UINT, WPARAM, LPARAM);
};

void pump_messages();
```

- [ ] **Step 2: Create `src/win32_window.cpp`**

```cpp
#include "win32_window.h"

static const wchar_t* kClass = L"dx_wall_probe_window";

LRESULT CALLBACK Win32Window::WndProc(HWND h, UINT msg, WPARAM w, LPARAM l) {
    Win32Window* self = reinterpret_cast<Win32Window*>(GetWindowLongPtrW(h, GWLP_USERDATA));
    switch (msg) {
        case WM_CLOSE: if (self) self->closed_ = true; return 0;
        case WM_DESTROY: if (self) self->closed_ = true; return 0;
        default: return DefWindowProcW(h, msg, w, l);
    }
}

Win32Window::Win32Window(const wchar_t* title, int x, int y, int w, int h) {
    static bool registered = false;
    if (!registered) {
        WNDCLASSEXW wc{ sizeof(wc) };
        wc.lpfnWndProc = &Win32Window::WndProc;
        wc.hInstance = GetModuleHandleW(nullptr);
        wc.hCursor = LoadCursorW(nullptr, IDC_ARROW);
        wc.lpszClassName = kClass;
        RegisterClassExW(&wc);
        registered = true;
    }
    hwnd_ = CreateWindowExW(0, kClass, title, WS_POPUP,
                            x, y, w, h, nullptr, nullptr, GetModuleHandleW(nullptr), nullptr);
    SetWindowLongPtrW(hwnd_, GWLP_USERDATA, reinterpret_cast<LONG_PTR>(this));
    ShowWindow(hwnd_, SW_SHOW);
}

Win32Window::~Win32Window() { if (hwnd_) DestroyWindow(hwnd_); }

void pump_messages() {
    MSG msg;
    while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}
```

- [ ] **Step 3: Add to vcxproj; add a temporary `--test-windows` path in main.cpp** that builds the layout+topology, opens one window per tile at the tile's display origin + tile rect size, and loops `pump_messages()` until all closed:

```cpp
// in main, after parsing args for "--test-windows <layout>":
WallLayout layout = load_wall_layout(layoutPath);
auto disp = enumerate_display_topology();
std::vector<std::unique_ptr<Win32Window>> windows;
for (size_t k = 0; k < layout.tiles.size(); ++k) {
    const auto& t = layout.tiles[k];
    const DisplayInfo& d = disp.at((size_t)t.display); // spatial index
    wchar_t title[64]; swprintf(title, 64, L"dx_wall_probe tile %zu", k);
    windows.push_back(std::make_unique<Win32Window>(title, d.left, d.top, t.w, t.h));
}
for (;;) {
    pump_messages();
    bool all_closed = true;
    for (auto& w : windows) if (!w->closed()) all_closed = false;
    if (all_closed || windows.empty()) break;
    Sleep(8);
}
```
(Add includes `<memory>`, `<vector>`.)

- [ ] **Step 4: Build and run**

```
msbuild tools\dx_wall_probe\dx_wall_probe.sln /p:Configuration=Release /p:Platform=x64
tools\dx_wall_probe\x64\Release\dx_wall_probe.exe --test-windows ..\..\servo\etc\multigpu\config\wall_layout.example_1x1.json
```
Expected: a borderless window appears at the tile's display origin, sized to the tile rect. Closing it (Alt+F4) exits.

- [ ] **Step 5: Commit**

```bash
cd tools/dx_wall_probe && git add -A && git commit -m "task4: borderless per-tile windows"
```

---

### Task 5: D3D11 device + flip swapchain per tile; clear + Present at vsync

**Files:**
- Create: `tools/dx_wall_probe/src/dx11_tile_renderer.h`
- Create: `tools/dx_wall_probe/src/dx11_tile_renderer.cpp`
- Modify: `dx_wall_probe.vcxproj`; `src/main.cpp`

**Interfaces:**
- Produces:
```cpp
class Dx11TileRenderer {
public:
    Dx11TileRenderer(IDXGIAdapter1* adapter, HWND hwnd, int width, int height);
    void begin_frame(float r, float g, float b);  // clear backbuffer
    void present(bool vsync);                      // Present(vsync?1:0, 0)
    ID3D11Device* device() const;
    ID3D11DeviceContext* context() const;
};
```

- [ ] **Step 1: Create `src/dx11_tile_renderer.h`**

```cpp
#pragma once
#include <d3d11.h>
#include <dxgi1_2.h>
#include <wrl/client.h>

class Dx11TileRenderer {
public:
    Dx11TileRenderer(IDXGIAdapter1* adapter, HWND hwnd, int width, int height);
    void begin_frame(float r, float g, float b);
    void present(bool vsync);
    ID3D11Device* device() const { return device_.Get(); }
    ID3D11DeviceContext* context() const { return context_.Get(); }
    int width() const { return width_; }
    int height() const { return height_; }
private:
    int width_, height_;
    Microsoft::WRL::ComPtr<ID3D11Device> device_;
    Microsoft::WRL::ComPtr<ID3D11DeviceContext> context_;
    Microsoft::WRL::ComPtr<IDXGISwapChain1> swapchain_;
    Microsoft::WRL::ComPtr<ID3D11RenderTargetView> rtv_;
};
```

- [ ] **Step 2: Create `src/dx11_tile_renderer.cpp`** (init + clear + present only for now)

```cpp
#include "dx11_tile_renderer.h"
#include <stdexcept>
using Microsoft::WRL::ComPtr;

static void check(HRESULT hr, const char* what) {
    if (FAILED(hr)) throw std::runtime_error(what);
}

Dx11TileRenderer::Dx11TileRenderer(IDXGIAdapter1* adapter, HWND hwnd, int width, int height)
    : width_(width), height_(height) {
    UINT flags = 0;
    D3D_FEATURE_LEVEL fl = D3D_FEATURE_LEVEL_11_0;
    check(D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr, flags,
                            &fl, 1, D3D11_SDK_VERSION, &device_, nullptr, &context_),
          "D3D11CreateDevice failed");

    ComPtr<IDXGIFactory2> factory;
    check(CreateDXGIFactory1(IID_PPV_ARGS(&factory)), "CreateDXGIFactory1 failed");
    DXGI_SWAP_CHAIN_DESC1 scd{};
    scd.Width = width; scd.Height = height;
    scd.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
    scd.SampleDesc.Count = 1;
    scd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    scd.BufferCount = 2;
    scd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
    check(factory->CreateSwapChainForHwnd(device_.Get(), hwnd, &scd, nullptr, nullptr, &swapchain_),
          "CreateSwapChainForHwnd failed");

    ComPtr<ID3D11Texture2D> backbuffer;
    check(swapchain_->GetBuffer(0, IID_PPV_ARGS(&backbuffer)), "GetBuffer failed");
    check(device_->CreateRenderTargetView(backbuffer.Get(), nullptr, &rtv_), "CreateRTV failed");
}

void Dx11TileRenderer::begin_frame(float r, float g, float b) {
    context_->OMSetRenderTargets(1, rtv_.GetAddressOf(), nullptr);
    D3D11_VIEWPORT vp{ 0, 0, (float)width_, (float)height_, 0, 1 };
    context_->RSSetViewports(1, &vp);
    const float clear[4] = { r, g, b, 1.0f };
    context_->ClearRenderTargetView(rtv_.Get(), clear);
}

void Dx11TileRenderer::present(bool vsync) {
    swapchain_->Present(vsync ? 1 : 0, 0);
}
```

- [ ] **Step 3: Add to vcxproj; add `--test-clear` path in main.cpp** that creates a window + renderer per tile (using `adapter_for_index(disp[tile.display].adapter_index)`) and loops: `pump_messages()`, each renderer `begin_frame(0,0,0.3f)` + `present(true)`, until closed. Log per-tile present FPS once/sec (count presents, print with GetTickCount64).

- [ ] **Step 4: Build and run**

```
msbuild tools\dx_wall_probe\dx_wall_probe.sln /p:Configuration=Release /p:Platform=x64
tools\dx_wall_probe\x64\Release\dx_wall_probe.exe --test-clear ..\..\servo\etc\multigpu\config\wall_layout.example_1x1.json
```
Expected: window shows a solid dark-blue fill; stderr logs `tile 0 present_fps≈60`.

- [ ] **Step 5: Commit**

```bash
cd tools/dx_wall_probe && git add -A && git commit -m "task5: D3D11 device+flip swapchain per tile, clear+present@vsync"
```

---

### Task 6: GStreamer video cell (playbin+appsink, latest-frame)

**Files:**
- Create: `tools/dx_wall_probe/src/video_cell.h`
- Create: `tools/dx_wall_probe/src/video_cell.cpp`
- Modify: `dx_wall_probe.vcxproj`; `src/main.cpp` (gst_init; `--test-decode`)

**Interfaces:**
- Produces:
```cpp
enum class YuvFormat { NV12, I420 };
struct YuvFrame {                 // valid only while `sample` is held
    GstSample* sample;            // owns the buffer; must gst_sample_unref when done
    YuvFormat format;
    int width, height;
    const uint8_t* plane[3];      // NV12: Y, UV; I420: Y, U, V
    int stride[3];
};
class VideoCell {
public:
    VideoCell(const std::string& uri);   // builds & starts playbin+appsink (loop)
    ~VideoCell();
    bool try_get_latest(YuvFrame& out);   // non-blocking; maps newest sample. false if none.
    void release(YuvFrame& f);            // unmap+unref
};
```

- [ ] **Step 1: Create `src/video_cell.h`** (as above, plus includes `<gst/gst.h>`, `<gst/app/gstappsink.h>`, `<gst/video/video.h>`, `<string>`, `<cstdint>`; store `GstElement* playbin_`, `GstAppSink* appsink_`).

- [ ] **Step 2: Create `src/video_cell.cpp`**

```cpp
#include "video_cell.h"
#include <stdexcept>

VideoCell::VideoCell(const std::string& uri) {
    playbin_ = gst_element_factory_make("playbin", nullptr);
    if (!playbin_) throw std::runtime_error("playbin creation failed");

    GstElement* sink = gst_element_factory_make("appsink", nullptr);
    appsink_ = GST_APP_SINK(sink);
    // Prefer NV12, allow I420 (system memory).
    GstCaps* caps = gst_caps_from_string("video/x-raw,format={NV12,I420}");
    gst_app_sink_set_caps(appsink_, caps);
    gst_caps_unref(caps);
    gst_app_sink_set_max_buffers(appsink_, 1);
    gst_app_sink_set_drop(appsink_, TRUE);
    g_object_set(sink, "sync", FALSE, "enable-last-sample", FALSE, nullptr);

    g_object_set(playbin_, "uri", uri.c_str(), "video-sink", sink, nullptr);
    // mute audio: set a fakesink audio-sink
    GstElement* afake = gst_element_factory_make("fakesink", nullptr);
    g_object_set(afake, "sync", FALSE, nullptr);
    g_object_set(playbin_, "audio-sink", afake, nullptr);

    if (gst_element_set_state(playbin_, GST_STATE_PLAYING) == GST_STATE_CHANGE_FAILURE)
        throw std::runtime_error("failed to set playbin PLAYING");
}

VideoCell::~VideoCell() {
    if (playbin_) { gst_element_set_state(playbin_, GST_STATE_NULL); gst_object_unref(playbin_); }
}

bool VideoCell::try_get_latest(YuvFrame& out) {
    // Loop at EOS: check bus for EOS and seek to 0.
    GstBus* bus = gst_element_get_bus(playbin_);
    if (GstMessage* m = gst_bus_pop_filtered(bus, GST_MESSAGE_EOS)) {
        gst_element_seek_simple(playbin_, GST_FORMAT_TIME,
            (GstSeekFlags)(GST_SEEK_FLAG_FLUSH | GST_SEEK_FLAG_KEY_UNIT), 0);
        gst_message_unref(m);
    }
    gst_object_unref(bus);

    GstSample* s = gst_app_sink_try_pull_sample(appsink_, 0);
    if (!s) return false;
    GstCaps* caps = gst_sample_get_caps(s);
    GstVideoInfo info;
    if (!gst_video_info_from_caps(&info, caps)) { gst_sample_unref(s); return false; }

    GstBuffer* buf = gst_sample_get_buffer(s);
    static thread_local GstVideoFrame vframe; // one per calling (render) thread
    if (!gst_video_frame_map(&vframe, &info, buf, GST_MAP_READ)) { gst_sample_unref(s); return false; }

    out.sample = s;
    out.width = GST_VIDEO_INFO_WIDTH(&info);
    out.height = GST_VIDEO_INFO_HEIGHT(&info);
    GstVideoFormat fmt = GST_VIDEO_INFO_FORMAT(&info);
    out.format = (fmt == GST_VIDEO_FORMAT_NV12) ? YuvFormat::NV12 : YuvFormat::I420;
    int nplanes = (out.format == YuvFormat::NV12) ? 2 : 3;
    for (int p = 0; p < nplanes; ++p) {
        out.plane[p] = (const uint8_t*)GST_VIDEO_FRAME_PLANE_DATA(&vframe, p);
        out.stride[p] = GST_VIDEO_FRAME_PLANE_STRIDE(&vframe, p);
    }
    // stash the map in the sample's mini-object via a raw copy: keep vframe in a member instead.
    // (Implementation detail: store `GstVideoFrame` in a per-cell/thread member; see release().)
    // For simplicity we memcpy the map handle into a per-frame holder:
    out.map_handle = new GstVideoFrame(vframe);
    return true;
}

void VideoCell::release(YuvFrame& f) {
    if (f.map_handle) { gst_video_frame_unmap(f.map_handle); delete f.map_handle; f.map_handle = nullptr; }
    if (f.sample) { gst_sample_unref(f.sample); f.sample = nullptr; }
}
```
Note: add `GstVideoFrame* map_handle = nullptr;` to the `YuvFrame` struct in the header (replaces the fragile thread_local). Remove the `static thread_local` line and instead map directly into a heap `GstVideoFrame`:
```cpp
    GstVideoFrame* vf = new GstVideoFrame();
    if (!gst_video_frame_map(vf, &info, buf, GST_MAP_READ)) { delete vf; gst_sample_unref(s); return false; }
    ... out.plane[p] = GST_VIDEO_FRAME_PLANE_DATA(vf,p); out.stride[p]=GST_VIDEO_FRAME_PLANE_STRIDE(vf,p);
    out.map_handle = vf;
```

- [ ] **Step 3: Add `gst_init` to main.cpp** (call `gst_init(&argc, &argv);` at top of `main`). Add `--test-decode <video>` that creates one `VideoCell` from the file path (converted to `file:///` URI), then loops ~5s pulling latest frames and logging `format/WxH` and a frames/sec counter.

- [ ] **Step 4: Add to vcxproj. Build and run**

```
msbuild tools\dx_wall_probe\dx_wall_probe.sln /p:Configuration=Release /p:Platform=x64
set PATH=%GSTREAMER_1_0_ROOT_MSVC_X86_64%bin;%PATH%
tools\dx_wall_probe\x64\Release\dx_wall_probe.exe --test-decode ..\..\servo\tests\Wildlife_FHD30fps_counter_10Mbitrate.mp4
```
Expected: logs e.g. `frame NV12 1920x1080` and `~30 fps` pulled. (If plugins fail to load, ensure the GStreamer `bin` is on PATH.)

- [ ] **Step 5: Commit**

```bash
cd tools/dx_wall_probe && git add -A && git commit -m "task6: GStreamer playbin+appsink video cell, latest-frame pull + loop"
```

---

### Task 7: YUV upload + shader + draw one full-window video

**Files:**
- Modify: `tools/dx_wall_probe/src/dx11_tile_renderer.{h,cpp}` (add YUV draw)
- Modify: `src/main.cpp` (`--test-one-video`)

**Interfaces:**
- Produces (added to Dx11TileRenderer):
```cpp
// Upload a YUV frame and draw it into the viewport rect (px) of the backbuffer.
void draw_yuv(const YuvFrame& f, int vx, int vy, int vw, int vh);
```
- Consumes: `YuvFrame` from Task 6.

- [ ] **Step 1: Add HLSL (runtime D3DCompile) + pipeline state to Dx11TileRenderer.**
Add members: `ComPtr<ID3D11VertexShader> vs_; ComPtr<ID3D11PixelShader> ps_; ComPtr<ID3D11SamplerState> sampler_; ComPtr<ID3D11Buffer> cb_;` and per-draw dynamic textures/SRVs for Y and UV/U/V (created/resized on first frame or when size changes). Shader source (embedded string), fullscreen triangle by SV_VertexID, converts NV12 or I420 (branch via a constant `is_nv12`):

```hlsl
cbuffer CB : register(b0) { int is_nv12; int3 _pad; };
Texture2D texY : register(t0);
Texture2D texUV : register(t1); // NV12: RG8 ; I420: this is U (R8)
Texture2D texV : register(t2);  // I420 only: V (R8)
SamplerState smp : register(s0);
struct VSOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VSOut VSMain(uint id : SV_VertexID) {
    VSOut o; float2 t = float2((id << 1) & 2, id & 2);
    o.uv = t; o.pos = float4(t * float2(2,-2) + float2(-1,1), 0, 1); return o;
}
float4 PSMain(VSOut i) : SV_Target {
    float y = texY.Sample(smp, i.uv).r;
    float u, v;
    if (is_nv12) { float2 uv = texUV.Sample(smp, i.uv).rg; u = uv.x; v = uv.y; }
    else { u = texUV.Sample(smp, i.uv).r; v = texV.Sample(smp, i.uv).r; }
    // BT.709 limited-range
    y = (y - 16.0/255.0) * (255.0/219.0);
    u = (u - 128.0/255.0) * (255.0/224.0);
    v = (v - 128.0/255.0) * (255.0/224.0);
    float r = y + 1.5748*v;
    float g = y - 0.1873*u - 0.4681*v;
    float b = y + 1.8556*u;
    return float4(saturate(float3(r,g,b)), 1.0);
}
```
Compile both with `D3DCompile(src, ..., "VSMain","vs_5_0",...)` / `"PSMain","ps_5_0"` in the ctor; create a linear clamp sampler and a constant buffer holding `is_nv12`.

- [ ] **Step 2: Implement `draw_yuv`** — create/resize dynamic textures (Y: R8 width×height; NV12 UV: R8G8 (width/2)×(height/2); I420 U,V: R8 (width/2)×(height/2)); `Map`+row-copy each plane honoring `stride`; create SRVs; set viewport to (vx,vy,vw,vh); bind VS/PS/sampler/SRVs/CB (`is_nv12`); `IASetPrimitiveTopology(TRIANGLELIST)`; `Draw(3,0)`.

- [ ] **Step 3: Add `--test-one-video <layout> <video>`** to main: one tile → window + renderer; one VideoCell; loop: pump_messages; `begin_frame(0,0,0)`; `if (cell.try_get_latest(f)) { renderer.draw_yuv(f, 0,0, W,H); cell.release(f); }`; `present(true)`. Log present_fps.

- [ ] **Step 4: Build and run**

```
msbuild tools\dx_wall_probe\dx_wall_probe.sln /p:Configuration=Release /p:Platform=x64
set PATH=%GSTREAMER_1_0_ROOT_MSVC_X86_64%bin;%PATH%
tools\dx_wall_probe\x64\Release\dx_wall_probe.exe --test-one-video ..\..\servo\etc\multigpu\config\wall_layout.example_1x1.json ..\..\servo\tests\Wildlife_FHD30fps_counter_10Mbitrate.mp4
```
Expected: the video plays full-window with correct colors at ~vsync; present_fps≈60.

- [ ] **Step 5: Commit**

```bash
cd tools/dx_wall_probe && git add -A && git commit -m "task7: YUV upload + shader + draw one full-window video"
```

---

### Task 8: Full grid, multi-tile multi-GPU, threads, present-mode, perf, final CLI

**Files:**
- Create: `tools/dx_wall_probe/src/app.h`, `src/app.cpp` (orchestration: tiles, cells, threads)
- Modify: `src/main.cpp` (final CLI, replace all `--test-*`)
- Modify: `dx_wall_probe.vcxproj`

**Interfaces:**
- Consumes: WallLayout, DisplayInfo, Dx11TileRenderer, VideoCell.
- Produces: the shipping program.

- [ ] **Step 1: Final CLI parse in main.cpp** — `--wall-layout <p>` (required), `--cols N` (default 1), `--rows N` (default 1), `--video <p>` (required), `--present-mode per-tile|single` (default per-tile), `--vsync on|off` (default on), `--wall-tile-index N` (optional; if set, render only that tile). Convert `--video` path to `file:///` URI (replace `\` with `/`, prefix `file:///`). Call `gst_init`. Then `run_app(...)`.

- [ ] **Step 2: `app.cpp` — build tiles.** For each tile (or just the `--wall-tile-index` one): resolve `DisplayInfo d = disp[tile.display]`; create `Win32Window` at `(d.left,d.top)` size `(tile.w,tile.h)`; create `Dx11TileRenderer(adapter_for_index(d.adapter_index), hwnd, tile.w, tile.h)`; create `cols*rows` `VideoCell`s (all from the same URI). Compute each cell's sub-rect in the tile: `cw=tile.w/cols, ch=tile.h/rows; cell(r,c) viewport = (c*cw, r*ch, cw, ch)`.

- [ ] **Step 3: Render function per tile** (`render_tile(TileState&, bool vsync)`):
```cpp
renderer.begin_frame(0,0,0);
for (int r = 0; r < rows; ++r)
  for (int c = 0; c < cols; ++c) {
    YuvFrame f;
    if (cells[r*cols+c]->try_get_latest(f)) {
        renderer.draw_yuv(f, c*cw, r*ch, cw, ch);
        cells[r*cols+c]->release(f);
    }
  }
renderer.present(vsync);
// perf: count, once/sec log "tile K present_fps=.. avg_present_ms=.. cells_updated=.."
```

- [ ] **Step 4: Threading.** `per-tile` mode: spawn one `std::thread` per tile running its render loop (`while(!closed) render_tile(...)`); the D3D11 device/context and swapchain for a tile are used only by its own thread (D3D11 immediate context is not thread-safe, so one thread per device is required). `single` mode: the main thread loops over all tiles calling `render_tile` sequentially. In BOTH modes the **main thread** runs `pump_messages()` in a loop (Win32 messages must be pumped on the thread that created the windows — so create all windows on the main thread, and in per-tile mode the render threads only touch D3D/GStreamer, never the HWND message queue). Stop when all windows `closed()`.

- [ ] **Step 5: Perf logging** — per tile: accumulate present count + present-time (`QueryPerformanceCounter` around `present()`), and count how many cells returned a fresh frame; every ~1s `fprintf(stderr, "tile %d present_fps=%.1f avg_present_ms=%.2f cells_updated_per_s=%.0f\n", ...)`.

- [ ] **Step 6: Build and run the real test**

```
msbuild tools\dx_wall_probe\dx_wall_probe.sln /p:Configuration=Release /p:Platform=x64
set PATH=%GSTREAMER_1_0_ROOT_MSVC_X86_64%bin;%PATH%
tools\dx_wall_probe\x64\Release\dx_wall_probe.exe --wall-layout ..\..\servo\etc\multigpu\config\wall_layout.example_1x1.json --cols 6 --rows 6 --video ..\..\servo\tests\Wildlife_FHD30fps_counter_10Mbitrate.mp4
```
Expected: a 6×6 grid of the video plays in the tile window; stderr logs `tile 0 present_fps≈60`. Then test a multi-tile layout on the real wall and compare present_fps to winit_wall for the same layout/video count.

- [ ] **Step 7: Commit**

```bash
cd tools/dx_wall_probe && git add -A && git commit -m "task8: full grid, multi-tile multi-GPU, per-tile threads, present-mode, perf"
```

---

## Notes for the implementer

- The `--test-*` scaffolding from Tasks 2–7 can be deleted in Task 1-of-8 cleanup or left as hidden debug modes; prefer deleting them in Task 8 Step 1 so `main.cpp` only has the final CLI.
- D3D11 immediate context is single-threaded per device: never touch a tile's device/context/swapchain from another tile's thread.
- Win32: create all windows on the main thread and pump messages there; render threads must not call into the HWND message queue.
- If GStreamer plugins fail to load at runtime ("no element playbin"), the GStreamer `bin` dir is not on PATH — prepend `%GSTREAMER_1_0_ROOT_MSVC_X86_64%bin`.
- Colors off? The shader assumes BT.709 limited range; if a clip is BT.601, read `GST_VIDEO_INFO` colorimetry and switch coefficients (follow-up, not required for the perf experiment).
