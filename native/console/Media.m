#import "Media.h"
#import <AppKit/AppKit.h>
#import <AVFoundation/AVFoundation.h>
#include <os/lock.h>
#include <stdatomic.h>

// connect:to:format: and installTapOnBus:bufferSize:format:block: are deprecated in the macOS 27
// SDK for error: variants that macOS 14, the oldest host, does not have.
#pragma clang diagnostic ignored "-Wdeprecated-declarations"

static const NSUInteger clipboardMax = 200 * 1024;

// Playback: interleaved stereo floats at the Guest's rate, 60 ms ahead. A
// backlog (the window was busy) skips back to the target rather than adding
// delay, as the browser viewer does.
static os_unfair_lock ringLock = OS_UNFAIR_LOCK_INIT;
static float *ring;
static NSUInteger ringCapacity, ringStart, ringSize, ringTarget;
static int ringRate;
static BOOL primed;
static _Atomic BOOL playing, available;

// AppKit's thread only.
static AVAudioEngine *output, *input;
static int outputRate;
static BOOL windowOpen, windowMinimized, muted, micOn, syncClipboard, copyRequested;
static NSInteger pasteboardCount = -1;
static NSButton *speakerButton, *micButton, *clipboardButton;

static void pull(float *left, float *right, AVAudioFrameCount frames) {
    os_unfair_lock_lock(&ringLock);
    NSUInteger count = (!primed && ringSize < ringTarget) ? 0 : MIN(frames, ringSize);
    primed = count > 0 && count == frames;
    for (NSUInteger i = 0; i < count; i++) {
        NSUInteger at = (ringStart + i) % ringCapacity;
        left[i] = ring[at * 2];
        if (right) right[i] = ring[at * 2 + 1];
    }
    ringStart = (ringStart + count) % (ringCapacity ? ringCapacity : 1);
    ringSize -= count;
    os_unfair_lock_unlock(&ringLock);
    for (NSUInteger i = count; i < frames; i++) { left[i] = 0; if (right) right[i] = 0; }
}

static void stopOutput(void) {
    [output stop];
    output = nil;
    outputRate = 0;
    os_unfair_lock_lock(&ringLock);
    ringSize = ringStart = 0; primed = NO; ringRate = 0;
    os_unfair_lock_unlock(&ringLock);
}

static void startOutput(int rate) {
    if (!playing || (output && outputRate == rate)) return;
    [output stop];
    output = [AVAudioEngine new];
    outputRate = rate;
    AVAudioFormat *format = [[AVAudioFormat alloc] initStandardFormatWithSampleRate:rate channels:2];
    AVAudioSourceNode *node = [[AVAudioSourceNode alloc] initWithFormat:format renderBlock:^OSStatus(BOOL *silence, const AudioTimeStamp *time, AVAudioFrameCount frames, AudioBufferList *list) {
        pull(list->mBuffers[0].mData, list->mNumberBuffers > 1 ? list->mBuffers[1].mData : NULL, frames);
        return noErr;
    }];
    [output attachNode:node];
    [output connect:node to:output.mainMixerNode format:format];
    if (![output startAndReturnError:NULL]) { output = nil; outputRate = 0; }
}

static void updatePlaying(void) {
    playing = windowOpen && !windowMinimized && !muted && available;
    if (!playing) stopOutput();
}

void VeynodeAudioAvailable(void) {
    dispatch_async(dispatch_get_main_queue(), ^{
        available = YES;
        speakerButton.hidden = micButton.hidden = NO;
        updatePlaying();
    });
}

void VeynodeAudioPlay(const void *pcm, NSUInteger size, int rate, int channels) {
    if (!playing || rate < 8000 || rate > 96000 || channels < 1 || channels > 2) return;
    const int16_t *samples = pcm;
    NSUInteger frames = size / 2 / (NSUInteger)channels;
    BOOL restart = NO;
    os_unfair_lock_lock(&ringLock);
    if (ringRate != rate) {
        free(ring);
        ringCapacity = (NSUInteger)rate;
        ring = calloc(ringCapacity * 2, sizeof(float));
        ringStart = ringSize = 0; primed = NO;
        ringTarget = (NSUInteger)rate * 6 / 100;
        ringRate = rate;
        restart = YES;
    }
    for (NSUInteger i = 0; ring && i < frames; i++) {
        if (ringSize == ringCapacity) { ringStart = (ringStart + 1) % ringCapacity; ringSize--; }
        NSUInteger at = (ringStart + ringSize) % ringCapacity;
        ring[at * 2] = samples[i * channels] / 32768.0f;
        ring[at * 2 + 1] = samples[i * channels + channels - 1] / 32768.0f;
        ringSize++;
    }
    if (ringSize > ringTarget * 4) {
        NSUInteger drop = ringSize - ringTarget;
        ringStart = (ringStart + drop) % ringCapacity;
        ringSize -= drop;
    }
    os_unfair_lock_unlock(&ringLock);
    if (restart) dispatch_async(dispatch_get_main_queue(), ^{ startOutput(rate); });
}

static void stopMic(void) {
    [input.inputNode removeTapOnBus:0];
    [input stop];
    input = nil;
    micOn = NO;
}

static void refreshButtons(void) {
    speakerButton.image = [NSImage imageWithSystemSymbolName:muted ? @"speaker.slash.fill" : @"speaker.wave.2.fill" accessibilityDescription:muted ? @"Unmute" : @"Mute"];
    speakerButton.toolTip = muted ? @"Unmute" : @"Mute";
    micButton.image = [NSImage imageWithSystemSymbolName:micOn ? @"mic.fill" : @"mic.slash" accessibilityDescription:micOn ? @"Turn Microphone Off" : @"Turn Microphone On"];
    // Red while the Guest hears this Mac.
    micButton.contentTintColor = micOn ? NSColor.systemRedColor : nil;
    micButton.toolTip = micOn ? @"Turn Microphone Off" : @"Turn Microphone On";
}

static void micProblem(NSString *text) {
    stopMic();
    refreshButtons();
    NSAlert *alert = [NSAlert new];
    alert.messageText = @"The microphone is unavailable";
    alert.informativeText = text;
    [alert runModal];
}

/** 48 kHz mono s16le in 20 ms frames, whatever the input device's format. */
static void startMic(void) {
    switch ([AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio]) {
    case AVAuthorizationStatusNotDetermined:
        [AVCaptureDevice requestAccessForMediaType:AVMediaTypeAudio completionHandler:^(BOOL granted) {
            dispatch_async(dispatch_get_main_queue(), ^{
                if (granted && micOn) startMic();
                else if (!granted) { micOn = NO; refreshButtons(); }
            });
        }];
        return;
    case AVAuthorizationStatusAuthorized:
        break;
    default:
        micProblem(@"Allow Veynode VM Console in System Settings › Privacy & Security › Microphone, then turn the microphone on again.");
        return;
    }
    input = [AVAudioEngine new];
    AVAudioInputNode *node = input.inputNode;
    AVAudioFormat *device = [node outputFormatForBus:0];
    AVAudioFormat *target = [[AVAudioFormat alloc] initWithCommonFormat:AVAudioPCMFormatInt16 sampleRate:48000 channels:1 interleaved:YES];
    AVAudioConverter *converter = device.sampleRate > 0 ? [[AVAudioConverter alloc] initFromFormat:device toFormat:target] : nil;
    if (!converter) { micProblem(@"This Mac has no microphone input."); return; }
    NSMutableData *pending = [NSMutableData new];
    [node installTapOnBus:0 bufferSize:1024 format:device block:^(AVAudioPCMBuffer *buffer, AVAudioTime *when) {
        AVAudioPCMBuffer *out = [[AVAudioPCMBuffer alloc] initWithPCMFormat:target frameCapacity:(AVAudioFrameCount)ceil(buffer.frameLength * 48000.0 / device.sampleRate) + 32];
        __block BOOL given = NO;
        [converter convertToBuffer:out error:NULL withInputFromBlock:^AVAudioBuffer *(AVAudioPacketCount count, AVAudioConverterInputStatus *status) {
            if (given) { *status = AVAudioConverterInputStatus_NoDataNow; return nil; }
            given = YES;
            *status = AVAudioConverterInputStatus_HaveData;
            return buffer;
        }];
        [pending appendBytes:out.int16ChannelData[0] length:out.frameLength * 2];
        while (pending.length >= 1920) {
            VeynodeAudioCapture([pending subdataWithRange:NSMakeRange(0, 1920)]);
            [pending replaceBytesInRange:NSMakeRange(0, 1920) withBytes:NULL length:0];
        }
    }];
    if (![input startAndReturnError:NULL]) micProblem(@"The microphone could not start.");
}

void VeynodeClipboardFromGuest(NSString *text) {
    dispatch_async(dispatch_get_main_queue(), ^{
        BOOL wanted = copyRequested || syncClipboard;
        copyRequested = NO;
        if (!windowOpen || !wanted) return;
        NSPasteboard *board = NSPasteboard.generalPasteboard;
        [board clearContents];
        [board setString:text forType:NSPasteboardTypeString];
        // Our own write is not a Mac copy to send back.
        pasteboardCount = board.changeCount;
    });
}

static void sendPasteboard(BOOL always) {
    NSPasteboard *board = NSPasteboard.generalPasteboard;
    if (!always && board.changeCount == pasteboardCount) return;
    pasteboardCount = board.changeCount;
    NSString *text = [board stringForType:NSPasteboardTypeString];
    if (text && [text lengthOfBytesUsingEncoding:NSUTF8StringEncoding] <= clipboardMax) VeynodeClipboardSet(text);
}

void VNMediaWindowBecameKey(void) {
    if (syncClipboard) sendPasteboard(NO);
}

void VNMediaWindowChanged(BOOL open, BOOL minimized) {
    windowOpen = open;
    windowMinimized = minimized;
    if (!open) stopMic();
    updatePlaying();
    refreshButtons();
}

@interface VNMediaTarget : NSObject
@end
@implementation VNMediaTarget
- (void)toggleSpeaker:(id)sender {
    muted = !muted;
    [NSUserDefaults.standardUserDefaults setBool:muted forKey:@"VeynodeMuted"];
    updatePlaying();
    refreshButtons();
}
- (void)toggleMic:(id)sender {
    micOn = !micOn;
    if (micOn) startMic(); else stopMic();
    refreshButtons();
}
- (void)showClipboard:(NSButton *)sender {
    NSMenu *menu = [NSMenu new];
    NSMenuItem *sync = [menu addItemWithTitle:@"Sync Clipboard" action:@selector(toggleSync:) keyEquivalent:@""];
    sync.target = self;
    sync.state = syncClipboard ? NSControlStateValueOn : NSControlStateValueOff;
    [menu addItem:NSMenuItem.separatorItem];
    [menu addItemWithTitle:@"Send Mac Clipboard to Desktop" action:@selector(sendClipboard:) keyEquivalent:@""].target = self;
    [menu addItemWithTitle:@"Copy Desktop Clipboard" action:@selector(copyClipboard:) keyEquivalent:@""].target = self;
    [menu popUpMenuPositioningItem:nil atLocation:NSMakePoint(0, NSHeight(sender.bounds) + 4) inView:sender];
}
- (void)toggleSync:(id)sender {
    syncClipboard = !syncClipboard;
    [NSUserDefaults.standardUserDefaults setBool:!syncClipboard forKey:@"VeynodeClipboardManual"];
    if (syncClipboard) sendPasteboard(NO);
}
- (void)sendClipboard:(id)sender { sendPasteboard(YES); }
- (void)copyClipboard:(id)sender { copyRequested = YES; VeynodeClipboardGet(); }
@end

static NSButton *button(NSString *symbol, NSString *label, SEL action, id target) {
    NSButton *b = [NSButton buttonWithImage:[NSImage imageWithSystemSymbolName:symbol accessibilityDescription:label] target:target action:action];
    b.bordered = NO;
    b.toolTip = label;
    return b;
}

NSView *VNMediaControls(void) {
    static VNMediaTarget *target;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        target = [VNMediaTarget new];
        muted = [NSUserDefaults.standardUserDefaults boolForKey:@"VeynodeMuted"];
        syncClipboard = ![NSUserDefaults.standardUserDefaults boolForKey:@"VeynodeClipboardManual"];
    });
    speakerButton = button(@"speaker.wave.2.fill", @"Mute", @selector(toggleSpeaker:), target);
    micButton = button(@"mic.slash", @"Turn Microphone On", @selector(toggleMic:), target);
    clipboardButton = button(@"doc.on.clipboard", @"Clipboard", @selector(showClipboard:), target);
    speakerButton.hidden = micButton.hidden = !available;
    refreshButtons();
    NSStackView *stack = [NSStackView stackViewWithViews:@[speakerButton, micButton, clipboardButton]];
    stack.spacing = 10;
    stack.edgeInsets = NSEdgeInsetsMake(0, 8, 0, 10);
    [stack setFrameSize:stack.fittingSize];
    return stack;
}
