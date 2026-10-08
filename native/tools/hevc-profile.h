#ifndef VEYNODE_HEVC_PROFILE_H
#define VEYNODE_HEVC_PROFILE_H
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
// Bounded SPS profile_tier_level gate, not a complete HEVC decoder. This
// diagnostic deliberately rejects multilayer and temporal sublayer streams.
// Input is one NAL with its two-byte header, without Annex B start codes.
typedef struct { uint8_t profile_space, profile_id, tier, level; } VeynodeHEVCProfile;
static bool veynode_hevc_main_level4(const uint8_t *nal,size_t length,VeynodeHEVCProfile *out) {
 if(!nal||!out||length<15||length>65536||(nal[0]&0x80)||((nal[0]>>1)&63)!=33||
    (nal[0]&1)||(nal[1]>>3)||(nal[1]&7)!=1)return false;
 uint8_t prefix[13]={0};size_t count=0;unsigned zeros=0;
 // Validate escaping across the entire bounded NAL, retaining only PTL bytes.
 for(size_t i=2;i<length;i++) {
  uint8_t byte=nal[i];
  if(zeros==2) {
   if(byte==3) {if(i+1>=length||nal[i+1]>3)return false;zeros=0;continue;}
   if(byte<3)return false;
  }
  if(count<sizeof(prefix))prefix[count]=byte;
  count++;zeros=byte==0?zeros+1:0;
 }
 if(count<13||(prefix[0]&0x0e)!=0||(prefix[0]&1)==0)return false;
 out->profile_space=prefix[1]>>6;out->tier=(prefix[1]>>5)&1;
 out->profile_id=prefix[1]&31;out->level=prefix[12];
 bool level=out->level==30||out->level==60||out->level==63||out->level==90||out->level==93||out->level==120;
 return out->profile_space==0&&out->profile_id==1&&out->tier==0&&level;
}
#endif
