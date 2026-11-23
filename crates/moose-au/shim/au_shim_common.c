/**
 * Shared globals and registration for AU v2 and v3 shims.
 *
 * The same compiled framework dylib serves both v2 (.component) and
 * v3 (.appex) - the appex compiles its Swift AUAudioUnit subclass
 * separately and reads g_callbacks/g_descriptor/etc. out of this
 * dylib at runtime via dynamic symbol lookup.
 */

#include "au_shim_types.h"
#include <stddef.h>

// Global state shared between v2 and v3 shims.
// visibility("default") ensures these are exported from the framework dylib
// so the appex binary (which compiles the ObjC/Swift classes separately) can access them.
__attribute__((visibility("default"))) const AuPluginDescriptor *g_descriptor = NULL;
__attribute__((visibility("default"))) const AuCallbacks *g_callbacks = NULL;

static int g_registered = 0;

/* `_v3` so a pre-v14 Rust staticlib (which calls `moose_au_register_v2` with
 * the removed static parameter table) fails to link against a v14 framework
 * build instead of registering an old-layout AuCallbacks. */
void moose_au_register_v3(
    const AuPluginDescriptor *descriptor,
    const AuCallbacks *callbacks
) {
    if (g_registered || !descriptor || !callbacks ||
        callbacks->abi_version != MOOSE_AU_ABI_VERSION)
        return;

    g_descriptor = descriptor;
    g_callbacks = callbacks;
    g_registered = 1;
}

extern void moose_au_rust_param_snapshot_begin(const void *ctx);
extern void moose_au_rust_param_snapshot_end(void);

__attribute__((visibility("default"))) void moose_au_param_snapshot_begin(const void *ctx) {
    moose_au_rust_param_snapshot_begin(ctx);
}

__attribute__((visibility("default"))) void moose_au_param_snapshot_end(void) {
    moose_au_rust_param_snapshot_end();
}

#ifdef MOOSE_AU_FAKE_AUDIOTOOLBOX
void moose_au_test_reset_registration(void) {
    g_descriptor = NULL;
    g_callbacks = NULL;
    g_registered = 0;
}
#endif

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
