// Bounded diagnostic only. Guest event/pixel assertions determine success;
// sending these messages alone does not prove that a Guest received input.
#include <spice-client.h>
#include <glib.h>
#include <stdio.h>
static GMainLoop *loop;
static SpiceInputsChannel *inputs;
static SpiceMainChannel *main_channel;
static int tick, sent;
static gboolean step(gpointer unused) {
    (void)unused;
    tick++;
    if (tick == 2 && main_channel)
        spice_main_channel_request_mouse_mode(main_channel, SPICE_MOUSE_MODE_CLIENT);
    if (tick == 4 && inputs) {
        int mode=0;
        if(main_channel)g_object_get(main_channel,"mouse-mode",&mode,NULL);
        printf("INPUT_SEND mode=%d x=200 y=200 display=0 scancode=30\n",mode);
        spice_inputs_channel_position(inputs,200,200,0,0);
        spice_inputs_channel_button_press(inputs,SPICE_MOUSE_BUTTON_LEFT,SPICE_MOUSE_BUTTON_MASK_LEFT);
        spice_inputs_channel_button_release(inputs,SPICE_MOUSE_BUTTON_LEFT,0);
        sent=1;
    }
    if(tick == 5 && inputs) {
        spice_inputs_channel_key_press(inputs,0x1e);
        spice_inputs_channel_key_release(inputs,0x1e);
    }
    if(tick == 8){g_main_loop_quit(loop);return G_SOURCE_REMOVE;}
    return G_SOURCE_CONTINUE;
}
static void channel_new(SpiceSession *session,SpiceChannel *channel,gpointer unused) {
    (void)session;(void)unused;
    if(SPICE_IS_MAIN_CHANNEL(channel))main_channel=SPICE_MAIN_CHANNEL(channel);
    if(SPICE_IS_INPUTS_CHANNEL(channel)){
        inputs=SPICE_INPUTS_CHANNEL(channel);spice_channel_connect(channel);
    }
}
int main(int argc,char **argv) {
    if(argc!=3){fprintf(stderr,"usage: spice-input-probe UNIX_SOCKET OUTPUT_PREFIX\n");return 2;}
    loop=g_main_loop_new(NULL,FALSE);
    SpiceSession *session=spice_session_new();
    g_object_set(session,"unix-path",argv[1],NULL);
    g_signal_connect(session,"channel-new",G_CALLBACK(channel_new),NULL);
    if(!spice_session_connect(session))return 3;
    g_timeout_add(1000,step,NULL);g_main_loop_run(loop);
    spice_session_disconnect(session);g_object_unref(session);g_main_loop_unref(loop);
    return sent?0:1;
}
