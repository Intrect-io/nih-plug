#import <Foundation/Foundation.h>
#import <objc/runtime.h>
#import <AudioUnit/AUCocoaUIView.h>
#include <assert.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#ifdef NDEBUG
#error "Bundle regression assertions must remain enabled"
#endif
static void *loadImage(const char *path) {
    void *image = dlopen(path, RTLD_NOW | RTLD_LOCAL);
    if (!image) fprintf(stderr, "%s\n", dlerror());
    assert(image);
    return image;
}
static void loadSibling(const char *path) {
    void *image = loadImage(path);
    (void)image;
    unsigned count = 0;
    const char **names = objc_copyClassNamesForImage(path, &count);
    for (unsigned i = 0; i < count; i++) {
        assert(strncmp(names[i], "NihPlugAu", strlen("NihPlugAu")) != 0);
    }
    free(names);
}
int main(int argc, const char *argv[]) {
    assert(argc == 5);
    @autoreleasepool {
        BOOL siblingsFirst = strcmp(argv[1], "siblings-first") == 0;
        assert(siblingsFirst || strcmp(argv[1], "au-first") == 0);
        if (siblingsFirst) { loadSibling(argv[3]); loadSibling(argv[4]); }
        void *auImage = loadImage(argv[2]);
        if (!siblingsFirst) { loadSibling(argv[3]); loadSibling(argv[4]); }
        (void)auImage;
        unsigned count = 0;
        const char **names = objc_copyClassNamesForImage(argv[2], &count);
        const char *factoryName = NULL;
        unsigned factories = 0, containers = 0;
        for (unsigned i = 0; i < count; i++) {
            if (strncmp(names[i], "NihPlugAuViewFactory_", strlen("NihPlugAuViewFactory_")) == 0) {
                factoryName = names[i]; factories++;
            }
            if (strncmp(names[i], "NihPlugAuContainerView_", strlen("NihPlugAuContainerView_")) == 0) containers++;
        }
        assert(factoryName && factories == 1 && containers == 1);
        NSString *bundlePath = @(argv[2]);
        for (unsigned i = 0; i < 3; i++) bundlePath = bundlePath.stringByDeletingLastPathComponent;
        NSBundle *bundle = [NSBundle bundleWithPath:bundlePath];
        assert(bundle);
        Class factory = [bundle classNamed:@(factoryName)];
        assert(factory && factory == objc_lookUpClass(factoryName));
        assert(class_conformsToProtocol(factory, @protocol(AUCocoaUIBase)));
        assert(strcmp(class_getImageName(factory), argv[2]) == 0);
        printf("PASS: generated bundles, %s, factory %s belongs to AU\n", argv[1], factoryName);
        free(names);
    }
    return 0;
}
