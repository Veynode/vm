#ifndef VEYNODE_SPICE_SCANOUT_GENERATION_H
#define VEYNODE_SPICE_SCANOUT_GENERATION_H
#include <stdint.h>
/* All calls require the caller's scanout mutex. source_fd is borrowed and must
 * stay open until free; only the producer holds its write end. This module is
 * for the verified Darwin FIFO/IOSurface transport, never a generic DMA-BUF. */
typedef struct VeynodeScanoutGeneration VeynodeScanoutGeneration;
VeynodeScanoutGeneration *veynode_scanout_generation_new(int source_fd,uint32_t width,uint32_t height);
int veynode_scanout_generation_publish(VeynodeScanoutGeneration *,const void *client);
void veynode_scanout_generation_release_client(VeynodeScanoutGeneration *,const void *client);
void veynode_scanout_generation_invalidate(VeynodeScanoutGeneration *);
void veynode_scanout_generation_free(VeynodeScanoutGeneration *);
#endif
