#ifndef VEYNODE_FRAME_PACING_H
#define VEYNODE_FRAME_PACING_H
#include <stdbool.h>
#include <stdint.h>
// Monotonic microseconds, rational 60/30 Hz deadline cadence. No catch-up
// queue: a delayed draw advances directly to the next future slot. The caller
// may drop this admitted frame on pool exhaustion without extending the queue.
typedef struct { int64_t anchor,next; uint64_t ordinal; unsigned fps; bool initialized; } VNFramePacing;
static inline bool VNFramePacingAdmit(VNFramePacing *p,int64_t now,bool visible){
    unsigned fps=visible?60:30;
    if(!p->initialized||p->fps!=fps||now<p->anchor){
        *p=(VNFramePacing){.anchor=now,.next=now+1000000/fps,.ordinal=1,.fps=fps,.initialized=true};return true;
    }
    if(now<p->next)return false;
    uint64_t elapsed=(uint64_t)(now-p->anchor);
    // First future rational slot. Using division here avoids accumulated
    // integer-microsecond rounding drift at both 60 Hz and 30 Hz.
    uint64_t nextOrdinal=((elapsed+1)*fps+999999)/1000000;
    if(nextOrdinal<=p->ordinal)nextOrdinal=p->ordinal+1;
    p->ordinal=nextOrdinal;p->next=p->anchor+(int64_t)(nextOrdinal*1000000/fps);return true;
}
#endif
