/**
 * AU v2 CocoaUI bridge. Register both classes per loaded image, rather than
 * per build: AU, VST3 and CLAP bundles can contain copies of the same dylib.
 * Each class's IMPs must call that image's Rust spawn/close registry.
 */
@import AppKit;
@import AudioToolbox;
#import <AudioUnit/AUCocoaUIView.h>
#import <objc/runtime.h>
#include <dispatch/dispatch.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>

extern void nih_plug_au_cocoaui_close_view(void *container_ns_view, void *handle_slot);
extern bool nih_plug_au_cocoaui_editor_size_for_audio_unit(
    void *audio_unit, uint32_t *out_width, uint32_t *out_height);
extern void *nih_plug_au_cocoaui_spawn_for_audio_unit(
    void *parent_ns_view, void *audio_unit);

typedef struct {
    void *handleSlot;
    void *audioUnit;
    BOOL wasHostedInWindow;
} NihPlugAuContainerState;

static NSMutableDictionary<NSValue *, NSView *> *g_containerViews;
static NSObject *g_containerLock;
static Class g_containerClass;
static Class g_factoryClass;
static ptrdiff_t g_stateOffset;
static IMP g_superDealloc;
static IMP g_superViewDidMoveToWindow;
static char g_imageToken;

static NihPlugAuContainerState *containerState(__unsafe_unretained NSView *view) {
    return (NihPlugAuContainerState *)((uint8_t *)(__bridge void *)view + g_stateOffset);
}

static void closeEditorIfNeeded(__unsafe_unretained NSView *view) {
    NihPlugAuContainerState *state = containerState(view);
    if (!state->handleSlot) {
        return;
    }
    void *handleSlot = state->handleSlot;
    state->handleSlot = NULL;
    nih_plug_au_cocoaui_close_view((__bridge void *)view, handleSlot);
}

static void containerViewDidMoveToWindow(NSView *self, SEL cmd) {
    ((void (*)(__unsafe_unretained id, SEL))g_superViewDidMoveToWindow)(self, cmd);
    NihPlugAuContainerState *state = containerState(self);
    if (self.window) {
        state->wasHostedInWindow = YES;
    } else if (state->wasHostedInWindow) {
        // A new Cocoa view starts detached. Close only after it was mounted.
        state->wasHostedInWindow = NO;
        closeEditorIfNeeded(self);
    }
}

// A C IMP is not an ARC -dealloc implementation: forward explicitly, and do
// not retain its receiver (including through the function pointer's type).
static void containerDealloc(__unsafe_unretained NSView *self, SEL cmd) {
    closeEditorIfNeeded(self);
    ((void (*)(__unsafe_unretained id, SEL))g_superDealloc)(self, cmd);
}

static unsigned factoryInterfaceVersion(id self, SEL cmd) {
    (void)self;
    (void)cmd;
    return 0;
}

static NSView *factoryView(id self, SEL cmd, AudioUnit au, NSSize preferredSize) {
    (void)self;
    (void)cmd;
    NSValue *key = [NSValue valueWithPointer:(void *)au];
    @synchronized(g_containerLock) {
        NSView *existing = g_containerViews[key];
        if (existing) {
            NihPlugAuContainerState *state = containerState(existing);
            BOOL detached = existing.window == nil;
            BOOL hidden = existing.window != nil && !existing.window.isVisible;
            // Preserve repeated factory calls before first mount, and an open
            // editor. A cached factory can reopen a completed editor session.
            if (!state->wasHostedInWindow || (!detached && !hidden)) {
                return existing;
            }
            closeEditorIfNeeded(existing);
            existing = nil;
        }

        uint32_t ew = 0;
        uint32_t eh = 0;
        if (!nih_plug_au_cocoaui_editor_size_for_audio_unit((void *)au, &ew, &eh)) {
            return nil;
        }
        CGFloat w = ew > 0 ? (CGFloat)ew
                  : (preferredSize.width > 0 ? preferredSize.width : 800);
        CGFloat h = eh > 0 ? (CGFloat)eh
                  : (preferredSize.height > 0 ? preferredSize.height : 600);
        NSView *container = [[g_containerClass alloc] initWithFrame:NSMakeRect(0, 0, w, h)];
        if (!container) {
            return nil;
        }
        NihPlugAuContainerState *state = containerState(container);
        state->audioUnit = (void *)au;
        void *handleSlot = nih_plug_au_cocoaui_spawn_for_audio_unit(
            (__bridge void *)container, (void *)au);
        if (!handleSlot) {
            // Failed spawning must not cache a permanently blank view.
            return nil;
        }
        state->handleSlot = handleSlot;
        g_containerViews[key] = container;
        return container;
    }
}

static void registerImageClasses(void) {
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        g_containerLock = [NSObject new];
        g_containerViews = [NSMutableDictionary new];
        char containerName[96];
        char factoryName[96];
        Class container;
        Class factory;
        unsigned collision = 0;
        for (;;) {
            // Image-local storage has a different address even for copies of
            // the identical dylib. Do not reuse a class left by an old load.
            snprintf(containerName, sizeof(containerName), "NihPlugAuContainer_%llx_%u",
                     (unsigned long long)(uintptr_t)&g_imageToken, collision);
            snprintf(factoryName, sizeof(factoryName), "NihPlugAuViewFactory_%llx_%u",
                     (unsigned long long)(uintptr_t)&g_imageToken, collision);
            container = objc_allocateClassPair([NSView class], containerName, 0);
            factory = objc_allocateClassPair([NSObject class], factoryName, 0);
            if (container && factory) {
                break;
            }
            if (container) objc_disposeClassPair(container);
            if (factory) objc_disposeClassPair(factory);
            // Allocation can also fail for reasons other than a name collision.
            if (!objc_lookUpClass(containerName) && !objc_lookUpClass(factoryName)) {
                NSLog(@"[nih-plug AU] cannot allocate CocoaUI classes");
                return;
            }
            collision++;
        }

        uint8_t alignment = 0;
        for (size_t bytes = _Alignof(NihPlugAuContainerState); bytes > 1; bytes >>= 1) {
            alignment++;
        }
        SEL deallocSelector = sel_registerName("dealloc");
        SEL movedSelector = @selector(viewDidMoveToWindow);
        SEL versionSelector = @selector(interfaceVersion);
        SEL viewSelector = @selector(uiViewForAudioUnit:withSize:);
        Protocol *protocol = @protocol(AUCocoaUIBase);
        struct objc_method_description versionMethod =
            protocol_getMethodDescription(protocol, versionSelector, YES, YES);
        struct objc_method_description viewMethod =
            protocol_getMethodDescription(protocol, viewSelector, YES, YES);
        g_superDealloc = class_getMethodImplementation([NSView class], deallocSelector);
        g_superViewDidMoveToWindow = class_getMethodImplementation([NSView class], movedSelector);
        BOOL ready = versionMethod.types && viewMethod.types
            && class_addIvar(container, "_nihPlugState", sizeof(NihPlugAuContainerState),
                             alignment, @encode(NihPlugAuContainerState))
            && class_addMethod(container, movedSelector, (IMP)containerViewDidMoveToWindow, "v@:")
            && class_addMethod(container, deallocSelector, (IMP)containerDealloc, "v@:")
            && class_addProtocol(factory, protocol)
            && class_addMethod(factory, versionSelector, (IMP)factoryInterfaceVersion, versionMethod.types)
            && class_addMethod(factory, viewSelector, (IMP)factoryView, viewMethod.types);
        if (!ready) {
            objc_disposeClassPair(factory);
            objc_disposeClassPair(container);
            NSLog(@"[nih-plug AU] cannot register CocoaUI methods or protocol");
            return;
        }
        objc_registerClassPair(container);
        objc_registerClassPair(factory);
        g_stateOffset = ivar_getOffset(class_getInstanceVariable(container, "_nihPlugState"));
        g_containerClass = container;
        g_factoryClass = factory;
    });
}

// The host must receive the name registered by this loaded image.
const char *nih_plug_au_cocoaui_class_name(void) {
    registerImageClasses();
    return g_factoryClass ? class_getName(g_factoryClass) : NULL;
}

// This may run during dealloc. A strong local would retain a dying object.
void nih_plug_au_release_container(void *container_ns_view) {
    __unsafe_unretained NSView *container = (__bridge NSView *)container_ns_view;
    NSValue *key = [NSValue valueWithPointer:containerState(container)->audioUnit];
    @synchronized(g_containerLock) {
        if (g_containerViews[key] == container) {
            [g_containerViews removeObjectForKey:key];
        }
    }
}

void nih_plug_au_cocoaui_close_audio_unit_view(void *audio_unit) {
    if (!audio_unit) {
        return;
    }
    NSValue *key = [NSValue valueWithPointer:audio_unit];
    @synchronized(g_containerLock) {
        NSView *container = g_containerViews[key];
        if (container) {
            // A host can retain the Cocoa view past AudioUnit disposal. Clear
            // the slot while its Rust Wrapper is still alive, before releasing
            // our reference, so later detach/dealloc cannot call a stale slot.
            closeEditorIfNeeded(container);
            [g_containerViews removeObjectForKey:key];
        }
    }
}
