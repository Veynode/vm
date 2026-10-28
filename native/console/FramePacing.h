#ifndef VEYNODE_FRAME_PACING_H
#define VEYNODE_FRAME_PACING_H
#include <stdbool.h>
#include <stdint.h>
// Monotonic microseconds, rational deadline cadence at the caller's rate: the
// refresh of the screen that shows the frames (the native window's screen, or
// what a browser viewer reports with `fps N`), at most 240 Hz, the fastest Mac
// display; 30 Hz while nothing shows them. A draw may claim its slot a quarter
// slot (at most 3 ms) early: when copying and encoding a frame take nearly a
// slot, a draw just before the boundary would otherwise be refused and the
// next one land past the following slot, losing frames. No catch-up queue: a
// delayed draw advances directly to the next future slot. The caller may drop
// this admitted frame on pool exhaustion without extending the queue.
#define VN_VISIBLE_FPS 240
#define VN_BACKGROUND_FPS 30
typedef struct { int64_t anchor,next; uint64_t ordinal; unsigned fps; bool initialized; } VNFramePacing;
static inline bool VNFramePacingAdmit(VNFramePacing *p,int64_t now,unsigned fps){
    if(fps<1)fps=1;
    if(fps>VN_VISIBLE_FPS)fps=VN_VISIBLE_FPS;
    if(!p->initialized||p->fps!=fps||now<p->anchor){
        *p=(VNFramePacing){.anchor=now,.next=now+1000000/fps,.ordinal=1,.fps=fps,.initialized=true};return true;
    }
    int64_t early=1000000/fps/4;if(early>3000)early=3000;
    if(now<p->next-early)return false;
    // The slot after the one this draw claims: the claimed slot itself when early.
    uint64_t elapsed=(uint64_t)((now>p->next?now:p->next)-p->anchor);
    // First future rational slot. Using division here avoids accumulated
    // integer-microsecond rounding drift at any rate.
    uint64_t nextOrdinal=((elapsed+1)*fps+999999)/1000000;
    if(nextOrdinal<=p->ordinal)nextOrdinal=p->ordinal+1;
    p->ordinal=nextOrdinal;p->next=p->anchor+(int64_t)(nextOrdinal*1000000/fps);return true;
}
#endif
