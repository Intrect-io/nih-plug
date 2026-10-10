#import <AppKit/AppKit.h>
#import <AudioUnit/AUCocoaUIView.h>
#import <objc/runtime.h>
#include <assert.h>
#include <dlfcn.h>
#include <stdio.h>
#include <string.h>

#ifdef NDEBUG
#error "CocoaUI regression assertions must remain enabled"
#endif

typedef struct {
    void *image;
    const char *(*className)(void);
    void (*closeUnit)(void *);
    unsigned (*spawns)(void);
    unsigned (*closes)(void);
    void (*failNextSpawn)(void);
    void (*markUnitClosed)(void *);
} Bridge;

static Bridge loadBridge(const char *path) {
    Bridge b = {0};
    b.image = dlopen(path, RTLD_NOW | RTLD_LOCAL);
    assert(b.image);
    b.className = dlsym(b.image, "nih_plug_au_cocoaui_class_name");
    b.closeUnit = dlsym(b.image, "nih_plug_au_cocoaui_close_audio_unit_view");
    b.spawns = dlsym(b.image, "fixture_spawns");
    b.closes = dlsym(b.image, "fixture_closes");
    b.failNextSpawn = dlsym(b.image, "fixture_fail_next_spawn");
    b.markUnitClosed = dlsym(b.image, "fixture_mark_unit_closed");
    assert(b.className && b.closeUnit && b.spawns && b.closes && b.failNextSpawn && b.markUnitClosed);
    return b;
}

static NSString *implementationImage(Class c, SEL selector) {
    Method method = class_getInstanceMethod(c, selector);
    assert(method);
    Dl_info info = {0};
    assert(dladdr((const void *)method_getImplementation(method), &info));
    return @(info.dli_fname);
}

static NSBundle *bundleForImage(const char *path) {
    NSString *bundlePath = @((path));
    for (unsigned i = 0; i < 3; i++) {
        bundlePath = bundlePath.stringByDeletingLastPathComponent;
    }
    NSBundle *bundle = [NSBundle bundleWithPath:bundlePath];
    assert(bundle);
    return bundle;
}

static Bridge loadSibling(const char *path) {
    Bridge b = loadBridge(path);
    assert(b.className() == NULL);
    unsigned count = 0;
    const char **names = objc_copyClassNamesForImage(path, &count);
    for (unsigned i = 0; i < count; i++) {
        assert(strncmp(names[i], "NihPlugAu", strlen("NihPlugAu")) != 0);
    }
    free(names);
    return b;
}

int main(int argc, const char *argv[]) {
    assert(argc == 7);
    @autoreleasepool {
        [NSApplication sharedApplication];
        BOOL siblingsFirst = strcmp(argv[1], "siblings-first") == 0;
        assert(siblingsFirst || strcmp(argv[1], "au-first") == 0);
        Bridge vst = {0};
        Bridge clap = {0};
        if (siblingsFirst) {
            vst = loadSibling(argv[4]);
            clap = loadSibling(argv[5]);
        }
        Bridge a = loadBridge(argv[2]);
        if (!siblingsFirst) {
            vst = loadSibling(argv[4]);
            clap = loadSibling(argv[5]);
        }
        Bridge b = loadBridge(argv[3]);
        const char *aName = a.className();
        const char *bName = b.className();
        assert(aName && bName && strcmp(aName, bName) != 0);
        assert(strcmp(aName, a.className()) == 0);
        assert(strcmp(bName, b.className()) == 0);
        Class aClass = objc_lookUpClass(aName);
        Class bClass = objc_lookUpClass(bName);
        assert(aClass && bClass && aClass != bClass);
        // A host may resolve the factory from its advertised bundle instead
        // of the global registry. Runtime-created classes fail this contract.
        NSBundle *aBundle = bundleForImage(argv[2]);
        NSBundle *bBundle = bundleForImage(argv[3]);
        assert([aBundle classNamed:@(aName)] == aClass);
        assert([bBundle classNamed:@(bName)] == bClass);
        assert([aBundle classNamed:@(bName)] == Nil);
        assert([bBundle classNamed:@(aName)] == Nil);
        assert(strcmp(class_getImageName(aClass), argv[2]) == 0);
        assert(strcmp(class_getImageName(bClass), argv[3]) == 0);
        assert(class_conformsToProtocol(aClass, @protocol(AUCocoaUIBase)));
        assert(class_conformsToProtocol(bClass, @protocol(AUCocoaUIBase)));
        SEL viewSelector = @selector(uiViewForAudioUnit:withSize:);
        NSString *aImage = implementationImage(aClass, viewSelector);
        NSString *bImage = implementationImage(bClass, viewSelector);
        assert([aImage isEqualToString:@(argv[2])]);
        assert([bImage isEqualToString:@(argv[3])]);

        id<AUCocoaUIBase> fa = [[aClass alloc] init];
        id<AUCocoaUIBase> fb = [[bClass alloc] init];
        assert([fa interfaceVersion] == 0 && [fb interfaceVersion] == 0);
        AudioUnit u1 = (AudioUnit)(uintptr_t)0x100;
        AudioUnit u2 = (AudioUnit)(uintptr_t)0x200;
        NSView *a1 = [fa uiViewForAudioUnit:u1 withSize:NSZeroSize];
        NSView *b1 = [fb uiViewForAudioUnit:u1 withSize:NSZeroSize];
        NSView *a2 = [fa uiViewForAudioUnit:u2 withSize:NSZeroSize];
        assert(a1 && b1 && a2 && a1 != a2 && a1 != b1);
        assert(object_getClass(a1) != object_getClass(b1));
        assert([implementationImage(object_getClass(a1), sel_registerName("dealloc")) isEqualToString:aImage]);
        assert([implementationImage(object_getClass(b1), sel_registerName("dealloc")) isEqualToString:bImage]);
        assert(NSEqualSizes(a1.frame.size, NSMakeSize(320, 200)));
        assert([fa uiViewForAudioUnit:u1 withSize:NSZeroSize] == a1);
        assert([fb uiViewForAudioUnit:u1 withSize:NSZeroSize] == b1);
        assert(a.spawns() == 2 && b.spawns() == 1);

        // Releasing one AU must not close another instance or another image.
        a.closeUnit((void *)u2);
        assert(a.closes() == 1 && b.closes() == 0);
        assert([fa uiViewForAudioUnit:u1 withSize:NSZeroSize] == a1);
        a2 = nil;
        assert(a.closes() == 1);

        // A failed spawn is retryable and never cached as a blank view.
        AudioUnit failureUnit = (AudioUnit)(uintptr_t)0x300;
        a.failNextSpawn();
        assert([fa uiViewForAudioUnit:failureUnit withSize:NSZeroSize] == nil);
        assert([fa uiViewForAudioUnit:failureUnit withSize:NSZeroSize] != nil);
        a.closeUnit((void *)failureUnit);
        assert(a.closes() == 2);

        NSWindow *window = [[NSWindow alloc] initWithContentRect:NSMakeRect(0, 0, 320, 200)
            styleMask:NSWindowStyleMaskBorderless backing:NSBackingStoreBuffered defer:NO];
        window.releasedWhenClosed = NO;
        window.contentView = a1;
        assert(a1.window == window);
        // A hidden retained editor must be replaced through the same factory.
        NSView *reopened = [fa uiViewForAudioUnit:u1 withSize:NSZeroSize];
        assert(reopened && reopened != a1 && a.closes() == 3);
        window.contentView = reopened;
        window.contentView = [[NSView alloc] initWithFrame:NSZeroRect];
        assert(a.closes() == 4);
        NSView *afterDetach = [fa uiViewForAudioUnit:u1 withSize:NSZeroSize];
        assert(afterDetach && afterDetach != reopened);

        // The host keeps its view past AU disposal, then attaches/detaches it.
        // That late lifecycle must not dereference the already-closed slot.
        a.closeUnit((void *)u1);
        a.markUnitClosed((void *)u1);
        unsigned closesAfterDispose = a.closes();
        window.contentView = afterDetach;
        window.contentView = [[NSView alloc] initWithFrame:NSZeroRect];
        afterDetach = nil;
        reopened = nil;
        a1 = nil;
        assert(a.closes() == closesAfterDispose);
        assert([fa uiViewForAudioUnit:u1 withSize:NSZeroSize] == nil);
        assert([fb uiViewForAudioUnit:u1 withSize:NSZeroSize] == b1);
        b.closeUnit((void *)u1);
        b1 = nil;

        // Autorelease drainage and repeated close/reopen exercise dealloc as
        // well as the dictionary's per-AU strong reference.
        for (unsigned i = 0; i < 100; i++) {
            __weak NSView *releasedView = nil;
            @autoreleasepool {
                AudioUnit unit = (AudioUnit)(uintptr_t)(0x1000 + i);
                NSView *view = [fa uiViewForAudioUnit:unit withSize:NSZeroSize];
                releasedView = view;
                assert(view && [fa uiViewForAudioUnit:unit withSize:NSZeroSize] == view);
                a.closeUnit((void *)unit);
                view = nil;
            }
            assert(releasedView == nil);
        }
        BOOL balanced = a.spawns() == a.closes() && b.spawns() == b.closes();
        assert(balanced);
        assert(vst.spawns() == 0 && vst.closes() == 0);
        assert(clap.spawns() == 0 && clap.closes() == 0);
        NSDictionary *result = @{
            @"scope": @"production ObjC bridge with fixture Rust callbacks; not plugin/DAW acceptance",
            @"verdict": balanced ? @"pass" : @"fail", @"factory_a": @(aName), @"factory_b": @(bName),
            @"implementation_a": aImage, @"implementation_b": bImage,
            @"load_order": @(argv[1]), @"bundle_lookup": @YES,
            @"sibling_au_classes": @0, @"sibling_spawns": @(vst.spawns() + clap.spawns()),
            @"spawns_a": @(a.spawns()), @"closes_a": @(a.closes()),
            @"spawns_b": @(b.spawns()), @"closes_b": @(b.closes()),
            @"reopen_cycles": @100
        };
        NSError *error = nil;
        NSData *json = [NSJSONSerialization dataWithJSONObject:result
            options:NSJSONWritingPrettyPrinted | NSJSONWritingSortedKeys error:&error];
        assert(json && !error);
        assert([json writeToFile:@(argv[6]) options:NSDataWritingAtomic error:&error] && !error);
        puts("PASS: static bundle CocoaUI, load orders and cached-factory lifecycle");
    }
    return 0;
}
