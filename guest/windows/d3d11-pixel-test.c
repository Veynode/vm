// D3D11 pixel test for Windows guests: renders a red triangle over a known
// clear color on the hardware adapter, reads the pixels back and checks them.
// Refuses WARP and Microsoft Basic Render, so a pass means the guest's GPU
// driver rendered. Prints one JSON line and writes the frame as a BMP.
//
// Build (llvm-mingw, https://github.com/mstorsjo/llvm-mingw):
//   aarch64-w64-mingw32-clang -O2 -o d3d11-pixel-test.exe d3d11-pixel-test.c \
//       -ld3d11 -ldxgi -ld3dcompiler
// Run in the logged-in desktop session: d3d11-pixel-test.exe [frame.bmp]
#define COBJMACROS
#define INITGUID
#include <d3d11.h>
#include <d3dcompiler.h>
#include <dxgi.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define SIZE 256

static const char shader[] =
    "float4 vs(uint id : SV_VertexID) : SV_Position {\n"
    "  float2 p[3] = { float2(0, 0.8), float2(0.8, -0.8), float2(-0.8, -0.8) };\n"
    "  return float4(p[id], 0, 1);\n"
    "}\n"
    "float4 ps() : SV_Target { return float4(1, 0, 0, 1); }\n";

static int fail(const char *step, HRESULT hr) {
    printf("{\"pass\":false,\"step\":\"%s\",\"hresult\":\"0x%08lx\"}\n", step, (unsigned long)hr);
    return 1;
}

static void write_bmp(const char *path, const unsigned char *rgba, UINT pitch) {
    FILE *file = fopen(path, "wb");
    if (!file) {
        return;
    }
    unsigned int image = SIZE * SIZE * 4, header[13] = {0};
    unsigned short magic = 0x4d42;
    header[0] = 54 + image;
    header[2] = 54;
    header[3] = 40;
    header[4] = SIZE;
    header[5] = SIZE;
    header[6] = 1 | (32 << 16);
    header[8] = image;
    fwrite(&magic, 2, 1, file);
    fwrite(header, 4, 13, file);
    for (int y = SIZE - 1; y >= 0; y--) {
        for (int x = 0; x < SIZE; x++) {
            const unsigned char *p = rgba + y * pitch + x * 4;
            unsigned char bgra[4] = {p[2], p[1], p[0], p[3]};
            fwrite(bgra, 4, 1, file);
        }
    }
    fclose(file);
}

static int close_to(const unsigned char *p, int r, int g, int b) {
    return abs(p[0] - r) <= 2 && abs(p[1] - g) <= 2 && abs(p[2] - b) <= 2 && p[3] == 255;
}

int main(int argc, char **argv) {
    IDXGIFactory1 *factory;
    IDXGIAdapter1 *adapter;
    DXGI_ADAPTER_DESC1 desc;
    HRESULT hr = CreateDXGIFactory1(&IID_IDXGIFactory1, (void **)&factory);
    if (FAILED(hr)) return fail("CreateDXGIFactory1", hr);
    hr = IDXGIFactory1_EnumAdapters1(factory, 0, &adapter);
    if (FAILED(hr)) return fail("EnumAdapters1", hr);
    IDXGIAdapter1_GetDesc1(adapter, &desc);
    char name[128];
    WideCharToMultiByte(CP_UTF8, 0, desc.Description, -1, name, sizeof name, NULL, NULL);
    if ((desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE) || strstr(name, "Basic Render")) {
        printf("{\"pass\":false,\"step\":\"adapter\",\"adapter\":\"%s\"}\n", name);
        return 1;
    }

    ID3D11Device *device;
    ID3D11DeviceContext *context;
    D3D_FEATURE_LEVEL level;
    hr = D3D11CreateDevice((IDXGIAdapter *)adapter, D3D_DRIVER_TYPE_UNKNOWN, NULL, 0, NULL, 0,
                           D3D11_SDK_VERSION, &device, &level, &context);
    if (FAILED(hr)) return fail("D3D11CreateDevice", hr);

    ID3DBlob *vs_code, *ps_code, *errors = NULL;
    hr = D3DCompile(shader, sizeof shader - 1, NULL, NULL, NULL, "vs", "vs_4_0", 0, 0, &vs_code, &errors);
    if (FAILED(hr)) return fail("compile vs", hr);
    hr = D3DCompile(shader, sizeof shader - 1, NULL, NULL, NULL, "ps", "ps_4_0", 0, 0, &ps_code, &errors);
    if (FAILED(hr)) return fail("compile ps", hr);
    ID3D11VertexShader *vs;
    ID3D11PixelShader *ps;
    hr = ID3D11Device_CreateVertexShader(device, ID3D10Blob_GetBufferPointer(vs_code),
                                         ID3D10Blob_GetBufferSize(vs_code), NULL, &vs);
    if (FAILED(hr)) return fail("CreateVertexShader", hr);
    hr = ID3D11Device_CreatePixelShader(device, ID3D10Blob_GetBufferPointer(ps_code),
                                        ID3D10Blob_GetBufferSize(ps_code), NULL, &ps);
    if (FAILED(hr)) return fail("CreatePixelShader", hr);

    D3D11_TEXTURE2D_DESC texture = {SIZE, SIZE, 1, 1, DXGI_FORMAT_R8G8B8A8_UNORM, {1, 0},
                                    D3D11_USAGE_DEFAULT, D3D11_BIND_RENDER_TARGET, 0, 0};
    ID3D11Texture2D *target, *staging;
    hr = ID3D11Device_CreateTexture2D(device, &texture, NULL, &target);
    if (FAILED(hr)) return fail("CreateTexture2D target", hr);
    texture.Usage = D3D11_USAGE_STAGING;
    texture.BindFlags = 0;
    texture.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    hr = ID3D11Device_CreateTexture2D(device, &texture, NULL, &staging);
    if (FAILED(hr)) return fail("CreateTexture2D staging", hr);
    ID3D11RenderTargetView *view;
    hr = ID3D11Device_CreateRenderTargetView(device, (ID3D11Resource *)target, NULL, &view);
    if (FAILED(hr)) return fail("CreateRenderTargetView", hr);

    const float clear[4] = {0.1f, 0.2f, 0.3f, 1.0f};
    D3D11_VIEWPORT viewport = {0, 0, SIZE, SIZE, 0, 1};
    ID3D11DeviceContext_ClearRenderTargetView(context, view, clear);
    ID3D11DeviceContext_OMSetRenderTargets(context, 1, &view, NULL);
    ID3D11DeviceContext_RSSetViewports(context, 1, &viewport);
    ID3D11DeviceContext_IASetPrimitiveTopology(context, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    ID3D11DeviceContext_VSSetShader(context, vs, NULL, 0);
    ID3D11DeviceContext_PSSetShader(context, ps, NULL, 0);
    ID3D11DeviceContext_Draw(context, 3, 0);
    ID3D11DeviceContext_CopyResource(context, (ID3D11Resource *)staging, (ID3D11Resource *)target);

    D3D11_MAPPED_SUBRESOURCE mapped;
    hr = ID3D11DeviceContext_Map(context, (ID3D11Resource *)staging, 0, D3D11_MAP_READ, 0, &mapped);
    if (FAILED(hr)) return fail("Map", hr);
    const unsigned char *pixels = mapped.pData;
    const unsigned char *center = pixels + (SIZE / 2) * mapped.RowPitch + (SIZE / 2) * 4;
    const unsigned char *corner = pixels + 2 * mapped.RowPitch + 2 * 4;
    // 0.1/0.2/0.3 in UNORM8.
    int pass = close_to(center, 255, 0, 0) && close_to(corner, 26, 51, 77);
    if (argc > 1) write_bmp(argv[1], pixels, mapped.RowPitch);
    printf("{\"pass\":%s,\"adapter\":\"%s\",\"vendor\":\"0x%04x\",\"device\":\"0x%04x\","
           "\"feature_level\":\"0x%x\",\"center\":[%d,%d,%d,%d],\"corner\":[%d,%d,%d,%d]}\n",
           pass ? "true" : "false", name, desc.VendorId, desc.DeviceId, level, center[0], center[1],
           center[2], center[3], corner[0], corner[1], corner[2], corner[3]);
    ID3D11DeviceContext_Unmap(context, (ID3D11Resource *)staging, 0);
    return pass ? 0 : 1;
}
