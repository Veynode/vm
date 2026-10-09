/* Small real Vulkan raster/readback oracle; no window, timing or compatibility
 * claim. */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vulkan.h>
#define OK(x)                                                                  \
  do {                                                                         \
    VkResult r = (x);                                                          \
    if (r != VK_SUCCESS) {                                                     \
      fprintf(stderr, "VK_PIXEL_FAIL call=%s result=%d\n", #x, r);             \
      exit(1);                                                                 \
    }                                                                          \
  } while (0)
static uint32_t memory_type(VkPhysicalDevice p, uint32_t bits,
                            VkMemoryPropertyFlags flags) {
  VkPhysicalDeviceMemoryProperties m;
  vkGetPhysicalDeviceMemoryProperties(p, &m);
  for (uint32_t i = 0; i < m.memoryTypeCount; i++)
    if ((bits & (1u << i)) && (m.memoryTypes[i].propertyFlags & flags) == flags)
      return i;
  fprintf(stderr, "VK_PIXEL_FAIL memory_type\n");
  exit(1);
}
static VkShaderModule shader(VkDevice d, const char *path) {
  FILE *f = fopen(path, "rb");
  if (!f)
    exit(1);
  fseek(f, 0, SEEK_END);
  long n = ftell(f);
  rewind(f);
  if (n <= 0 || n > 65536 || n % 4)
    exit(1);
  uint32_t *p = malloc((size_t)n);
  if (!p || fread(p, 1, (size_t)n, f) != (size_t)n)
    exit(1);
  fclose(f);
  VkShaderModuleCreateInfo c = {.sType =
                                    VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
                                .codeSize = (size_t)n,
                                .pCode = p};
  VkShaderModule s;
  OK(vkCreateShaderModule(d, &c, NULL, &s));
  free(p);
  return s;
}
int main(void) {
  VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
                           .pApplicationName = "Veynode Vulkan pixel oracle",
                           .apiVersion = VK_API_VERSION_1_2};
  VkInstanceCreateInfo ic = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
                             .pApplicationInfo = &app};
  VkInstance inst;
  OK(vkCreateInstance(&ic, NULL, &inst));
  uint32_t count = 0;
  OK(vkEnumeratePhysicalDevices(inst, &count, NULL));
  if (count != 1) {
    fprintf(stderr, "VK_PIXEL_FAIL device_count=%u\n", count);
    return 1;
  }
  VkPhysicalDevice phy;
  OK(vkEnumeratePhysicalDevices(inst, &count, &phy));
  VkPhysicalDeviceDriverProperties driver = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRIVER_PROPERTIES};
  VkPhysicalDeviceProperties2 prop = {
      .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
      .pNext = &driver};
  vkGetPhysicalDeviceProperties2(phy, &prop);
  if (prop.properties.deviceType != VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU ||
      prop.properties.vendorID != 0x106b ||
      driver.driverID != VK_DRIVER_ID_MESA_VENUS ||
      !strstr(prop.properties.deviceName, "Venus (Apple")) {
    fprintf(stderr, "VK_PIXEL_FAIL hardware_identity\n");
    return 1;
  }
  printf("VK_PIXEL_DEVICE name=%s driver=%s vendor=%x type=%u api=%u\n",
         prop.properties.deviceName, driver.driverName,
         prop.properties.vendorID, prop.properties.deviceType,
         prop.properties.apiVersion);
  uint32_t nq = 0;
  vkGetPhysicalDeviceQueueFamilyProperties(phy, &nq, NULL);
  if (nq > 64 || !nq)
    return 1;
  VkQueueFamilyProperties qs[64];
  vkGetPhysicalDeviceQueueFamilyProperties(phy, &nq, qs);
  uint32_t qi = 0;
  while (qi < nq && !(qs[qi].queueFlags & VK_QUEUE_GRAPHICS_BIT))
    qi++;
  if (qi == nq)
    return 1;
  float priority = 1;
  VkDeviceQueueCreateInfo qc = {.sType =
                                    VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
                                .queueFamilyIndex = qi,
                                .queueCount = 1,
                                .pQueuePriorities = &priority};
  uint32_t ne = 0;
  OK(vkEnumerateDeviceExtensionProperties(phy, NULL, &ne, NULL));
  VkExtensionProperties *es = calloc(ne, sizeof(*es));
  OK(vkEnumerateDeviceExtensionProperties(phy, NULL, &ne, es));
  const char *ext = "VK_KHR_portability_subset";
  int portability = 0;
  for (uint32_t i = 0; i < ne; i++)
    if (!strcmp(es[i].extensionName, ext))
      portability = 1;
  free(es);
  VkDeviceCreateInfo dc = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
                           .queueCreateInfoCount = 1,
                           .pQueueCreateInfos = &qc,
                           .enabledExtensionCount = (uint32_t)portability,
                           .ppEnabledExtensionNames =
                               portability ? &ext : NULL};
  VkDevice dev;
  OK(vkCreateDevice(phy, &dc, NULL, &dev));
  VkQueue queue;
  vkGetDeviceQueue(dev, qi, 0, &queue);
  VkImageCreateInfo imc = {.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
                           .imageType = VK_IMAGE_TYPE_2D,
                           .format = VK_FORMAT_R8G8B8A8_UNORM,
                           .extent = {64, 64, 1},
                           .mipLevels = 1,
                           .arrayLayers = 1,
                           .samples = VK_SAMPLE_COUNT_1_BIT,
                           .tiling = VK_IMAGE_TILING_OPTIMAL,
                           .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
                                    VK_IMAGE_USAGE_TRANSFER_SRC_BIT,
                           .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
                           .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED};
  VkImage image;
  OK(vkCreateImage(dev, &imc, NULL, &image));
  VkMemoryRequirements req;
  vkGetImageMemoryRequirements(dev, image, &req);
  VkMemoryAllocateInfo ma = {
      .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
      .allocationSize = req.size,
      .memoryTypeIndex = memory_type(phy, req.memoryTypeBits,
                                     VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)};
  VkDeviceMemory image_mem;
  OK(vkAllocateMemory(dev, &ma, NULL, &image_mem));
  OK(vkBindImageMemory(dev, image, image_mem, 0));
  VkImageViewCreateInfo vc = {
      .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
      .image = image,
      .viewType = VK_IMAGE_VIEW_TYPE_2D,
      .format = imc.format,
      .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
  VkImageView view;
  OK(vkCreateImageView(dev, &vc, NULL, &view));
  VkBufferCreateInfo bc = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
                           .size = 64 * 64 * 4,
                           .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT,
                           .sharingMode = VK_SHARING_MODE_EXCLUSIVE};
  VkBuffer buffer;
  OK(vkCreateBuffer(dev, &bc, NULL, &buffer));
  vkGetBufferMemoryRequirements(dev, buffer, &req);
  ma.allocationSize = req.size;
  ma.memoryTypeIndex = memory_type(phy, req.memoryTypeBits,
                                   VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                       VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
  VkDeviceMemory host_mem;
  OK(vkAllocateMemory(dev, &ma, NULL, &host_mem));
  OK(vkBindBufferMemory(dev, buffer, host_mem, 0));
  void *pixels;
  OK(vkMapMemory(dev, host_mem, 0, VK_WHOLE_SIZE, 0, &pixels));
  VkAttachmentDescription attachment = {
      .format = imc.format,
      .samples = VK_SAMPLE_COUNT_1_BIT,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR,
      .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
      .stencilLoadOp = VK_ATTACHMENT_LOAD_OP_DONT_CARE,
      .stencilStoreOp = VK_ATTACHMENT_STORE_OP_DONT_CARE,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
      .finalLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL};
  VkAttachmentReference ar = {0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
  VkSubpassDescription sp = {.pipelineBindPoint =
                                 VK_PIPELINE_BIND_POINT_GRAPHICS,
                             .colorAttachmentCount = 1,
                             .pColorAttachments = &ar};
  VkSubpassDependency deps[2] = {
      {.srcSubpass = VK_SUBPASS_EXTERNAL,
       .dstSubpass = 0,
       .srcStageMask = VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
       .dstStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
       .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT},
      {.srcSubpass = 0,
       .dstSubpass = VK_SUBPASS_EXTERNAL,
       .srcStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
       .dstStageMask = VK_PIPELINE_STAGE_TRANSFER_BIT,
       .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
       .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT}};
  VkRenderPassCreateInfo rc = {.sType =
                                   VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO,
                               .attachmentCount = 1,
                               .pAttachments = &attachment,
                               .subpassCount = 1,
                               .pSubpasses = &sp,
                               .dependencyCount = 2,
                               .pDependencies = deps};
  VkRenderPass pass;
  OK(vkCreateRenderPass(dev, &rc, NULL, &pass));
  VkFramebufferCreateInfo fc = {.sType =
                                    VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO,
                                .renderPass = pass,
                                .attachmentCount = 1,
                                .pAttachments = &view,
                                .width = 64,
                                .height = 64,
                                .layers = 1};
  VkFramebuffer fb;
  OK(vkCreateFramebuffer(dev, &fc, NULL, &fb));
  VkShaderModule vs = shader(dev, "/veynode/triangle.vert.spv"),
                 fs = shader(dev, "/veynode/triangle.frag.spv");
  VkPushConstantRange push = {VK_SHADER_STAGE_FRAGMENT_BIT, 0, 16};
  VkPipelineLayoutCreateInfo plc = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
      .pushConstantRangeCount = 1,
      .pPushConstantRanges = &push};
  VkPipelineLayout layout;
  OK(vkCreatePipelineLayout(dev, &plc, NULL, &layout));
  VkPipelineShaderStageCreateInfo stages[2] = {
      {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
       .stage = VK_SHADER_STAGE_VERTEX_BIT,
       .module = vs,
       .pName = "main"},
      {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
       .stage = VK_SHADER_STAGE_FRAGMENT_BIT,
       .module = fs,
       .pName = "main"}};
  VkPipelineVertexInputStateCreateInfo vi = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO};
  VkPipelineInputAssemblyStateCreateInfo ia = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
      .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST};
  VkViewport vp = {0, 0, 64, 64, 0, 1};
  VkRect2D rect = {{0, 0}, {64, 64}};
  VkPipelineViewportStateCreateInfo vps = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
      .viewportCount = 1,
      .pViewports = &vp,
      .scissorCount = 1,
      .pScissors = &rect};
  VkPipelineRasterizationStateCreateInfo rs = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
      .polygonMode = VK_POLYGON_MODE_FILL,
      .cullMode = VK_CULL_MODE_NONE,
      .frontFace = VK_FRONT_FACE_COUNTER_CLOCKWISE,
      .lineWidth = 1};
  VkPipelineMultisampleStateCreateInfo ms = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
      .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT};
  VkPipelineColorBlendAttachmentState ba = {.colorWriteMask = 15};
  VkPipelineColorBlendStateCreateInfo bs = {
      .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
      .attachmentCount = 1,
      .pAttachments = &ba};
  VkGraphicsPipelineCreateInfo pc = {
      .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO,
      .stageCount = 2,
      .pStages = stages,
      .pVertexInputState = &vi,
      .pInputAssemblyState = &ia,
      .pViewportState = &vps,
      .pRasterizationState = &rs,
      .pMultisampleState = &ms,
      .pColorBlendState = &bs,
      .layout = layout,
      .renderPass = pass};
  VkPipeline pipe;
  OK(vkCreateGraphicsPipelines(dev, VK_NULL_HANDLE, 1, &pc, NULL, &pipe));
  VkCommandPoolCreateInfo poolc = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
      .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
      .queueFamilyIndex = qi};
  VkCommandPool pool;
  OK(vkCreateCommandPool(dev, &poolc, NULL, &pool));
  VkCommandBufferAllocateInfo ca = {
      .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
      .commandPool = pool,
      .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
      .commandBufferCount = 1};
  VkCommandBuffer cmd;
  OK(vkAllocateCommandBuffers(dev, &ca, &cmd));
  VkFenceCreateInfo fencec = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
  VkFence fence;
  OK(vkCreateFence(dev, &fencec, NULL, &fence));
  for (unsigned frame = 0; frame < 3; frame++) {
    memset(pixels, 0x55, 64 * 64 * 4);
    OK(vkResetCommandBuffer(cmd, 0));
    VkCommandBufferBeginInfo begin = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
    OK(vkBeginCommandBuffer(cmd, &begin));
    VkClearValue clear = {.color = {{0, 0, 0, 1}}};
    VkRenderPassBeginInfo rb = {.sType =
                                    VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO,
                                .renderPass = pass,
                                .framebuffer = fb,
                                .renderArea = rect,
                                .clearValueCount = 1,
                                .pClearValues = &clear};
    vkCmdBeginRenderPass(cmd, &rb, VK_SUBPASS_CONTENTS_INLINE);
    vkCmdBindPipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pipe);
    float color[4] = {0, 0, 0, 1};
    color[frame] = 1;
    vkCmdPushConstants(cmd, layout, VK_SHADER_STAGE_FRAGMENT_BIT, 0, 16, color);
    vkCmdDraw(cmd, 3, 1, 0, 0);
    vkCmdEndRenderPass(cmd);
    VkBufferImageCopy copy = {
        .imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
        .imageExtent = {64, 64, 1}};
    vkCmdCopyImageToBuffer(cmd, image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                           buffer, 1, &copy);
    VkMemoryBarrier barrier = {.sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
                               .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                               .dstAccessMask = VK_ACCESS_HOST_READ_BIT};
    vkCmdPipelineBarrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT,
                         VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &barrier, 0, NULL, 0,
                         NULL);
    OK(vkEndCommandBuffer(cmd));
    VkSubmitInfo submit = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
                           .commandBufferCount = 1,
                           .pCommandBuffers = &cmd};
    OK(vkQueueSubmit(queue, 1, &submit, fence));
    OK(vkWaitForFences(dev, 1, &fence, VK_TRUE, 5000000000ULL));
    OK(vkResetFences(dev, 1, &fence));
    unsigned tested = 0, bad = 0;
    for (int y = 0; y < 64; y++)
      for (int x = 0; x < 64; x++) {
        /* Expected triangle vertices in framebuffer pixels:
           (8,8),(56,8),(32,56). Ignore a 2-pixel band around edges; interior
           and exterior are independent oracles. */
        float px = x + .5f, py = y + .5f, left = 8 + (py - 8) / 2,
              right = 56 - (py - 8) / 2;
        int inside = py > 10 && py < 54 && px > left + 2 && px < right - 2;
        int outside = py < 6 || py > 58 || px < left - 2 || px > right + 2;
        if (!inside && !outside)
          continue;
        tested++;
        uint8_t *q = (uint8_t *)pixels + 4 * (y * 64 + x);
        for (unsigned k = 0; k < 4; k++) {
          unsigned expected = k == 3 ? 255 : (inside && k == frame ? 255 : 0);
          if (q[k] != expected) {
            bad++;
            break;
          }
        }
      }
    printf("VK_PIXEL_CASE frame=%u tested=%u mismatches=%u\n", frame, tested,
           bad);
    if (bad || tested < 3000) {
      fprintf(stderr, "VK_PIXEL_FAIL pixels\n");
      return 1;
    }
  }
  FILE *out = fopen("/veynode/vulkan-triangle.ppm", "wb");
  if (!out)
    return 1;
  fprintf(out, "P6\n64 64\n255\n");
  for (unsigned i = 0; i < 64 * 64; i++)
    if (fwrite((uint8_t *)pixels + 4 * i, 1, 3, out) != 3)
      return 1;
  if (fclose(out))
    return 1;
  OK(vkDeviceWaitIdle(dev));
  vkDestroyFence(dev, fence, NULL);
  vkDestroyCommandPool(dev, pool, NULL);
  vkDestroyPipeline(dev, pipe, NULL);
  vkDestroyPipelineLayout(dev, layout, NULL);
  vkDestroyShaderModule(dev, vs, NULL);
  vkDestroyShaderModule(dev, fs, NULL);
  vkDestroyFramebuffer(dev, fb, NULL);
  vkDestroyRenderPass(dev, pass, NULL);
  vkUnmapMemory(dev, host_mem);
  vkDestroyBuffer(dev, buffer, NULL);
  vkFreeMemory(dev, host_mem, NULL);
  vkDestroyImageView(dev, view, NULL);
  vkDestroyImage(dev, image, NULL);
  vkFreeMemory(dev, image_mem, NULL);
  vkDestroyDevice(dev, NULL);
  vkDestroyInstance(inst, NULL);
  printf("VK_PIXEL_PASS frames=3 size=64x64\n");
  return 0;
}
