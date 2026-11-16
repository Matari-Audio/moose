#include "../../shim/vst3_shim.cpp"

static int token;

template <typename Sample>
static uint32_t synthesize(void*, const Sample**, Sample** outputs, uint32_t, uint32_t numOut,
                          const uint32_t*, uint32_t, uint32_t,
                          const uint32_t*, uint32_t, uint32_t, uint32_t frames,
                          const Vst3Transport*, const Vst3ParamChange*, uint32_t, int32_t) {
    for (uint32_t ch = 0; ch < numOut; ++ch)
        for (uint32_t i = 0; i < frames; ++i)
            outputs[ch][i] = Sample(0.25);
    return 1;
}

template <typename Sample>
static bool audible_block(MooseComponent& component, int32 sampleSize) {
    ProcessSetup setup{0, sampleSize, 16, 48000.0};
    if (component.setupProcessing(&setup) != kResultOk) return false;
    Sample samples[2][16]{};
    Sample* channels[] = {samples[0], samples[1]};
    AudioBusBuffers output{};
    output.numChannels = 2;
    if constexpr (sizeof(Sample) == sizeof(float)) output.channelBuffers32 = channels;
    else output.channelBuffers64 = channels;
    ProcessData data{};
    data.symbolicSampleSize = sampleSize;
    data.numSamples = 16;
    data.numOutputs = 1;
    data.outputs = &output;
    // Hosts may reuse a bus previously marked silent. Every new render must
    // publish its own silence state rather than inherit that earlier block.
    for (int block = 0; block < 2; ++block) {
        output.silenceFlags = 3;
        if (component.process(&data) != kResultOk) return false;
        for (int ch = 0; ch < 2; ++ch) {
            const double hostSample = output.silenceFlags & (uint64_t{1} << ch)
                ? 0.0 : samples[ch][0];
            if (hostSample != 0.25) {
                std::fprintf(stderr, "%zu-bit: generated=%g host=%g silenceFlags=%llu\n",
                             sizeof(Sample) * 8, double(samples[ch][0]), hostSample,
                             static_cast<unsigned long long>(output.silenceFlags));
                return false;
            }
        }
    }
    return true;
}

int main() {
    Vst3PluginDescriptor descriptor{};
    const uint8_t mainBus = 0;
    descriptor.num_outputs = descriptor.max_outputs = 2;
    descriptor.num_output_buses = 1;
    descriptor.output_bus_kinds = &mainBus;
    descriptor.supports_f64 = 1;
    Vst3Callbacks callbacks{};
    callbacks.create = []() -> void* { return &token; };
    callbacks.destroy = [](void*) {};
    callbacks.wants_persist_notifications = [](void*) -> bool { return false; };
    callbacks.layout_bus_channels = [](uint32_t, int32_t output, uint32_t bus) -> uint32_t {
        return output && bus == 0 ? 2 : 0;
    };
    callbacks.process = synthesize<float>;
    callbacks.process_f64 = synthesize<double>;
    g_desc = &descriptor;
    g_cb = &callbacks;
    MooseComponent component;
    const bool f32 = audible_block<float>(component, kSample32);
    const bool f64 = audible_block<double>(component, kSample64);
    if (!f32 || !f64) return 1;
    std::puts("output_silence=PASS (32-bit and 64-bit)");
}
