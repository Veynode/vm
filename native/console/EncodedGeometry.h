#ifndef VEYNODE_ENCODED_GEOMETRY_H
#define VEYNODE_ENCODED_GEOMETRY_H
#include <stdbool.h>
#include <stddef.h>
// Supported SPICE source bounds, with even coded dimensions and no upscaling.
// Integer aspect-fit avoids floating-point rounding at an exact codec boundary.
// This selects geometry only; actual hardware/profile validation remains required.
// H.264 is Constrained Baseline level 3.1 (1280x720) unless the consumer asked for 4.0 (1920x1080),
// which only a WebCodecs desktop viewer does: the WebRTC console negotiates profile-level-id 42e01f.
static inline bool VNEncodedGeometryForLevel(size_t sourceWidth,size_t sourceHeight,bool hevc,int h264Level,size_t *width,size_t *height){
    if(!width||!height)return false;
    *width=*height=0;
    if(sourceWidth<2||sourceHeight<2||sourceWidth>3840||sourceHeight>2160||(sourceWidth&1)||(sourceHeight&1))return false;
    size_t maxWidth=hevc||h264Level==40?1920:1280,maxHeight=hevc||h264Level==40?1080:720;
    size_t w=sourceWidth,h=sourceHeight;
    if(w>maxWidth||h>maxHeight){
        if(sourceWidth*maxHeight>sourceHeight*maxWidth){w=maxWidth;h=(sourceHeight*maxWidth/sourceWidth)&~(size_t)1;}
        else{h=maxHeight;w=(sourceWidth*maxHeight/sourceHeight)&~(size_t)1;}
    }
    if(w<2||h<2||w>maxWidth||h>maxHeight)return false;
    *width=w;*height=h;return true;
}
static inline bool VNEncodedGeometry(size_t sourceWidth,size_t sourceHeight,bool hevc,size_t *width,size_t *height){
    return VNEncodedGeometryForLevel(sourceWidth,sourceHeight,hevc,31,width,height);
}
#endif
