#include "MooseAAX_Bridge.h"

extern "C" bool moose_test_aax_latency_retry() {
    MooseAaxLatency latency;
    unsigned calls = 0;
    unsigned last = 0;
    const auto submit = [&](uint32_t samples) {
        last = samples;
        return ++calls != 1;
    };

    latency.Request(64);
    latency.Flush(submit);
    if (calls != 1 || last != 64) return false;
    latency.Flush(submit);
    if (calls != 2 || last != 64) return false;
    latency.Flush(submit);
    if (calls != 2) return false;
    latency.Request(128);
    latency.Flush(submit);
    return calls == 3 && last == 128;
}
