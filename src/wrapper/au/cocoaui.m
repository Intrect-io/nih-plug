/**
 * AU v2 CocoaUI view factory — ObjC shim.
 *
 * `define_class!` (objc2/Rust) does NOT emit __OBJC_CLASS_PROTOCOLS metadata,
 * so hosts reject the factory via `conformsToProtocol:` before ever calling
 * `interfaceVersion` or `uiViewForAudioUnit:withSize:`.
 *
 * Compiling @interface ... : NSObject <AUCocoaUIBase> in real ObjC generates
 * the protocol-conformance metadata that hosts require.
 *
 * DESIGN: The host calls [[ClassName alloc] init] independently — we cannot
 * store state on the factory before `uiViewForAudioUnit:withSize:` fires.
 * Rust therefore keeps pending spawn closures keyed by the AudioUnit handle.
 * This is essential for hosts such as Live that can interleave CocoaUI
 * discovery for more than one instance or ask for a view more than once.
 *
 * Factory and container names are injected at compile time. The bundler
 * builds the AU separately, omitting these classes from sibling formats.
 * Static class metadata is required for NSBundle classNamed: ownership.
 */

@import AppKit;
@import AudioToolbox;
#import <AudioUnit/AUCocoaUIView.h>
#include <stdbool.h>
#import <objc/runtime.h>

#ifdef NIH_PLUG_AU_COCOAUI_DISABLED
// Sibling format images export the AU entry point for format detection, but
// must not register this AU-only class metadata in the shared ObjC runtime.
const char *nih_plug_au_cocoaui_class_name(void) { return NULL; }
void nih_plug_au_release_container(void *container) { (void)container; }
void nih_plug_au_cocoaui_close_audio_unit_view(void *unit) { (void)unit; }
#else
/* Provided by wrapper.rs via extern "C". The editor spawn template remains
 * available for the full AudioUnit lifetime because Live can cache this
 * factory and call it again without re-querying CocoaUI. */
extern void  nih_plug_au_cocoaui_close_view(void *container_ns_view, void *handle_slot);
extern bool nih_plug_au_cocoaui_editor_size_for_audio_unit(
    void *audio_unit, uint32_t *out_width, uint32_t *out_height);
extern void *nih_plug_au_cocoaui_spawn_for_audio_unit(
    void *parent_ns_view, void *audio_unit);

#if !defined(NIH_PLUG_AU_VIEW_CLASS) || !defined(NIH_PLUG_AU_CONTAINER_CLASS)
#error "AU factory and container names must both be provided by build.rs"
#endif

@class NIH_PLUG_AU_CONTAINER_CLASS;

/* One strong container per AudioUnit instance. A single global container made
 * opening one instance clear another instance's editor handle. Retaining each
 * returned view also bridges baseview's nested autorelease pool until the host
 * has attached it. */
static NSMutableDictionary<NSValue *, NIH_PLUG_AU_CONTAINER_CLASS *> *g_containerViews = nil;
static NSObject *g_containerLock = nil;

__attribute__((constructor))
static void _init_container_lock(void) {
    g_containerLock = [NSObject new];
    g_containerViews = [NSMutableDictionary new];
}

/* ── Container NSView — overrides dealloc to drop the Rust editor handle ── */

@interface NIH_PLUG_AU_CONTAINER_CLASS : NSView
/// Opaque pointer to the Wrapper's GuiHandleSlot, set by uiViewForAudioUnit:.
@property (nonatomic, assign) void *handleSlot;
@property (nonatomic, assign) void *audioUnit;
/// Becomes true only after the host has actually mounted this view in a window.
@property (nonatomic, assign) BOOL wasHostedInWindow;
@end

@implementation NIH_PLUG_AU_CONTAINER_CLASS

- (void)closeEditorIfNeeded {
    if (!self.handleSlot) {
        return;
    }
    void *handleSlot = self.handleSlot;
    self.handleSlot = NULL;
    nih_plug_au_cocoaui_close_view((__bridge void *)self, handleSlot);
}

- (void)viewDidMoveToWindow {
    [super viewDidMoveToWindow];
    if (self.window) {
        self.wasHostedInWindow = YES;
    } else if (self.wasHostedInWindow) {
        /* A returned Cocoa view starts detached, so only treat a nil window as
         * close after it has been mounted at least once. Live removes the view
         * when the editor closes while the AU instance itself remains alive. */
        self.wasHostedInWindow = NO;
        [self closeEditorIfNeeded];
    }
}

- (void)dealloc {
#ifdef DEBUG
    NSLog(@"[nih-plug AU] NIH_PLUG_AU_CONTAINER_CLASS dealloc: %p", (__bridge void *)self);
#endif
    [self closeEditorIfNeeded];
}

@end

/* ── Factory NSObject conforming to AUCocoaUIBase ───────────────────────── */

@interface NIH_PLUG_AU_VIEW_CLASS : NSObject <AUCocoaUIBase>
@end

@implementation NIH_PLUG_AU_VIEW_CLASS

- (unsigned)interfaceVersion {
    return 0;
}

- (NSView *)uiViewForAudioUnit:(AudioUnit)au withSize:(NSSize)preferredSize {
#ifdef DEBUG
    NSLog(@"[nih-plug AU] uiViewForAudioUnit:withSize: au=%p size=%.0fx%.0f",
          (void *)au, preferredSize.width, preferredSize.height);
#endif

    NSValue *key = [NSValue valueWithPointer:(void *)au];
    @synchronized(g_containerLock) {
        NIH_PLUG_AU_CONTAINER_CLASS *existing = g_containerViews[key];
        if (existing) {
            /* Some hosts invoke the factory twice for one editor-open cycle.
             * Before the view has ever been mounted, returning the same view
             * is valid and avoids a nil second result after the pending closure
             * was consumed. After it has been mounted, a detached or hidden
             * view is a completed editor session: Live may retain it instead
             * of removing it from the hierarchy when the wrench is closed. */
            BOOL detached = existing.window == nil;
            BOOL hidden = existing.window != nil && !existing.window.isVisible;
            if (!existing.wasHostedInWindow || (!detached && !hidden)) {
                return existing;
            }

            [existing closeEditorIfNeeded];
            existing = nil;
        }

        uint32_t ew = 0;
        uint32_t eh = 0;
        if (!nih_plug_au_cocoaui_editor_size_for_audio_unit((void *)au, &ew, &eh)) {
            return nil;
        }

        /* Use the plugin's declared size; Live passes preferredSize={0,0}. */
        CGFloat w = ew > 0 ? (CGFloat)ew
                  : (preferredSize.width  > 0 ? preferredSize.width  : 800);
        CGFloat h = eh > 0 ? (CGFloat)eh
                  : (preferredSize.height > 0 ? preferredSize.height : 600);
        NSRect frame = NSMakeRect(0, 0, w, h);
        NIH_PLUG_AU_CONTAINER_CLASS *container = [[NIH_PLUG_AU_CONTAINER_CLASS alloc] initWithFrame:frame];
        if (!container) {
            return nil;
        }
        container.audioUnit = (void *)au;

#ifdef DEBUG
        NSLog(@"[nih-plug AU] uiViewForAudioUnit: container=%p, spawning editor", (__bridge void *)container);
#endif
        void *handle_slot = nih_plug_au_cocoaui_spawn_for_audio_unit(
            (__bridge void *)container, (void *)au);
        if (!handle_slot) {
            /* The editor was not spawned, so this container hosts nothing. Caching it would
             * make every later factory call for this AudioUnit return the same permanently
             * blank view, so report the failure and let the host retry instead. */
#ifdef DEBUG
            NSLog(@"[nih-plug AU] uiViewForAudioUnit: editor spawn failed, discarding container");
#endif
            return nil;
        }

        container.handleSlot = handle_slot;
        g_containerViews[key] = container;
#ifdef DEBUG
        NSLog(@"[nih-plug AU] uiViewForAudioUnit: done, returning container");
#endif
        return container;
    }
}

@end

/*
 * Called from the container dealloc path. Remove only the matching instance
 * entry; other AU instances must keep their own visible editors alive.
 */
void nih_plug_au_release_container(void *container_ns_view) {
    __unsafe_unretained NIH_PLUG_AU_CONTAINER_CLASS *container =
        (__bridge NIH_PLUG_AU_CONTAINER_CLASS *)container_ns_view;
    NSValue *key = [NSValue valueWithPointer:container.audioUnit];
    @synchronized(g_containerLock) {
        if (g_containerViews[key] == container) {
            [g_containerViews removeObjectForKey:key];
        }
    }
}

/* Called by Wrapper::close. This is the explicit lifecycle event that lets us
 * release the per-instance strong view reference without affecting another
 * instance's editor. */
void nih_plug_au_cocoaui_close_audio_unit_view(void *audio_unit) {
    if (!audio_unit) {
        return;
    }
    NSValue *key = [NSValue valueWithPointer:audio_unit];
    @synchronized(g_containerLock) {
        NIH_PLUG_AU_CONTAINER_CLASS *container = g_containerViews[key];
        if (container) {
            // A host may retain the view after AudioUnit disposal. Clear the
            // slot while its Rust Wrapper is alive, before releasing our view.
            [container closeEditorIfNeeded];
            [g_containerViews removeObjectForKey:key];
        }
    }
}

const char *nih_plug_au_cocoaui_class_name(void) {
    return class_getName([NIH_PLUG_AU_VIEW_CLASS class]);
}
#endif
