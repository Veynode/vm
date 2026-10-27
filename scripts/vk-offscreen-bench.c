// Headless Vulkan benchmark that builds the same way on the macOS host
// (MoltenVK, no loader) and in a Linux guest (Venus through libvulkan), so a
// guest result can be read as a fraction of the host. It renders offscreen
// and checks the result, which separates GPU-path cost from presentation.
//
//   Linux: cc -O2 -o vk-offscreen-bench vk-offscreen-bench.c -ldl -lxcb
//   macOS: cc -O2 -I<dir with vulkan/vulkan.h> -o vk-offscreen-bench vk-offscreen-bench.c
//   vk-offscreen-bench [-l LIBRARY] [-s light|heavy] [-m sync|pipe|copy|x11]
//                      [-p immediate|mailbox|fifo] [-w WIDTH] [-h HEIGHT] [-t SECONDS]
//
// light: one small triangle per frame (per-frame overhead, round trips).
// heavy: 16 additive full-screen layers with an ALU loop (GPU throughput).
// sync waits for each frame; pipe keeps three in flight; copy also copies each
// frame to a host-visible buffer and reads it, like a CPU-side presentation;
// x11 (Linux) presents each frame to an X11 window through a swapchain
// (present mode -p, default immediate), so x11 against pipe is the cost of
// presentation.
// Prints "VK_BENCH ... fps=N" and exits 0 when the probed pixels are correct.
#define VK_NO_PROTOTYPES
#ifdef __linux__
#define VK_USE_PLATFORM_XCB_KHR
#include <xcb/xcb.h>
#endif
#include <vulkan/vulkan.h>
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "vk-offscreen-bench.spv.h"

#define FN(name) static PFN_##name name;
#define FUNCTIONS(X)                                                                           \
    X(vkEnumeratePhysicalDevices) X(vkGetPhysicalDeviceProperties)                            \
    X(vkGetPhysicalDeviceQueueFamilyProperties) X(vkGetPhysicalDeviceMemoryProperties)        \
    X(vkCreateDevice) X(vkGetDeviceQueue) X(vkCreateImage) X(vkGetImageMemoryRequirements)    \
    X(vkAllocateMemory) X(vkBindImageMemory) X(vkCreateImageView) X(vkCreateRenderPass)       \
    X(vkCreateFramebuffer) X(vkCreateShaderModule) X(vkCreatePipelineLayout)                  \
    X(vkCreateGraphicsPipelines) X(vkCreateCommandPool) X(vkAllocateCommandBuffers)           \
    X(vkBeginCommandBuffer) X(vkEndCommandBuffer) X(vkCmdBeginRenderPass)                     \
    X(vkCmdEndRenderPass) X(vkCmdBindPipeline) X(vkCmdPushConstants) X(vkCmdDraw)             \
    X(vkCmdCopyImageToBuffer) X(vkCmdPipelineBarrier) X(vkCreateFence) X(vkWaitForFences)     \
    X(vkResetFences) X(vkQueueSubmit) X(vkQueueWaitIdle) X(vkCreateBuffer)                    \
    X(vkGetBufferMemoryRequirements) X(vkBindBufferMemory) X(vkMapMemory)                     \
    X(vkInvalidateMappedMemoryRanges) X(vkDeviceWaitIdle)
FUNCTIONS(FN)
#ifdef __linux__
#define PRESENT_FUNCTIONS(X)                                                                   \
    X(vkCreateXcbSurfaceKHR) X(vkGetPhysicalDeviceSurfaceCapabilitiesKHR)                     \
    X(vkGetPhysicalDeviceSurfacePresentModesKHR) X(vkCreateSwapchainKHR)                      \
    X(vkGetSwapchainImagesKHR) X(vkAcquireNextImageKHR) X(vkQueuePresentKHR) X(vkCreateSemaphore)
PRESENT_FUNCTIONS(FN)
#endif
static PFN_vkGetInstanceProcAddr vkGetInstanceProcAddr;
static PFN_vkCreateInstance vkCreateInstance;

static void check(VkResult result, const char *what)
{
    if (result != VK_SUCCESS) {
        fprintf(stderr, "%s failed: %d\n", what, result);
        exit(1);
    }
}

static double now(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static uint32_t memory_type(VkPhysicalDevice gpu, uint32_t bits, VkMemoryPropertyFlags want)
{
    VkPhysicalDeviceMemoryProperties props;
    vkGetPhysicalDeviceMemoryProperties(gpu, &props);
    for (uint32_t i = 0; i < props.memoryTypeCount; i++)
        if ((bits & (1u << i)) && (props.memoryTypes[i].propertyFlags & want) == want)
            return i;
    fprintf(stderr, "no memory type for %#x\n", want);
    exit(1);
}

struct push {
    float color[4];
    float scale;
    float layers;
    float iterations;
    float pad;
};

int main(int argc, char **argv)
{
#ifdef __APPLE__
    const char *library = "libMoltenVK.dylib";
#else
    const char *library = "libvulkan.so.1";
#endif
    const char *scene = "heavy", *mode = "pipe", *present_mode = "immediate";
    uint32_t width = 1920, height = 1080;
    double seconds = 10;
    for (int i = 1; i + 1 < argc; i += 2) {
        if (!strcmp(argv[i], "-l")) library = argv[i + 1];
        else if (!strcmp(argv[i], "-s")) scene = argv[i + 1];
        else if (!strcmp(argv[i], "-m")) mode = argv[i + 1];
        else if (!strcmp(argv[i], "-p")) present_mode = argv[i + 1];
        else if (!strcmp(argv[i], "-w")) width = atoi(argv[i + 1]);
        else if (!strcmp(argv[i], "-h")) height = atoi(argv[i + 1]);
        else if (!strcmp(argv[i], "-t")) seconds = atof(argv[i + 1]);
        else { fprintf(stderr, "unknown option %s\n", argv[i]); return 2; }
    }
    int heavy = !strcmp(scene, "heavy");
    int copy = !strcmp(mode, "copy");
    int x11 = !strcmp(mode, "x11");
#ifndef __linux__
    (void)present_mode;
#endif
    uint32_t in_flight = !strcmp(mode, "sync") ? 1 : 3;

    void *handle = dlopen(library, RTLD_NOW | RTLD_LOCAL);
    if (!handle) { fprintf(stderr, "dlopen %s: %s\n", library, dlerror()); return 1; }
    vkGetInstanceProcAddr = (PFN_vkGetInstanceProcAddr)dlsym(handle, "vkGetInstanceProcAddr");
    vkCreateInstance = (PFN_vkCreateInstance)vkGetInstanceProcAddr(NULL, "vkCreateInstance");

    VkApplicationInfo app = {VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName = "vk-offscreen-bench",
                             .apiVersion = VK_API_VERSION_1_1};
    const char *instance_extensions[] = {"VK_KHR_surface", "VK_KHR_xcb_surface"};
    VkInstanceCreateInfo instance_info = {VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app,
                                          .enabledExtensionCount = x11 ? 2 : 0,
                                          .ppEnabledExtensionNames = instance_extensions};
    VkInstance instance;
    check(vkCreateInstance(&instance_info, NULL, &instance), "vkCreateInstance");
#define LOAD(name) name = (PFN_##name)vkGetInstanceProcAddr(instance, #name);
    FUNCTIONS(LOAD)
#ifdef __linux__
    PRESENT_FUNCTIONS(LOAD)
#endif

    uint32_t count = 1;
    VkPhysicalDevice gpu;
    VkResult enumerated = vkEnumeratePhysicalDevices(instance, &count, &gpu);
    if (enumerated != VK_SUCCESS && enumerated != VK_INCOMPLETE) check(enumerated, "vkEnumeratePhysicalDevices");
    VkPhysicalDeviceProperties props;
    vkGetPhysicalDeviceProperties(gpu, &props);
    uint32_t families = 16, family = UINT32_MAX;
    VkQueueFamilyProperties family_props[16];
    vkGetPhysicalDeviceQueueFamilyProperties(gpu, &families, family_props);
    for (uint32_t i = 0; i < families && family == UINT32_MAX; i++)
        if (family_props[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) family = i;

    float priority = 1;
    VkDeviceQueueCreateInfo queue_info = {VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = family,
                                          .queueCount = 1, .pQueuePriorities = &priority};
#ifdef __APPLE__
    const char *device_extensions[] = {"VK_KHR_portability_subset"};
    uint32_t device_extension_count = 1;
#else
    const char *device_extensions[] = {"VK_KHR_swapchain"};
    uint32_t device_extension_count = x11 ? 1 : 0;
#endif
    VkDeviceCreateInfo device_info = {VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1,
                                      .pQueueCreateInfos = &queue_info,
                                      .enabledExtensionCount = device_extension_count,
                                      .ppEnabledExtensionNames = device_extensions};
    VkDevice device;
    check(vkCreateDevice(gpu, &device_info, NULL, &device), "vkCreateDevice");
    VkQueue queue;
    vkGetDeviceQueue(device, family, 0, &queue);

    VkImageCreateInfo image_info = {VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D,
                                    // The swapchain's format, so one pipeline renders to both.
                                    .format = x11 ? VK_FORMAT_B8G8R8A8_UNORM : VK_FORMAT_R8G8B8A8_UNORM, .extent = {width, height, 1},
                                    .mipLevels = 1, .arrayLayers = 1, .samples = VK_SAMPLE_COUNT_1_BIT,
                                    .tiling = VK_IMAGE_TILING_OPTIMAL,
                                    .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT};
    VkImage image;
    check(vkCreateImage(device, &image_info, NULL, &image), "vkCreateImage");
    VkMemoryRequirements req;
    vkGetImageMemoryRequirements(device, image, &req);
    VkMemoryAllocateInfo alloc = {VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = req.size,
                                  .memoryTypeIndex = memory_type(gpu, req.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)};
    VkDeviceMemory image_memory;
    check(vkAllocateMemory(device, &alloc, NULL, &image_memory), "vkAllocateMemory image");
    check(vkBindImageMemory(device, image, image_memory, 0), "vkBindImageMemory");
    VkImageViewCreateInfo view_info = {VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = image,
                                       .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = image_info.format,
                                       .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
    VkImageView view;
    check(vkCreateImageView(device, &view_info, NULL, &view), "vkCreateImageView");

    VkAttachmentDescription attachment = {.format = image_info.format, .samples = VK_SAMPLE_COUNT_1_BIT,
                                          .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
                                          .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
                                          .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
                                          .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
                                          .finalLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL};
    VkAttachmentReference color_ref = {0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
    VkSubpassDescription subpass = {.pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS,
                                    .colorAttachmentCount = 1, .pColorAttachments = &color_ref};
    VkRenderPassCreateInfo pass_info = {VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO, .attachmentCount = 1,
                                        .pAttachments = &attachment, .subpassCount = 1, .pSubpasses = &subpass};
    VkRenderPass pass;
    check(vkCreateRenderPass(device, &pass_info, NULL, &pass), "vkCreateRenderPass");
    VkFramebufferCreateInfo fb_info = {VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO, .renderPass = pass,
                                       .attachmentCount = 1, .pAttachments = &view,
                                       .width = width, .height = height, .layers = 1};
    VkFramebuffer framebuffer;
    check(vkCreateFramebuffer(device, &fb_info, NULL, &framebuffer), "vkCreateFramebuffer");

    VkShaderModuleCreateInfo vs_info = {VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                                        .codeSize = sizeof(bench_vert), .pCode = bench_vert};
    VkShaderModuleCreateInfo fs_info = {VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                                        .codeSize = sizeof(bench_frag), .pCode = bench_frag};
    VkShaderModule vs, fs;
    check(vkCreateShaderModule(device, &vs_info, NULL, &vs), "vkCreateShaderModule vert");
    check(vkCreateShaderModule(device, &fs_info, NULL, &fs), "vkCreateShaderModule frag");
    VkPushConstantRange range = {VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, 0, sizeof(struct push)};
    VkPipelineLayoutCreateInfo layout_info = {VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
                                              .pushConstantRangeCount = 1, .pPushConstantRanges = &range};
    VkPipelineLayout layout;
    check(vkCreatePipelineLayout(device, &layout_info, NULL, &layout), "vkCreatePipelineLayout");
    VkPipelineShaderStageCreateInfo stages[2] = {
        {VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_VERTEX_BIT, .module = vs, .pName = "main"},
        {VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_FRAGMENT_BIT, .module = fs, .pName = "main"}};
    VkPipelineVertexInputStateCreateInfo vertex_input = {VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO};
    VkPipelineInputAssemblyStateCreateInfo assembly = {VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
                                                       .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST};
    VkViewport viewport = {0, 0, width, height, 0, 1};
    VkRect2D scissor = {{0, 0}, {width, height}};
    VkPipelineViewportStateCreateInfo viewport_state = {VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
                                                        .viewportCount = 1, .pViewports = &viewport,
                                                        .scissorCount = 1, .pScissors = &scissor};
    VkPipelineRasterizationStateCreateInfo raster = {VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
                                                     .polygonMode = VK_POLYGON_MODE_FILL, .cullMode = VK_CULL_MODE_NONE,
                                                     .lineWidth = 1};
    VkPipelineMultisampleStateCreateInfo multisample = {VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
                                                        .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT};
    VkPipelineColorBlendAttachmentState blend_attachment = {.blendEnable = VK_TRUE,
                                                            .srcColorBlendFactor = VK_BLEND_FACTOR_ONE,
                                                            .dstColorBlendFactor = VK_BLEND_FACTOR_ONE,
                                                            .colorBlendOp = VK_BLEND_OP_ADD,
                                                            .srcAlphaBlendFactor = VK_BLEND_FACTOR_ONE,
                                                            .dstAlphaBlendFactor = VK_BLEND_FACTOR_ZERO,
                                                            .alphaBlendOp = VK_BLEND_OP_ADD,
                                                            .colorWriteMask = 0xf};
    VkPipelineColorBlendStateCreateInfo blend = {VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
                                                 .attachmentCount = 1, .pAttachments = &blend_attachment};
    VkGraphicsPipelineCreateInfo pipeline_info = {VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, .stageCount = 2,
                                                  .pStages = stages, .pVertexInputState = &vertex_input,
                                                  .pInputAssemblyState = &assembly, .pViewportState = &viewport_state,
                                                  .pRasterizationState = &raster, .pMultisampleState = &multisample,
                                                  .pColorBlendState = &blend, .layout = layout, .renderPass = pass};
    VkPipeline pipeline;
    check(vkCreateGraphicsPipelines(device, VK_NULL_HANDLE, 1, &pipeline_info, NULL, &pipeline), "vkCreateGraphicsPipelines");

    VkBufferCreateInfo buffer_info = {VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = (VkDeviceSize)width * height * 4,
                                      .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT};
    VkBuffer buffer;
    check(vkCreateBuffer(device, &buffer_info, NULL, &buffer), "vkCreateBuffer");
    vkGetBufferMemoryRequirements(device, buffer, &req);
    alloc.allocationSize = req.size;
    alloc.memoryTypeIndex = memory_type(gpu, req.memoryTypeBits,
                                        VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_CACHED_BIT);
    VkDeviceMemory buffer_memory;
    check(vkAllocateMemory(device, &alloc, NULL, &buffer_memory), "vkAllocateMemory buffer");
    check(vkBindBufferMemory(device, buffer, buffer_memory, 0), "vkBindBufferMemory");
    volatile uint8_t *pixels;
    check(vkMapMemory(device, buffer_memory, 0, VK_WHOLE_SIZE, 0, (void **)&pixels), "vkMapMemory");

    VkCommandPoolCreateInfo pool_info = {VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = family};
    VkCommandPool pool;
    check(vkCreateCommandPool(device, &pool_info, NULL, &pool), "vkCreateCommandPool");
    // Two prerecorded command buffers: render only, and render plus readback.
    VkCommandBufferAllocateInfo cb_info = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = pool,
                                           .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 2};
    VkCommandBuffer commands[2];
    check(vkAllocateCommandBuffers(device, &cb_info, commands), "vkAllocateCommandBuffers");
    // Layers add up (so the GPU cannot skip hidden ones) to 64,128,192 (exact in 8 bits).
    float layers = heavy ? 16 : 1;
    struct push push = {{64 / 255.0f / layers, 128 / 255.0f / layers, 192 / 255.0f / layers, 1},
                        heavy ? 1.0f : 0.1f, layers, heavy ? 10 : 1, 0};
    for (int c = 0; c < 2; c++) {
        VkCommandBufferBeginInfo begin = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
                                          .flags = VK_COMMAND_BUFFER_USAGE_SIMULTANEOUS_USE_BIT};
        check(vkBeginCommandBuffer(commands[c], &begin), "vkBeginCommandBuffer");
        VkClearValue clear = {.color = {{0, 0, 0, 1}}};
        VkRenderPassBeginInfo rp = {VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO, .renderPass = pass,
                                    .framebuffer = framebuffer, .renderArea = scissor,
                                    .clearValueCount = 1, .pClearValues = &clear};
        vkCmdBeginRenderPass(commands[c], &rp, VK_SUBPASS_CONTENTS_INLINE);
        vkCmdBindPipeline(commands[c], VK_PIPELINE_BIND_POINT_GRAPHICS, pipeline);
        vkCmdPushConstants(commands[c], layout, range.stageFlags, 0, sizeof(push), &push);
        vkCmdDraw(commands[c], 3, (uint32_t)push.layers, 0, 0);
        vkCmdEndRenderPass(commands[c]);
        if (c == 1) {
            VkBufferImageCopy region = {.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
                                        .imageExtent = {width, height, 1}};
            vkCmdCopyImageToBuffer(commands[c], image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, buffer, 1, &region);
            VkMemoryBarrier barrier = {VK_STRUCTURE_TYPE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                                       .dstAccessMask = VK_ACCESS_HOST_READ_BIT};
            vkCmdPipelineBarrier(commands[c], VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0,
                                 1, &barrier, 0, NULL, 0, NULL);
        }
        check(vkEndCommandBuffer(commands[c]), "vkEndCommandBuffer");
    }

    VkFence fences[3];
    VkFenceCreateInfo fence_info = {VK_STRUCTURE_TYPE_FENCE_CREATE_INFO, .flags = VK_FENCE_CREATE_SIGNALED_BIT};
    for (uint32_t i = 0; i < 3; i++) check(vkCreateFence(device, &fence_info, NULL, &fences[i]), "vkCreateFence");
    VkMappedMemoryRange mapped = {VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE, .memory = buffer_memory, .size = VK_WHOLE_SIZE};

    // One warm-up frame, then the timed loop.
    unsigned long frames = 0, checksum = 0;
    uint8_t *frame = malloc((size_t)width * height * 4);
    double start = 0, end = 0;
#ifdef __linux__
    if (x11) {
        xcb_connection_t *connection = xcb_connect(NULL, NULL);
        if (xcb_connection_has_error(connection)) { fprintf(stderr, "cannot connect to X\n"); return 1; }
        xcb_screen_t *screen = xcb_setup_roots_iterator(xcb_get_setup(connection)).data;
        xcb_window_t window = xcb_generate_id(connection);
        xcb_create_window(connection, XCB_COPY_FROM_PARENT, window, screen->root, 0, 0, width, height, 0,
                          XCB_WINDOW_CLASS_INPUT_OUTPUT, screen->root_visual, 0, NULL);
        xcb_map_window(connection, window);
        xcb_flush(connection);
        VkXcbSurfaceCreateInfoKHR surface_info = {VK_STRUCTURE_TYPE_XCB_SURFACE_CREATE_INFO_KHR,
                                                  .connection = connection, .window = window};
        VkSurfaceKHR surface;
        check(vkCreateXcbSurfaceKHR(instance, &surface_info, NULL, &surface), "vkCreateXcbSurfaceKHR");
        VkSurfaceCapabilitiesKHR caps;
        check(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(gpu, surface, &caps), "surface capabilities");
        VkPresentModeKHR wanted = !strcmp(present_mode, "fifo")      ? VK_PRESENT_MODE_FIFO_KHR
                                  : !strcmp(present_mode, "mailbox") ? VK_PRESENT_MODE_MAILBOX_KHR
                                                                     : VK_PRESENT_MODE_IMMEDIATE_KHR;
        uint32_t mode_count = 8;
        VkPresentModeKHR modes[8];
        vkGetPhysicalDeviceSurfacePresentModesKHR(gpu, surface, &mode_count, modes);
        VkPresentModeKHR chosen = VK_PRESENT_MODE_FIFO_KHR;
        for (uint32_t i = 0; i < mode_count; i++) if (modes[i] == wanted) chosen = wanted;
        VkSwapchainCreateInfoKHR swap_info = {VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR, .surface = surface,
                                              .minImageCount = caps.minImageCount < 3 ? 3 : caps.minImageCount,
                                              .imageFormat = image_info.format,
                                              .imageColorSpace = VK_COLOR_SPACE_SRGB_NONLINEAR_KHR,
                                              .imageExtent = {width, height}, .imageArrayLayers = 1,
                                              .imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
                                              .preTransform = caps.currentTransform,
                                              .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
                                              .presentMode = chosen, .clipped = VK_TRUE};
        VkSwapchainKHR swapchain;
        check(vkCreateSwapchainKHR(device, &swap_info, NULL, &swapchain), "vkCreateSwapchainKHR");
        uint32_t image_count = 8;
        VkImage images[8];
        check(vkGetSwapchainImagesKHR(device, swapchain, &image_count, images), "vkGetSwapchainImagesKHR");
        attachment.finalLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
        VkRenderPass present_pass;
        check(vkCreateRenderPass(device, &pass_info, NULL, &present_pass), "vkCreateRenderPass present");
        VkCommandBuffer present_commands[8];
        cb_info.commandBufferCount = image_count;
        check(vkAllocateCommandBuffers(device, &cb_info, present_commands), "vkAllocateCommandBuffers");
        for (uint32_t i = 0; i < image_count; i++) {
            view_info.image = images[i];
            VkImageView image_view;
            check(vkCreateImageView(device, &view_info, NULL, &image_view), "vkCreateImageView");
            fb_info.renderPass = present_pass;
            fb_info.pAttachments = &image_view;
            VkFramebuffer fb;
            check(vkCreateFramebuffer(device, &fb_info, NULL, &fb), "vkCreateFramebuffer");
            VkCommandBufferBeginInfo begin = {VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
            check(vkBeginCommandBuffer(present_commands[i], &begin), "vkBeginCommandBuffer");
            VkClearValue clear = {.color = {{0, 0, 0, 1}}};
            VkRenderPassBeginInfo rp = {VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO, .renderPass = present_pass,
                                        .framebuffer = fb, .renderArea = scissor,
                                        .clearValueCount = 1, .pClearValues = &clear};
            vkCmdBeginRenderPass(present_commands[i], &rp, VK_SUBPASS_CONTENTS_INLINE);
            vkCmdBindPipeline(present_commands[i], VK_PIPELINE_BIND_POINT_GRAPHICS, pipeline);
            vkCmdPushConstants(present_commands[i], layout, range.stageFlags, 0, sizeof(push), &push);
            vkCmdDraw(present_commands[i], 3, (uint32_t)push.layers, 0, 0);
            vkCmdEndRenderPass(present_commands[i]);
            check(vkEndCommandBuffer(present_commands[i]), "vkEndCommandBuffer");
        }
        VkSemaphore acquired[3], rendered[8];
        VkSemaphoreCreateInfo semaphore_info = {VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
        for (uint32_t i = 0; i < 3; i++) check(vkCreateSemaphore(device, &semaphore_info, NULL, &acquired[i]), "sem");
        for (uint32_t i = 0; i < image_count; i++) check(vkCreateSemaphore(device, &semaphore_info, NULL, &rendered[i]), "sem");
        for (;;) {
            VkFence fence = fences[frames % 3];
            check(vkWaitForFences(device, 1, &fence, VK_TRUE, UINT64_MAX), "vkWaitForFences");
            end = now();
            if (frames == 1) start = end;
            if (frames > 1 && end - start >= seconds) break;
            uint32_t index;
            check(vkAcquireNextImageKHR(device, swapchain, UINT64_MAX, acquired[frames % 3], VK_NULL_HANDLE, &index),
                  "vkAcquireNextImageKHR");
            check(vkResetFences(device, 1, &fence), "vkResetFences");
            VkPipelineStageFlags stage = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
            VkSubmitInfo submit = {VK_STRUCTURE_TYPE_SUBMIT_INFO, .waitSemaphoreCount = 1,
                                   .pWaitSemaphores = &acquired[frames % 3], .pWaitDstStageMask = &stage,
                                   .commandBufferCount = 1, .pCommandBuffers = &present_commands[index],
                                   .signalSemaphoreCount = 1, .pSignalSemaphores = &rendered[index]};
            check(vkQueueSubmit(queue, 1, &submit, fence), "vkQueueSubmit");
            VkPresentInfoKHR present = {VK_STRUCTURE_TYPE_PRESENT_INFO_KHR, .waitSemaphoreCount = 1,
                                        .pWaitSemaphores = &rendered[index], .swapchainCount = 1,
                                        .pSwapchains = &swapchain, .pImageIndices = &index};
            VkResult presented = vkQueuePresentKHR(queue, &present);
            if (presented != VK_SUBOPTIMAL_KHR) check(presented, "vkQueuePresentKHR");
            frames++;
        }
        in_flight = 3;
        printf("VK_BENCH_PRESENT present_mode=%s images=%u\n",
               chosen == VK_PRESENT_MODE_IMMEDIATE_KHR ? "immediate"
               : chosen == VK_PRESENT_MODE_MAILBOX_KHR ? "mailbox" : "fifo", image_count);
    }
#endif
    for (; !x11;) {
        VkFence fence = fences[frames % in_flight];
        check(vkWaitForFences(device, 1, &fence, VK_TRUE, UINT64_MAX), "vkWaitForFences");
        if (copy && frames > 0) {
            // The previous frame in this slot finished: read it like a presenter would.
            vkInvalidateMappedMemoryRanges(device, 1, &mapped);
            memcpy(frame, (const void *)pixels, (size_t)width * height * 4);
            checksum += frame[(size_t)width * height * 2];
        }
        end = now();
        if (frames == 1) start = end;
        if (frames > 1 && end - start >= seconds) break;
        check(vkResetFences(device, 1, &fence), "vkResetFences");
        VkSubmitInfo submit = {VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1,
                               .pCommandBuffers = &commands[copy ? 1 : 0]};
        check(vkQueueSubmit(queue, 1, &submit, fence), "vkQueueSubmit");
        frames++;
    }
    check(vkDeviceWaitIdle(device), "vkDeviceWaitIdle");
    unsigned long timed = frames - 1 - (in_flight - 1);

    // Final readback (offscreen): the centre is the sum of the layers, a
    // corner outside the light scene's small triangle stays the clear colour.
    VkSubmitInfo submit = {VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &commands[1]};
    check(vkResetFences(device, 1, &fences[0]), "vkResetFences");
    check(vkQueueSubmit(queue, 1, &submit, fences[0]), "vkQueueSubmit");
    check(vkWaitForFences(device, 1, &fences[0], VK_TRUE, UINT64_MAX), "vkWaitForFences");
    vkInvalidateMappedMemoryRanges(device, 1, &mapped);
    const volatile uint8_t *centre = pixels + ((size_t)(height / 2) * width + width / 2) * 4;
    const volatile uint8_t *corner = pixels + 4;
    int r = x11 ? 2 : 0, b = x11 ? 0 : 2;
    int ok = abs(centre[r] - 64) <= 2 && abs(centre[1] - 128) <= 2 && abs(centre[b] - 192) <= 2 &&
             (heavy ? (abs(corner[r] - 64) <= 2) : (corner[0] == 0 && corner[1] == 0 && corner[2] == 0));
    printf("VK_BENCH device=\"%s\" scene=%s mode=%s size=%ux%u frames=%lu seconds=%.2f fps=%.1f pixel=%s"
           " centre=%u,%u,%u checksum=%lu\n",
           props.deviceName, scene, mode, width, height, timed, end - start, timed / (end - start),
           ok ? "ok" : "BAD", centre[r], centre[1], centre[b], checksum);
    return ok ? 0 : 1;
}
