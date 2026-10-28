#import <Foundation/Foundation.h>

// The native window's sound and clipboard (Config::audio, the Guest's
// spice-vdagent). Sound plays while the window is open and not minimized; the
// microphone is off until turned on in the window, and off again when it
// closes. The clipboard follows this Mac's while "Sync Clipboard" is on: a
// Guest copy is put on the pasteboard, and a Mac copy goes to the Guest when
// the window becomes key. Text only.

@class NSView;

// From the SPICE thread (spice-console.m).
void VeynodeAudioAvailable(void);
void VeynodeAudioPlay(const void *pcm, NSUInteger size, int rate, int channels);
void VeynodeClipboardFromGuest(NSString *text);

// Into spice-console.m, from any thread.
void VeynodeClipboardSet(NSString *text);
/** Asks for the Guest's clipboard; it arrives as VeynodeClipboardFromGuest. */
void VeynodeClipboardGet(void);
void VeynodeAudioCapture(NSData *mono);

// From AppKit's thread (Display.m).
/** The window's title bar controls: speaker, microphone and clipboard. */
NSView *VNMediaControls(void);
void VNMediaWindowChanged(BOOL open, BOOL minimized);
void VNMediaWindowBecameKey(void);
