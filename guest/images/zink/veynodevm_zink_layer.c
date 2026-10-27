/* Implicit Vulkan layer that works around MoltenVK 1.4.2 shader-translation
 * bugs Zink (OpenGL on Vulkan) hits on Venus. It acts only on instances whose
 * engine is Mesa's Zink; other Vulkan applications see the device unchanged
 * (DXVK, for one, requires robustBufferAccess).
 *
 * - robustBufferAccess/robustBufferAccess2 are reported unsupported. With them
 *   enabled MoltenVK bounds-checks buffer accesses, and for Zink's arrayed
 *   uniform buffers (`ubos[]`) it emits `size / 4u` on the array of sizes,
 *   which Metal refuses ("invalid operands to binary expression ('const
 *   constant uint *' and 'unsigned int')"): every draw reading a uniform fails.
 * - geometryShader is reported unsupported. Venus does not pass
 *   VK_EXT_provoking_vertex through, so Zink would wrap every draw in a
 *   generated geometry shader for GL's last-vertex convention, and MoltenVK's
 *   mesh-shader emulation of it does not compile ("redefinition of parameter
 *   'gl_PrimitiveID'"). Zink's GL version is already capped at 3.1 by the
 *   missing VK_EXT_depth_clip_enable, which GL 3.2 needs alongside geometry
 *   shaders. Flat-shaded varyings take the first vertex instead of the last.
 * - VK_EXT_host_image_copy is hidden. Zink adds HOST_TRANSFER usage to every
 *   image whose format allows it, and MoltenVK cannot create depth images with
 *   it, so every depth texture fails with GL_OUT_OF_MEMORY (Zink only knows to
 *   drop the usage when it is built for macOS itself).
 * - OpName/OpMemberName are stripped from SPIR-V. MoltenVK keeps debug names
 *   as MSL identifiers, and Gallium's internal blit shaders name a texture
 *   `sampler`, which hides Metal's sampler type ("must use 'struct' tag to
 *   refer to type 'sampler'").
 * Build: cc -O2 -shared -fPIC -o libVkLayer_veynodevm_zink.so this.c
 */
#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <vulkan/vk_layer.h>
#include <vulkan/vulkan.h>

#define EXPORT __attribute__((visibility("default")))

/* Loader dispatch pointer: instances share it with their physical devices,
 * devices with their queues and command buffers. */
static void *key(const void *handle) { return *(void *const *)handle; }

struct instance {
  void *key;
  VkInstance handle;
  PFN_vkGetInstanceProcAddr gipa;
  PFN_vkGetPhysicalDeviceFeatures features;
  PFN_vkGetPhysicalDeviceFeatures2 features2;
  PFN_vkGetPhysicalDeviceFeatures2 features2_khr;
  PFN_vkEnumerateDeviceExtensionProperties extensions;
  int zink;
  struct instance *next;
};
struct device {
  void *key;
  PFN_vkGetDeviceProcAddr gdpa;
  PFN_vkCreateShaderModule create_shader_module;
  PFN_vkCreateGraphicsPipelines create_graphics_pipelines;
  PFN_vkCreateComputePipelines create_compute_pipelines;
  int zink;
  struct device *next;
};
static struct instance *instances;
static struct device *devices;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;

static struct instance *find(const void *handle) {
  pthread_mutex_lock(&lock);
  struct instance *i = instances;
  while (i && i->key != key(handle))
    i = i->next;
  pthread_mutex_unlock(&lock);
  return i;
}

/* Devices stay registered for the life of the process. */
static struct device *find_device(const void *handle) {
  pthread_mutex_lock(&lock);
  struct device *d = devices;
  while (d && d->key != key(handle))
    d = d->next;
  pthread_mutex_unlock(&lock);
  return d;
}

static VkResult VKAPI_CALL create_instance(const VkInstanceCreateInfo *info,
                                           const VkAllocationCallbacks *alloc,
                                           VkInstance *out) {
  VkLayerInstanceCreateInfo *chain = (VkLayerInstanceCreateInfo *)info->pNext;
  while (chain && !(chain->sType == VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO &&
                    chain->function == VK_LAYER_LINK_INFO))
    chain = (VkLayerInstanceCreateInfo *)chain->pNext;
  if (!chain)
    return VK_ERROR_INITIALIZATION_FAILED;
  PFN_vkGetInstanceProcAddr gipa = chain->u.pLayerInfo->pfnNextGetInstanceProcAddr;
  chain->u.pLayerInfo = chain->u.pLayerInfo->pNext;
  PFN_vkCreateInstance next =
      (PFN_vkCreateInstance)gipa(VK_NULL_HANDLE, "vkCreateInstance");
  VkResult result = next(info, alloc, out);
  if (result != VK_SUCCESS)
    return result;
  struct instance *i = calloc(1, sizeof *i);
  if (!i)
    return VK_SUCCESS;
  i->key = key(*out);
  i->handle = *out;
  i->gipa = gipa;
  i->features = (PFN_vkGetPhysicalDeviceFeatures)gipa(*out, "vkGetPhysicalDeviceFeatures");
  i->features2 = (PFN_vkGetPhysicalDeviceFeatures2)gipa(*out, "vkGetPhysicalDeviceFeatures2");
  i->features2_khr =
      (PFN_vkGetPhysicalDeviceFeatures2)gipa(*out, "vkGetPhysicalDeviceFeatures2KHR");
  i->extensions = (PFN_vkEnumerateDeviceExtensionProperties)gipa(
      *out, "vkEnumerateDeviceExtensionProperties");
  const VkApplicationInfo *app = info->pApplicationInfo;
  i->zink = app && app->pEngineName && !strcmp(app->pEngineName, "mesa zink");
  pthread_mutex_lock(&lock);
  i->next = instances;
  instances = i;
  pthread_mutex_unlock(&lock);
  return VK_SUCCESS;
}

static void VKAPI_CALL destroy_instance(VkInstance instance,
                                        const VkAllocationCallbacks *alloc) {
  pthread_mutex_lock(&lock);
  struct instance **p = &instances, *i = NULL;
  while (*p && (*p)->key != key(instance))
    p = &(*p)->next;
  if (*p) {
    i = *p;
    *p = i->next;
  }
  pthread_mutex_unlock(&lock);
  if (!i)
    return;
  PFN_vkDestroyInstance next =
      (PFN_vkDestroyInstance)i->gipa(instance, "vkDestroyInstance");
  free(i);
  next(instance, alloc);
}

static void mask(const struct instance *i, VkPhysicalDeviceFeatures2 *f) {
  if (!i->zink)
    return;
  f->features.robustBufferAccess = VK_FALSE;
  f->features.geometryShader = VK_FALSE;
  for (VkBaseOutStructure *s = f->pNext; s; s = s->pNext)
    if (s->sType == VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_ROBUSTNESS_2_FEATURES_EXT)
      ((VkPhysicalDeviceRobustness2FeaturesEXT *)s)->robustBufferAccess2 = VK_FALSE;
    else if (s->sType == VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_HOST_IMAGE_COPY_FEATURES_EXT)
      ((VkPhysicalDeviceHostImageCopyFeaturesEXT *)s)->hostImageCopy = VK_FALSE;
}

static VkResult VKAPI_CALL extensions(VkPhysicalDevice pd, const char *layer,
                                      uint32_t *count, VkExtensionProperties *out) {
  struct instance *i = find(pd);
  if (!i->zink || layer)
    return i->extensions(pd, layer, count, out);
  uint32_t total = 0;
  VkResult result = i->extensions(pd, NULL, &total, NULL);
  VkExtensionProperties *all = result == VK_SUCCESS ? calloc(total + 1, sizeof *all) : NULL;
  if (!all)
    return result == VK_SUCCESS ? VK_ERROR_OUT_OF_HOST_MEMORY : result;
  result = i->extensions(pd, NULL, &total, all);
  uint32_t kept = 0;
  for (uint32_t n = 0; result >= VK_SUCCESS && n < total; n++)
    if (strcmp(all[n].extensionName, VK_EXT_HOST_IMAGE_COPY_EXTENSION_NAME))
      all[kept++] = all[n];
  if (result >= VK_SUCCESS) {
    result = VK_SUCCESS;
    if (!out) {
      *count = kept;
    } else {
      if (*count < kept)
        result = VK_INCOMPLETE;
      else
        *count = kept;
      memcpy(out, all, *count * sizeof *all);
    }
  }
  free(all);
  return result;
}

static void VKAPI_CALL features(VkPhysicalDevice pd, VkPhysicalDeviceFeatures *f) {
  struct instance *i = find(pd);
  i->features(pd, f);
  if (i->zink) {
    f->robustBufferAccess = VK_FALSE;
    f->geometryShader = VK_FALSE;
  }
}

static void VKAPI_CALL features2(VkPhysicalDevice pd, VkPhysicalDeviceFeatures2 *f) {
  struct instance *i = find(pd);
  i->features2(pd, f);
  mask(i, f);
}

static void VKAPI_CALL features2_khr(VkPhysicalDevice pd,
                                     VkPhysicalDeviceFeatures2 *f) {
  struct instance *i = find(pd);
  i->features2_khr(pd, f);
  mask(i, f);
}

static VkResult VKAPI_CALL create_device(VkPhysicalDevice pd,
                                         const VkDeviceCreateInfo *info,
                                         const VkAllocationCallbacks *alloc,
                                         VkDevice *out) {
  VkLayerDeviceCreateInfo *chain = (VkLayerDeviceCreateInfo *)info->pNext;
  while (chain && !(chain->sType == VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO &&
                    chain->function == VK_LAYER_LINK_INFO))
    chain = (VkLayerDeviceCreateInfo *)chain->pNext;
  if (!chain)
    return VK_ERROR_INITIALIZATION_FAILED;
  struct instance *i = find(pd);
  PFN_vkGetInstanceProcAddr gipa = chain->u.pLayerInfo->pfnNextGetInstanceProcAddr;
  PFN_vkGetDeviceProcAddr gdpa = chain->u.pLayerInfo->pfnNextGetDeviceProcAddr;
  chain->u.pLayerInfo = chain->u.pLayerInfo->pNext;
  PFN_vkCreateDevice next = (PFN_vkCreateDevice)gipa(i->handle, "vkCreateDevice");
  VkResult result = next(pd, info, alloc, out);
  struct device *d = result == VK_SUCCESS ? calloc(1, sizeof *d) : NULL;
  if (!d)
    return result;
  d->key = key(*out);
  d->gdpa = gdpa;
  d->zink = i->zink;
  d->create_shader_module = (PFN_vkCreateShaderModule)gdpa(*out, "vkCreateShaderModule");
  d->create_graphics_pipelines =
      (PFN_vkCreateGraphicsPipelines)gdpa(*out, "vkCreateGraphicsPipelines");
  d->create_compute_pipelines =
      (PFN_vkCreateComputePipelines)gdpa(*out, "vkCreateComputePipelines");
  pthread_mutex_lock(&lock);
  d->next = devices;
  devices = d;
  pthread_mutex_unlock(&lock);
  return result;
}

/* Copy of SPIR-V without OpName (5) and OpMemberName (6); NULL on bad input
 * or allocation failure, in which case the caller passes the original. */
static uint32_t *strip_names(const uint32_t *code, size_t size, size_t *out_size) {
  size_t words = size / 4;
  if (words < 5 || code[0] != 0x07230203)
    return NULL;
  uint32_t *out = malloc(size);
  if (!out)
    return NULL;
  memcpy(out, code, 5 * 4);
  size_t n = 5;
  for (size_t at = 5; at < words;) {
    uint32_t count = code[at] >> 16, op = code[at] & 0xffff;
    if (!count || at + count > words) {
      free(out);
      return NULL;
    }
    if (op != 5 && op != 6) {
      memcpy(out + n, code + at, count * 4);
      n += count;
    }
    at += count;
  }
  *out_size = n * 4;
  return out;
}

static VkResult VKAPI_CALL create_shader_module(VkDevice device,
                                                const VkShaderModuleCreateInfo *info,
                                                const VkAllocationCallbacks *alloc,
                                                VkShaderModule *out) {
  struct device *d = find_device(device);
  size_t size;
  uint32_t *code = d->zink ? strip_names(info->pCode, info->codeSize, &size) : NULL;
  if (!code)
    return d->create_shader_module(device, info, alloc, out);
  VkShaderModuleCreateInfo copy = *info;
  copy.pCode = code;
  copy.codeSize = size;
  VkResult result = d->create_shader_module(device, &copy, alloc, out);
  free(code);
  return result;
}

/* Zink can pass SPIR-V inline (a VkShaderModuleCreateInfo chained to a stage
 * without a module). Those structs are its own short-lived copies, so their
 * code is swapped for the stripped copy during the call and restored after. */
struct inline_code {
  VkShaderModuleCreateInfo *info;
  const uint32_t *code;
  size_t size;
};

static unsigned strip_stage(const VkPipelineShaderStageCreateInfo *stage,
                            struct inline_code *saved) {
  if (stage->module != VK_NULL_HANDLE)
    return 0;
  for (const VkBaseInStructure *s = stage->pNext; s; s = s->pNext) {
    if (s->sType != VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO)
      continue;
    VkShaderModuleCreateInfo *info = (VkShaderModuleCreateInfo *)s;
    size_t size;
    uint32_t *code = strip_names(info->pCode, info->codeSize, &size);
    if (!code)
      return 0;
    *saved = (struct inline_code){info, info->pCode, info->codeSize};
    info->pCode = code;
    info->codeSize = size;
    return 1;
  }
  return 0;
}

static void restore(struct inline_code *saved, unsigned count) {
  for (unsigned n = 0; n < count; n++) {
    free((void *)saved[n].info->pCode);
    saved[n].info->pCode = saved[n].code;
    saved[n].info->codeSize = saved[n].size;
  }
  free(saved);
}

static VkResult VKAPI_CALL create_graphics_pipelines(
    VkDevice device, VkPipelineCache cache, uint32_t count,
    const VkGraphicsPipelineCreateInfo *infos, const VkAllocationCallbacks *alloc,
    VkPipeline *out) {
  struct device *d = find_device(device);
  unsigned stages = 0, saved_count = 0;
  for (uint32_t p = 0; p < count; p++)
    stages += infos[p].stageCount;
  struct inline_code *saved =
      d->zink && stages ? calloc(stages, sizeof *saved) : NULL;
  if (saved)
    for (uint32_t p = 0; p < count; p++)
      for (uint32_t s = 0; s < infos[p].stageCount; s++)
        saved_count += strip_stage(&infos[p].pStages[s], &saved[saved_count]);
  VkResult result = d->create_graphics_pipelines(device, cache, count, infos, alloc, out);
  if (saved)
    restore(saved, saved_count);
  return result;
}

static VkResult VKAPI_CALL create_compute_pipelines(
    VkDevice device, VkPipelineCache cache, uint32_t count,
    const VkComputePipelineCreateInfo *infos, const VkAllocationCallbacks *alloc,
    VkPipeline *out) {
  struct device *d = find_device(device);
  unsigned saved_count = 0;
  struct inline_code *saved = d->zink && count ? calloc(count, sizeof *saved) : NULL;
  if (saved)
    for (uint32_t p = 0; p < count; p++)
      saved_count += strip_stage(&infos[p].stage, &saved[saved_count]);
  VkResult result = d->create_compute_pipelines(device, cache, count, infos, alloc, out);
  if (saved)
    restore(saved, saved_count);
  return result;
}

static PFN_vkVoidFunction device_hook(const char *name) {
  static const struct {
    const char *name;
    PFN_vkVoidFunction fn;
  } hooks[] = {
      {"vkCreateShaderModule", (PFN_vkVoidFunction)create_shader_module},
      {"vkCreateGraphicsPipelines", (PFN_vkVoidFunction)create_graphics_pipelines},
      {"vkCreateComputePipelines", (PFN_vkVoidFunction)create_compute_pipelines},
  };
  for (size_t n = 0; n < sizeof hooks / sizeof hooks[0]; n++)
    if (!strcmp(name, hooks[n].name))
      return hooks[n].fn;
  return NULL;
}

EXPORT VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL
veynodevm_zink_GetDeviceProcAddr(VkDevice device, const char *name) {
  if (!strcmp(name, "vkGetDeviceProcAddr"))
    return (PFN_vkVoidFunction)veynodevm_zink_GetDeviceProcAddr;
  struct device *d = find_device(device);
  if (!d)
    return NULL;
  PFN_vkVoidFunction hook = d->zink ? device_hook(name) : NULL;
  return hook ? hook : d->gdpa(device, name);
}

EXPORT VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL
veynodevm_zink_GetInstanceProcAddr(VkInstance instance, const char *name) {
  static const struct {
    const char *name;
    PFN_vkVoidFunction fn;
  } hooks[] = {
      {"vkGetInstanceProcAddr", (PFN_vkVoidFunction)veynodevm_zink_GetInstanceProcAddr},
      {"vkGetDeviceProcAddr", (PFN_vkVoidFunction)veynodevm_zink_GetDeviceProcAddr},
      {"vkCreateInstance", (PFN_vkVoidFunction)create_instance},
      {"vkDestroyInstance", (PFN_vkVoidFunction)destroy_instance},
      {"vkCreateDevice", (PFN_vkVoidFunction)create_device},
      {"vkGetPhysicalDeviceFeatures", (PFN_vkVoidFunction)features},
      {"vkGetPhysicalDeviceFeatures2", (PFN_vkVoidFunction)features2},
      {"vkGetPhysicalDeviceFeatures2KHR", (PFN_vkVoidFunction)features2_khr},
      {"vkEnumerateDeviceExtensionProperties", (PFN_vkVoidFunction)extensions},
  };
  for (size_t n = 0; n < sizeof hooks / sizeof hooks[0]; n++)
    if (!strcmp(name, hooks[n].name))
      return hooks[n].fn;
  struct instance *i = instance ? find(instance) : NULL;
  if (i && i->zink) {
    PFN_vkVoidFunction hook = device_hook(name);
    if (hook)
      return hook;
  }
  return i ? i->gipa(instance, name) : NULL;
}
