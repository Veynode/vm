#include "../tools/hevc-profile.h"
#include <assert.h>
#include <stdio.h>
#include <string.h>
// Actual VideoToolbox Main level4 SPS, captured from real SPICE Guest frames.
static const uint8_t actual[]={0x42,0x01,0x01,0x01,0x60,0,0,3,0,0xb0,0,0,3,0,0,3,0,0x78,0xa0,0x02,0x80,0x80,0x32,0x16,0x20,0x5e,0xe4,0x59,0x14,0xbf,0xf2,0xe7,0xf1,0x3f,0xa2};
int main(void){VeynodeHEVCProfile p;uint8_t b[sizeof(actual)+4];
 assert(veynode_hevc_main_level4(actual,sizeof(actual),&p));assert(p.profile_id==1&&p.tier==0&&p.level==120);
 for(size_t n=0;n<18;n++)assert(!veynode_hevc_main_level4(actual,n,&p));
 memcpy(b,actual,sizeof(actual));b[3]=2;assert(!veynode_hevc_main_level4(b,sizeof(actual),&p)); // Main10
 b[3]=0x21;assert(!veynode_hevc_main_level4(b,sizeof(actual),&p)); // High tier
 b[3]=0x41;assert(!veynode_hevc_main_level4(b,sizeof(actual),&p)); // Other profile space
 b[3]=1;b[17]=123;assert(!veynode_hevc_main_level4(b,sizeof(actual),&p)); // level4.1
 b[17]=0;assert(!veynode_hevc_main_level4(b,sizeof(actual),&p));
 b[17]=93;assert(veynode_hevc_main_level4(b,sizeof(actual),&p));
 memcpy(b,actual,sizeof(actual));b[2]=3;assert(!veynode_hevc_main_level4(b,sizeof(actual),&p)); // sublayers
 memcpy(b,actual,sizeof(actual));b[8]=4;assert(!veynode_hevc_main_level4(b,sizeof(actual),&p)); // malformed escape
 memcpy(b,actual,sizeof(actual));b[7]=0;assert(!veynode_hevc_main_level4(b,sizeof(actual),&p)); // missing escape
 memcpy(b,actual,sizeof(actual));b[0]|=128;assert(!veynode_hevc_main_level4(b,sizeof(actual),&p));
 memcpy(b,actual,sizeof(actual));b[1]=9;assert(!veynode_hevc_main_level4(b,sizeof(actual),&p)); // extra layer
 memcpy(b,actual,sizeof(actual));b[sizeof(actual)]=0;b[sizeof(actual)+1]=0;b[sizeof(actual)+2]=3;
 assert(!veynode_hevc_main_level4(b,sizeof(actual)+3,&p)); // dangling prevention byte
 puts("HEVC actual SPS and malformed/profile/tier/level boundaries passed");return 0;}
