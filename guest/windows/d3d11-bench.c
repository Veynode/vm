// D3D11 benchmark for Windows guests: renders offscreen frames and times each
// one end to end, printing one JSON line. Each frame ends by copying its
// centre pixel to a staging texture and mapping it, which can't return before
// the frame is rendered (under Neptune/DXMT an event query reports completion
// immediately, so it can't serve as the fence).
//
//   fill   one full-screen triangle with an ALU-heavy pixel shader (GPU bound)
//   draws  DRAWS small triangles per frame, each with a constant-buffer update
//          (API and guest-to-host command transport bound)
//   present the fill frame into a full-screen window's flip swapchain, then
//          Present(SYNC - 1, 0): SYNC 1 = no vsync, 2 = vsync (frames reach the
//          display; Present blocks once the queue is full)
//
// Build (llvm-mingw): aarch64-w64-mingw32-clang -O2 -o d3d11-bench.exe d3d11-bench.c \
//     -ld3d11 -ldxgi -ld3dcompiler -luser32
// Run in the desktop session:
//   d3d11-bench.exe fill|draws [WIDTH HEIGHT FRAMES DRAWS [warp|gpu [SYNC]]]
// For fill and draws, SYNC > 1 waits only every SYNC frames (pipelined, like a game); frame times
// are then per batch divided by SYNC.
#define COBJMACROS
#define INITGUID
#include <d3d11.h>
#include <d3dcompiler.h>
#include <dxgi.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static const char shader[] =
    "cbuffer c : register(b0) { float4 offset; };\n"
    "float4 vs_full(uint id : SV_VertexID) : SV_Position {\n"
    "  float2 p = float2((id << 1) & 2, id & 2);\n"
    "  return float4(p * float2(2, -2) + float2(-1, 1), 0, 1);\n"
    "}\n"
    "float4 vs_small(uint id : SV_VertexID) : SV_Position {\n"
    "  float2 p[3] = { float2(0, 0.02), float2(0.02, -0.02), float2(-0.02, -0.02) };\n"
    "  return float4(p[id] + offset.xy, 0, 1);\n"
    "}\n"
    "float4 ps_heavy(float4 pos : SV_Position) : SV_Target {\n"
    "  float2 uv = pos.xy * 0.001;\n"
    "  float3 c = 0;\n"
    "  [loop] for (int i = 0; i < 64; i++) {\n"
    "    uv = float2(sin(uv.x * 3.1 + uv.y), cos(uv.y * 2.7 - uv.x));\n"
    "    c += float3(uv, uv.x * uv.y) * 0.02;\n"
    "  }\n"
    "  return float4(frac(c), 1);\n"
    "}\n"
    "float4 ps_flat() : SV_Target { return float4(0, 1, 0, 1); }\n";

static int fail(const char *step, HRESULT hr) {
    printf("{\"ok\":false,\"step\":\"%s\",\"hresult\":\"0x%08lx\"}\n", step, (unsigned long)hr);
    return 1;
}

static void *compile(ID3D11Device *device, const char *entry, const char *target) {
    ID3DBlob *code, *errors = NULL;
    if (FAILED(D3DCompile(shader, sizeof shader - 1, NULL, NULL, NULL, entry, target, 0, 0, &code, &errors))) {
        return NULL;
    }
    void *object = NULL;
    const void *bytes = ID3D10Blob_GetBufferPointer(code);
    SIZE_T size = ID3D10Blob_GetBufferSize(code);
    if (target[0] == 'v') {
        ID3D11Device_CreateVertexShader(device, bytes, size, NULL, (ID3D11VertexShader **)&object);
    } else {
        ID3D11Device_CreatePixelShader(device, bytes, size, NULL, (ID3D11PixelShader **)&object);
    }
    return object;
}

static int compare(const void *a, const void *b) {
    double x = *(const double *)a, y = *(const double *)b;
    return (x > y) - (x < y);
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "fill";
    int width = argc > 2 ? atoi(argv[2]) : 1920, height = argc > 3 ? atoi(argv[3]) : 1080;
    int frames = argc > 4 ? atoi(argv[4]) : 200, draws = argc > 5 ? atoi(argv[5]) : 2000;
    int warp = argc > 6 && !strcmp(argv[6], "warp");
    int sync = argc > 7 ? atoi(argv[7]) : 1;
    if (sync < 1) sync = 1;
    int present = !strcmp(mode, "present");
    int fill = present || !strcmp(mode, "fill");

    IDXGIFactory1 *factory;
    IDXGIAdapter1 *adapter = NULL;
    char name[128] = "WARP";
    HRESULT hr = CreateDXGIFactory1(&IID_IDXGIFactory1, (void **)&factory);
    if (FAILED(hr)) return fail("CreateDXGIFactory1", hr);
    if (!warp) {
        DXGI_ADAPTER_DESC1 desc;
        if (FAILED(hr = IDXGIFactory1_EnumAdapters1(factory, 0, &adapter))) return fail("EnumAdapters1", hr);
        IDXGIAdapter1_GetDesc1(adapter, &desc);
        WideCharToMultiByte(CP_UTF8, 0, desc.Description, -1, name, sizeof name, NULL, NULL);
    }
    ID3D11Device *device;
    ID3D11DeviceContext *context;
    D3D_FEATURE_LEVEL level;
    IDXGISwapChain *chain = NULL;
    DXGI_SWAP_CHAIN_DESC swap = {{width, height, {0, 1}, DXGI_FORMAT_R8G8B8A8_UNORM}, {1, 0},
                                 DXGI_USAGE_RENDER_TARGET_OUTPUT, 2, NULL, TRUE, DXGI_SWAP_EFFECT_FLIP_DISCARD, 0};
    if (present) {
        swap.OutputWindow = CreateWindowExA(WS_EX_TOPMOST, "STATIC", "d3d11-bench", WS_POPUP | WS_VISIBLE, 0, 0,
                                            width, height, NULL, NULL, NULL, NULL);
        if (!swap.OutputWindow) return fail("CreateWindow", HRESULT_FROM_WIN32(GetLastError()));
    }
    hr = D3D11CreateDeviceAndSwapChain((IDXGIAdapter *)adapter,
                                       warp ? D3D_DRIVER_TYPE_WARP : D3D_DRIVER_TYPE_UNKNOWN, NULL, 0, NULL, 0,
                                       D3D11_SDK_VERSION, present ? &swap : NULL, present ? &chain : NULL,
                                       &device, &level, &context);
    if (FAILED(hr)) return fail("D3D11CreateDevice", hr);

    ID3D11VertexShader *vs = compile(device, fill ? "vs_full" : "vs_small", "vs_4_0");
    ID3D11PixelShader *ps = compile(device, fill ? "ps_heavy" : "ps_flat", "ps_4_0");
    if (!vs || !ps) return fail("shaders", E_FAIL);
    D3D11_TEXTURE2D_DESC texture = {width, height, 1, 1, DXGI_FORMAT_R8G8B8A8_UNORM, {1, 0},
                                    D3D11_USAGE_DEFAULT, D3D11_BIND_RENDER_TARGET, 0, 0};
    ID3D11Texture2D *target;
    ID3D11RenderTargetView *view;
    if (present) {
        hr = IDXGISwapChain_GetBuffer(chain, 0, &IID_ID3D11Texture2D, (void **)&target);
    } else {
        hr = ID3D11Device_CreateTexture2D(device, &texture, NULL, &target);
    }
    if (FAILED(hr)) return fail("target", hr);
    if (FAILED(hr = ID3D11Device_CreateRenderTargetView(device, (ID3D11Resource *)target, NULL, &view))) {
        return fail("view", hr);
    }
    D3D11_BUFFER_DESC cb = {16, D3D11_USAGE_DYNAMIC, D3D11_BIND_CONSTANT_BUFFER, D3D11_CPU_ACCESS_WRITE, 0, 0};
    ID3D11Buffer *constants;
    if (FAILED(hr = ID3D11Device_CreateBuffer(device, &cb, NULL, &constants))) return fail("constants", hr);
    D3D11_TEXTURE2D_DESC pixel = {1, 1, 1, 1, DXGI_FORMAT_R8G8B8A8_UNORM, {1, 0},
                                  D3D11_USAGE_STAGING, 0, D3D11_CPU_ACCESS_READ, 0};
    ID3D11Texture2D *staging;
    if (FAILED(hr = ID3D11Device_CreateTexture2D(device, &pixel, NULL, &staging))) return fail("staging", hr);
    D3D11_BOX centre = {width / 2, height / 2, 0, width / 2 + 1, height / 2 + 1, 1};
    unsigned int last = 0;

    D3D11_VIEWPORT viewport = {0, 0, (float)width, (float)height, 0, 1};
    const float clear[4] = {0, 0, 0, 1};
    LARGE_INTEGER frequency, start, end, first;
    QueryPerformanceFrequency(&frequency);
    int warmup = 10;
    double *ms = calloc(frames, sizeof *ms);
    for (int frame = -warmup; frame < frames; frame++) {
        if (present || (frame + warmup) % sync == 0) QueryPerformanceCounter(&start);
        if (frame == 0) QueryPerformanceCounter(&first);
        ID3D11DeviceContext_ClearRenderTargetView(context, view, clear);
        ID3D11DeviceContext_OMSetRenderTargets(context, 1, &view, NULL);
        ID3D11DeviceContext_RSSetViewports(context, 1, &viewport);
        ID3D11DeviceContext_IASetPrimitiveTopology(context, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        ID3D11DeviceContext_VSSetShader(context, vs, NULL, 0);
        ID3D11DeviceContext_PSSetShader(context, ps, NULL, 0);
        ID3D11DeviceContext_VSSetConstantBuffers(context, 0, 1, &constants);
        if (fill) {
            ID3D11DeviceContext_Draw(context, 3, 0);
        } else {
            for (int i = 0; i < draws; i++) {
                D3D11_MAPPED_SUBRESOURCE mapped;
                ID3D11DeviceContext_Map(context, (ID3D11Resource *)constants, 0, D3D11_MAP_WRITE_DISCARD, 0, &mapped);
                float *offset = mapped.pData;
                offset[0] = (float)(i % 50) / 25.0f - 0.98f;
                offset[1] = (float)(i / 50 % 50) / 25.0f - 0.98f;
                offset[2] = offset[3] = 0;
                ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)constants, 0);
                ID3D11DeviceContext_Draw(context, 3, 0);
            }
        }
        if (present) {
            MSG message;
            while (PeekMessageA(&message, NULL, 0, 0, PM_REMOVE)) DispatchMessageA(&message);
            if (FAILED(hr = IDXGISwapChain_Present(chain, sync - 1, 0))) return fail("Present", hr);
            QueryPerformanceCounter(&end);
            if (frame >= 0) ms[frame] = (end.QuadPart - start.QuadPart) * 1000.0 / frequency.QuadPart;
            continue;
        }
        if ((frame + warmup + 1) % sync) continue;
        ID3D11DeviceContext_CopySubresourceRegion(context, (ID3D11Resource *)staging, 0, 0, 0, 0,
                                                  (ID3D11Resource *)target, 0, &centre);
        D3D11_MAPPED_SUBRESOURCE read;
        if (FAILED(hr = ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_READ, 0, &read))) {
            return fail("Map", hr);
        }
        last = *(unsigned int *)read.pData;
        ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)staging, 0);
        QueryPerformanceCounter(&end);
        for (int i = frame - sync + 1; i <= frame; i++) {
            if (i >= 0) ms[i] = (end.QuadPart - start.QuadPart) * 1000.0 / frequency.QuadPart / sync;
        }
    }
    double total = (end.QuadPart - first.QuadPart) * 1000.0 / frequency.QuadPart;
    qsort(ms, frames, sizeof *ms, compare);
    printf("{\"ok\":true,\"mode\":\"%s\",\"adapter\":\"%s\",\"feature_level\":\"0x%x\",\"width\":%d,\"height\":%d,"
           "\"frames\":%d,\"sync\":%d,\"draws\":%d,\"fps\":%.1f,\"ms_p50\":%.2f,\"ms_p95\":%.2f,\"ms_max\":%.2f,"
           "\"centre\":\"0x%08x\"}\n",
           mode, name, level, width, height, frames, sync, fill ? 1 : draws, frames * 1000.0 / total,
           ms[frames / 2], ms[frames * 95 / 100], ms[frames - 1], last);
    return 0;
}
