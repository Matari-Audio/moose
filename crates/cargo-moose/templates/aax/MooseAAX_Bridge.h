#pragma once

#include "aax_shim_types.h"

#include <cstdio>
#include <optional>

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

// A timer turn retries the latest request until the controller accepts it.
class MooseAaxLatency {
public:
    void Request(uint32_t samples) { mRequested = samples; }

    template <typename Submit>
    void Flush(Submit submit) {
        const auto requested = mRequested;
        if (requested && requested != mLastReported && submit(*requested)) {
            mLastReported = requested;
        }
    }

private:
    std::optional<uint32_t> mRequested;
    std::optional<uint32_t> mLastReported;
};
