#!/usr/bin/env python3
"""Apply project-owned Darwin FIFO generation fix only to the isolated server source."""
from pathlib import Path
import shutil,difflib
ROOT=Path(__file__).resolve().parents[1];BASE=ROOT/'.local/spice-reconnect-build';SRC=BASE/'spice-0.14.3';marker=SRC/'.veynode-reconnect-fixed'
if marker.exists():raise SystemExit('Fix already applied; retained isolated source')
changes={}
def edit(name,old,new):
 p=SRC/name;s=p.read_text();changes.setdefault(name,s)
 if s.count(old)!=1:raise RuntimeError(f'Expected exact one upstream anchor: {name}: {old[:80]}')
 p.write_text(s.replace(old,new))
for name in ['spice-scanout-generation.c','spice-scanout-generation.h']:shutil.copyfile(ROOT/'native/tools'/name,SRC/'server'/name)
edit('server/meson.build',"  'agent-msg-filter.c',","  'agent-msg-filter.c',\n  'spice-scanout-generation.c',")
edit('meson.build',"# TCP_KEEPIDLE definition in netinet/tcp.h","spice_server_deps += dependency('appleframeworks', modules : ['IOSurface', 'CoreFoundation'])\n\n# TCP_KEEPIDLE definition in netinet/tcp.h")
edit('server/red-qxl.c','#include "red-qxl.h"','#include "red-qxl.h"\n#include "spice-scanout-generation.h"\n#include <poll.h>')
edit('server/red-qxl.c','    SpiceMsgDisplayGlScanoutUnix scanout;','    SpiceMsgDisplayGlScanoutUnix scanout;\n    VeynodeScanoutGeneration *scanout_generation;\n    SpiceWatch *scanout_generation_watch;')
helpers='''/* Called under the QXL scanout mutex. Core watch registration/removal and
 * QXL replacement/destruction run on the QEMU main thread. Worker publication
 * and disconnect share the same mutex and never register or remove watches. */
static void red_qxl_clear_scanout_generation(QXLState *state)
{
    if (state->scanout_generation_watch) {
        red_watch_remove(state->scanout_generation_watch);
        state->scanout_generation_watch = NULL;
    }
    veynode_scanout_generation_free(state->scanout_generation);
    state->scanout_generation = NULL;
}

static void red_qxl_scanout_producer_event(int fd, int event, void *opaque)
{
    QXLState *state = opaque;
    SpiceWatch *remove = NULL;
    struct pollfd p = { .fd = fd, .events = POLLIN };
    pthread_mutex_lock(&state->scanout_mutex);
    if (fd == state->scanout.drm_dma_buf_fd &&
        (poll(&p, 1, 0) < 0 || (p.revents & (POLLHUP | POLLERR | POLLNVAL)))) {
        veynode_scanout_generation_invalidate(state->scanout_generation);
        remove = state->scanout_generation_watch;
        state->scanout_generation_watch = NULL;
    }
    pthread_mutex_unlock(&state->scanout_mutex);
    if (remove) {
        red_watch_remove(remove);
    }
}

/* Caller holds the mutex through red_qxl_get_gl_scanout. */
int red_qxl_client_scanout_fd(QXLInstance *qxl, const void *client)
{
    return veynode_scanout_generation_publish(qxl->st->scanout_generation, client);
}

void red_qxl_release_scanout_client(QXLInstance *qxl, const void *client)
{
    if (!qxl || !qxl->st) {
        return;
    }
    pthread_mutex_lock(&qxl->st->scanout_mutex);
    veynode_scanout_generation_release_client(qxl->st->scanout_generation, client);
    pthread_mutex_unlock(&qxl->st->scanout_mutex);
}

'''
edit('server/red-qxl.c','SpiceMsgDisplayGlScanoutUnix *red_qxl_get_gl_scanout(QXLInstance *qxl)',helpers+'SpiceMsgDisplayGlScanoutUnix *red_qxl_get_gl_scanout(QXLInstance *qxl)')
edit('server/red-qxl.c','    if (qxl_state->scanout.drm_dma_buf_fd >= 0) {\n        close(qxl_state->scanout.drm_dma_buf_fd);\n    }','''    red_qxl_clear_scanout_generation(qxl_state);
    if (qxl_state->scanout.drm_dma_buf_fd >= 0) {
        close(qxl_state->scanout.drm_dma_buf_fd);
    }
    if (fd >= 0) {
        qxl_state->scanout_generation = veynode_scanout_generation_new(fd, width, height);
        if (qxl_state->scanout_generation) {
            qxl_state->scanout_generation_watch = reds_core_watch_add(qxl_state->reds,
                fd, SPICE_WATCH_EVENT_READ, red_qxl_scanout_producer_event, qxl_state);
        }
        if (!qxl_state->scanout_generation || !qxl_state->scanout_generation_watch) {
            spice_warning("Darwin scanout generation rejected; no usable live producer ID");
            red_qxl_clear_scanout_generation(qxl_state);
            close(fd);
            fd = -1;
        }
    }''')
edit('server/red-qxl.c','    pthread_mutex_destroy(&qxl_state->scanout_mutex);','''    red_qxl_clear_scanout_generation(qxl_state);
    if (qxl_state->scanout.drm_dma_buf_fd >= 0) {
        close(qxl_state->scanout.drm_dma_buf_fd);
    }
    pthread_mutex_destroy(&qxl_state->scanout_mutex);''')
edit('server/red-qxl.h','SpiceMsgDisplayGlScanoutUnix *red_qxl_get_gl_scanout(QXLInstance *qxl);','''SpiceMsgDisplayGlScanoutUnix *red_qxl_get_gl_scanout(QXLInstance *qxl);
int red_qxl_client_scanout_fd(QXLInstance *qxl, const void *client);
void red_qxl_release_scanout_client(QXLInstance *qxl, const void *client);''')
edit('server/dcc-send.c','''    if (scanout != NULL) {
        red_channel_client_init_send_data(rcc, SPICE_MSG_DISPLAY_GL_SCANOUT_UNIX);
        spice_marshall_msg_display_gl_scanout_unix(m, scanout);
    }
    red_qxl_put_gl_scanout(qxl, scanout);''','''    bool failed = false;
    if (scanout != NULL) {
        int fd = red_qxl_client_scanout_fd(qxl, rcc);
        if (fd < 0) {
            spice_warning("Darwin client scanout FD publication failed: %s", strerror(errno));
            failed = true;
        } else {
            SpiceMsgDisplayGlScanoutUnix client_scanout = *scanout;
            client_scanout.drm_dma_buf_fd = fd;
            red_channel_client_init_send_data(rcc, SPICE_MSG_DISPLAY_GL_SCANOUT_UNIX);
            spice_marshall_msg_display_gl_scanout_unix(m, &client_scanout);
            close(fd); /* marshaller owns its duplicate */
        }
    }
    red_qxl_put_gl_scanout(qxl, scanout);
    if (failed) {
        /* Disconnect only after unlocking: its cleanup acquires the same mutex. */
        red_channel_client_disconnect(rcc);
    }''')
edit('server/dcc.c','#include "dcc-private.h"','#include "dcc-private.h"\n#include "red-qxl.h"')
edit('server/dcc.c','    dcc_stop(dcc); // TODO: start/stop -> connect/disconnect?','    red_qxl_release_scanout_client(display->priv->qxl, rcc);\n    dcc_stop(dcc); // TODO: start/stop -> connect/disconnect?')
patch=''.join(''.join(difflib.unified_diff(old.splitlines(True),(SRC/name).read_text().splitlines(True),fromfile='a/'+name,tofile='b/'+name,n=0)) for name,old in changes.items())
(ROOT/'archive/docs-evidence/apple-spice-reconnect-fresh-client.patch').write_text(patch)
marker.touch();print('Applied only to '+str(SRC))
