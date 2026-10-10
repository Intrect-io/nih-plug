// Only the Rust callbacks are fixtures; link and execute the production ObjC shim.
#import <Foundation/Foundation.h>
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

extern void nih_plug_au_release_container(void *container);

typedef struct {
    void *image;
} FixtureSlot;

static char imageToken;
static unsigned spawns;
static unsigned closes;
static bool failNextSpawn;
static void *closedUnit;

unsigned fixture_spawns(void) { return spawns; }
unsigned fixture_closes(void) { return closes; }
void fixture_fail_next_spawn(void) { failNextSpawn = true; }
void fixture_mark_unit_closed(void *unit) { closedUnit = unit; }

bool nih_plug_au_cocoaui_editor_size_for_audio_unit(
    void *unit, uint32_t *width, uint32_t *height) {
    if (!unit || unit == closedUnit) return false;
    *width = 320;
    *height = 200;
    return true;
}

void *nih_plug_au_cocoaui_spawn_for_audio_unit(void *parent, void *unit) {
    assert(parent && unit && unit != closedUnit);
    if (failNextSpawn) {
        failNextSpawn = false;
        return NULL;
    }
    FixtureSlot *slot = calloc(1, sizeof(FixtureSlot));
    assert(slot);
    slot->image = &imageToken;
    spawns++;
    return slot;
}

void nih_plug_au_cocoaui_close_view(void *parent, void *opaqueSlot) {
    FixtureSlot *slot = opaqueSlot;
    assert(slot && slot->image == &imageToken);
    closes++;
    free(slot);
    nih_plug_au_release_container(parent);
}
