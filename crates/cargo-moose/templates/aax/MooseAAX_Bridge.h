#pragma once

#include "aax_shim_types.h"

#include <cstdio>

inline const MooseAaxDescriptor* MooseAax_Descriptor() {
    if (moose_aax_abi_version() != MOOSE_AAX_ABI_VERSION) {
        std::fprintf(stderr,
                     "[moose-aax] ABI version mismatch: rust=0x%08X expected=0x%08X\n",
                     moose_aax_abi_version(),
                     MOOSE_AAX_ABI_VERSION);
        return nullptr;
    }
    return moose_aax_descriptor();
}

inline const MooseAaxCallbacks* MooseAax_Callbacks() {
    if (moose_aax_abi_version() != MOOSE_AAX_ABI_VERSION) {
        return nullptr;
    }
    return moose_aax_callbacks();
}
