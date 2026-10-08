// Bounded coordinate diagnostic; output is compared with actual Guest evdev/X11.
#include <spice-client.h>
#include <glib.h>
#include <stdio.h>
static GMainLoop *loop;
static SpiceInputsChannel *inputs;
static SpiceMainChannel *main_channel;
static int tick,sent;
static const int points[][2]={{320,600},{0,0},{1,1},{2,2},{200,200},{639,399},{640,400},{1278,798},{1279,799},{0,799},{1279,0},{320,600}};
static gboolean step(gpointer unused){
 (void)unused;tick++;
 if(tick==2&&main_channel)spice_main_channel_request_mouse_mode(main_channel,SPICE_MOUSE_MODE_CLIENT);
 if(tick>=4&&tick<4+(int)G_N_ELEMENTS(points)&&inputs){
  int i=tick-4;printf("COORD_SENT {\"index\":%d,\"x\":%d,\"y\":%d}\n",i,points[i][0],points[i][1]);fflush(stdout);
  spice_inputs_channel_position(inputs,points[i][0],points[i][1],0,0);sent++;
 }
 if(tick==18){g_main_loop_quit(loop);return G_SOURCE_REMOVE;}
 return G_SOURCE_CONTINUE;
}
static void channel_new(SpiceSession *session,SpiceChannel *channel,gpointer unused){
 (void)session;(void)unused;
 if(SPICE_IS_MAIN_CHANNEL(channel))main_channel=SPICE_MAIN_CHANNEL(channel);
 if(SPICE_IS_INPUTS_CHANNEL(channel)){inputs=SPICE_INPUTS_CHANNEL(channel);spice_channel_connect(channel);}
}
int main(int argc,char **argv){
 if(argc!=3)return 2;loop=g_main_loop_new(NULL,FALSE);SpiceSession *session=spice_session_new();
 g_object_set(session,"unix-path",argv[1],NULL);g_signal_connect(session,"channel-new",G_CALLBACK(channel_new),NULL);
 if(!spice_session_connect(session))return 3;
 g_timeout_add(500,step,NULL);g_main_loop_run(loop);
 spice_session_disconnect(session);g_object_unref(session);g_main_loop_unref(loop);return sent==(int)G_N_ELEMENTS(points)?0:1;
}
