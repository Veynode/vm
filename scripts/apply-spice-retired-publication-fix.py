#!/usr/bin/env python3
"""Retired FIFO generation is not a client failure; patch isolated SPICE only."""
import difflib,pathlib
ROOT=pathlib.Path(__file__).resolve().parents[1];SOURCE=ROOT/'.local/spice-reconnect-build/spice-0.14.3/server/dcc-send.c'
if not (SOURCE.parent.parent/'.veynode-reconnect-fixed').is_file():raise SystemExit('Apply the fresh-client generation fix first')
old='''        if (fd < 0) {
            spice_warning("Darwin client scanout FD publication failed: %s", strerror(errno));
            failed = true;
        } else {'''
new='''        if (fd < 0) {
            /* The producer can retire a generation before this queued item is
             * marshalled. publish() has invalidated every old client pipe;
             * ESTALE supplies no FD/ID and is not a transport failure. Leave
             * send_data empty, like the scanout == NULL branch, and await the
             * next scanout. Other publication failures still disconnect. */
            if (errno != ESTALE) {
                spice_warning("Darwin client scanout FD publication failed: %s", strerror(errno));
                failed = true;
            }
        } else {'''
text=SOURCE.read_text()
if text.count(old)==1:
 changed=text.replace(old,new);SOURCE.write_text(changed)
 patch=''.join(difflib.unified_diff(text.splitlines(True),changed.splitlines(True),fromfile='a/server/dcc-send.c',tofile='b/server/dcc-send.c',n=0))
 (ROOT/'docs/evidence/apple-spice-retired-publication.patch').write_text(patch)
 print('Applied retired-publication fix to isolated SPICE source')
elif text.count(new)==1:print('Retired-publication fix already present')
else:raise SystemExit('Unexpected isolated SPICE source; refusing ambiguous edit')
