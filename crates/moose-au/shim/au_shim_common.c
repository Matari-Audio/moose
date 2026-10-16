/**
 * Shared globals and registration for AU v2 and v3 shims.
 *
 * The same compiled framework dylib serves both v2 (.component) and
 * v3 (.appex) - the appex compiles its Swift AUAudioUnit subclass
 * separately and reads g_callbacks/g_descriptor/etc. out of this
 * dylib at runtime via dynamic symbol lookup.
 */

#include "au_shim_types.h"
#include "au_parameter_catalog.h"
#include <stdio.h>

// Global state shared between v2 and v3 shims.
// visibility("default") ensures these are exported from the framework dylib
// so the appex binary (which compiles the ObjC classes separately) can access them.
__attribute__((visibility("default"))) const AuPluginDescriptor *g_descriptor = NULL;
__attribute__((visibility("default"))) const AuCallbacks *g_callbacks = NULL;
__attribute__((visibility("default"))) const AuParamDescriptor *g_param_descriptors = NULL;
__attribute__((visibility("default"))) uint32_t g_num_params = 0;

static int g_registered = 0;
static MooseAuParameterCatalog g_parameter_catalog = {0};

uint32_t moose_au_parameter_index(uint32_t id) {
    return moose_au_catalog_index(&g_parameter_catalog, id);
}
uint32_t moose_au_parameter_clump(uint32_t index) {
    return moose_au_catalog_clump(&g_parameter_catalog, index);
}
const char *moose_au_parameter_clump_name(uint32_t id) {
    return moose_au_catalog_clump_name(&g_parameter_catalog, id);
}

/* `_v2` so a pre-2.0 Rust staticlib (which calls `moose_au_register`)
 * fails to link against a 2.x framework build instead of registering
 * an old-layout AuCallbacks that every consumer would misread. */
void moose_au_register_v2(
    const AuPluginDescriptor *descriptor,
    const AuCallbacks *callbacks,
    const AuParamDescriptor *param_descriptors,
    uint32_t num_params
) {
    if (g_registered) return;
    if (!moose_au_catalog_build(&g_parameter_catalog, param_descriptors, num_params)) {
        fprintf(stderr, "moose: AU parameter catalog allocation or ID validation failed\n");
        return;
    }

    g_descriptor = descriptor;
    g_callbacks = callbacks;
    g_param_descriptors = param_descriptors;
    g_num_params = num_params;
    g_registered = 1;
}

// Constructor: populates globals when the dylib is loaded.
__attribute__((constructor))
static void au_shim_constructor(void) {
    moose_au_init();
}

// Weak stubs for symbols the `export_au!` macro emits in the *consumer*
// cdylib. They let `moose-au` build its own (unused) cdylib target -
// which is the only way cargo stops warning about
// `rustc-link-arg-cdylib` from this rlib. Consumer-supplied strong
// definitions override these at link time in the real plugin dylib.
__attribute__((weak))
void moose_au_init(void) {}
__attribute__((weak))
void *MooseAUFactory(const void *desc) {
    (void)desc;
    return NULL;
}
