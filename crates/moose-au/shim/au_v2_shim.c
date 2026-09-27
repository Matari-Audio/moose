/**
 * Moose AU v2 shim.
 *
 * Implements AudioComponentPlugInInterface for AU v2 hosts (Reaper, auval).
 * The factory function returns this interface. All plugin logic is delegated
 * to Rust via the shared AuCallbacks table.
 */

#include <AudioToolbox/AudioToolbox.h>
#include <CoreFoundation/CoreFoundation.h>
#include <CoreMIDI/CoreMIDI.h>
#include <string.h>
#include <stdlib.h>
#include <dlfcn.h>
#include <pthread.h>
#include <Block.h>
#include <math.h>

#include "au_shim_types.h"

// Capacity of the per-instance host-automation event queue drained
// each render block. One block's worth of automation - generous even
// under dense per-block automation.
#define AU_PARAM_EVENT_CAP 512

static uint32_t au_ump_word_count(uint32_t word0);
static int au_ump_protocol_accepts(MIDIProtocolID protocol,
                                   uint32_t messageType);

// ---------------------------------------------------------------------------
// Per-instance state
// ---------------------------------------------------------------------------

typedef struct {
    AudioComponentPlugInInterface interface; // MUST be first
    AudioComponentInstance componentInstance;
    void *rustCtx;
    Boolean paramFeedbackAvailable;

    AudioStreamBasicDescription inputFormat;
    AudioStreamBasicDescription outputFormat;
    double sampleRate;
    UInt32 maxFramesPerSlice;
    Boolean initialized;

    // kAudioUnitProperty_OfflineRender (= 37): 1 while the host renders
    // offline (bounce / freeze), 0 for realtime. The host sets it; we
    // forward the change to Rust via g_callbacks->set_render_mode so the
    // plugin's process_mode tracks it, and hand it back on GetProperty.
    UInt32 offlineRender;

    // kAudioUnitProperty_PresentPreset (= 36) state. The host writes
    // an AUPreset (presetNumber + retained CFStringRef name) here and
    // reads it back; we also round-trip the name through
    // kAudioUnitProperty_ClassInfo's `kAUPresetNameKey` (= "name") so
    // auval's "Preset name is not retained in retrieved class data"
    // check passes. Negative presetNumber means "user preset, not from
    // the factory list" per Apple convention.
    AUPreset currentPreset;

    // kAudioUnitProperty_FactoryPresets table, built lazily from the
    // Rust factory_preset_* callbacks. Entry names stay retained until
    // Close; GetProperty hands out a CFArray of borrowed AUPreset
    // pointers into this table (AUBase convention: the host releases
    // the array, the AU owns the entries).
    AUPreset *factoryPresets;
    uint32_t factoryPresetCount;

    // Internal output buffers
    float *outputBuffers[32];
    UInt32 outputBufferSize; // in frames

    // Input callback (for effects)
    AURenderCallback inputCallback;
    void *inputCallbackRefCon;

    // Connection-based input
    AudioUnit sourceUnit;
    UInt32 sourceOutputBus;

    // Sidechain input element (kAudioUnitScope_Input element 1), present
    // only when g_descriptor->sidechain_in_channels > 0. Its own stream
    // format and callback / connection, pulled after the main input and
    // concatenated onto the flat channel array handed to cb_process.
    AudioStreamBasicDescription sidechainFormat;
    AURenderCallback sidechainInputCallback;
    void *sidechainInputCallbackRefCon;
    AudioUnit sidechainSourceUnit;
    UInt32 sidechainSourceOutputBus;

    // MIDI buffer (for instruments)
    AuNativeEvent midiBuffer[256];
    uint32_t midiCount;
    uint32_t midiOverflow;

    // Host parameter-automation events, drained each render block into
    // cb_process as EventBody::ParamChange rows so the editor follows
    // host automation (matching VST3 / AU v3). Touched only on the
    // audio thread: au_v2_schedule_parameters and au_v2_render run
    // there, and au_v2_set_parameter enqueues only when it does too
    // (see audioThreadId), so the queue needs no lock.
    AuParamEvent paramEvents[AU_PARAM_EVENT_CAP];
    uint32_t paramEventCount;
    uint32_t paramOverflow;
    // Kernel id of the audio (render) thread, captured at the top of
    // every render / schedule call. Lets au_v2_set_parameter - which
    // the host may call from any thread - tell whether it is on the
    // audio thread and so may touch paramEvents. Read/written relaxed:
    // a stale read only ever skips one enqueue, never corrupts state.
    uint64_t audioThreadId;

    // Plugin → host MIDI output callback (set via
    // kAudioUnitProperty_MIDIOutputCallback). Hosts that want to
    // receive MIDI from instruments register the callback once after
    // initialization; we drain `output_events` from Rust at the end
    // of each render block and forward via this callback.
    AUMIDIOutputCallback midiOutputCallback;
    void *midiOutputUserData;
    AUMIDIEventListBlock midiOutputEventListBlock;
    SInt32 hostMIDIProtocol;

    // Heap-allocated scratch for SysEx output packet lists. Sized
    // to hold one MIDIPacketList of up to 256 SysEx events whose
    // total framed payload tops out at moose_core::SYSEX_POOL_PREALLOC
    // (128 KiB) + 2 framing bytes per event. Per-packet overhead is
    // ~14 B (timestamp + length + headers) - we allocate enough
    // headroom that the entire pool of SysEx events fits in one
    // call to midiOutputCallback.
    Byte *sysexPacketBuf;
    uint32_t sysexPacketBufSize;
    // One-event framing scratch: `0xF0` + inner bytes + `0xF7`.
    // `MIDIPacketListAdd` copies the byte buffer synchronously, so
    // we only need enough space for the single largest event we'll
    // ever pass in. Sized to moose_core::SYSEX_POOL_PREALLOC + 2.
    Byte *sysexFrameScratch;
    uint32_t sysexFrameScratchSize;

    // Host callbacks (set via kAudioUnitProperty_HostCallbacks). Used to
    // query tempo / play state / bar position from the host each render.
    HostCallbackInfo hostCallbacks;
    Boolean hasHostCallbacks;

    // Property listeners
    struct {
        AudioUnitPropertyID prop;
        AudioUnitPropertyListenerProc proc;
        void *userData;
    } listeners[32];
    uint32_t listenerCount;

    // Render-notify callbacks (kAudioUnitAddRenderNotify). Invoked with
    // kAudioUnitRenderAction_PreRender before processing and _PostRender
    // after, which AUGraph metering / offline harnesses depend on.
    // Host threads add/remove (legal during playback) while the render
    // thread iterates - guarded by a seqlock (`renderNotifySeq`): writers
    // bump it odd->even around each mutation, the render thread copies a
    // consistent snapshot and retries if a write straddled the copy.
    struct {
        AURenderCallback proc;
        void *userData;
    } renderNotifies[32];
    uint32_t renderNotifyCount;
    // Seqlock sequence: even = stable, odd = a writer is mid-update.
    uint32_t renderNotifySeq;
} MooseAUv2;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static void build_asbd(AudioStreamBasicDescription *asbd, double sampleRate, uint32_t channels) {
    memset(asbd, 0, sizeof(*asbd));
    asbd->mSampleRate = sampleRate;
    asbd->mFormatID = kAudioFormatLinearPCM;
    asbd->mFormatFlags = kAudioFormatFlagsNativeFloatPacked | kAudioFormatFlagIsNonInterleaved;
    asbd->mBitsPerChannel = 32;
    asbd->mChannelsPerFrame = channels;
    asbd->mFramesPerPacket = 1;
    asbd->mBytesPerFrame = 4;
    asbd->mBytesPerPacket = 4;
}

// Is `channels` an acceptable count for `scope`'s stream format? A
// multi-layout plugin (num_layouts > 0) accepts any declared layout's
// main-bus count, so the host can select mono / stereo / surround from
// the configs advertised by SupportedNumChannels. A single-layout
// plugin keeps the one num_inputs / num_outputs count.
static bool channel_count_supported(uint32_t channels, AudioUnitScope scope) {
    if (g_descriptor->num_layouts > 0) {
        const int16_t *arr = scope == kAudioUnitScope_Input
            ? g_descriptor->layout_in_channels
            : g_descriptor->layout_out_channels;
        for (uint32_t i = 0; i < g_descriptor->num_layouts; i++) {
            if (arr[i] == (int16_t)channels) return true;
        }
        return false;
    }
    uint32_t only = scope == kAudioUnitScope_Input
        ? g_descriptor->num_inputs : g_descriptor->num_outputs;
    return channels == only;
}

// Is the (in, out) channel pair one of the plugin's declared layouts?
// SetStreamFormat validates each scope independently, so a host can pair
// a valid input count with a valid output count that don't belong to the
// same layout (e.g. 1-in / 2-out). Initialize checks the whole pair here
// and rejects those combinations.
static bool layout_pair_supported(uint32_t in, uint32_t out) {
    if (g_descriptor->num_layouts == 0)
        return in == g_descriptor->num_inputs && out == g_descriptor->num_outputs;
    for (uint32_t i = 0; i < g_descriptor->num_layouts; i++) {
        if ((uint32_t)g_descriptor->layout_in_channels[i] == in
            && (uint32_t)g_descriptor->layout_out_channels[i] == out)
            return true;
    }
    return false;
}

static void notify_listeners(MooseAUv2 *inst, AudioUnitPropertyID prop,
                             AudioUnitScope scope, AudioUnitElement elem) {
    for (uint32_t i = 0; i < inst->listenerCount; i++) {
        if (inst->listeners[i].prop == prop) {
            inst->listeners[i].proc(inst->listeners[i].userData,
                                     inst->componentInstance, prop, scope, elem);
        }
    }
}

// Global ctx→MooseAUv2* mapping for host notification from GUI callbacks
#define kMaxAUInstances 64
static void *g_au_ctx_keys[kMaxAUInstances] = {0};
static MooseAUv2 *g_au_ctx_vals[kMaxAUInstances] = {0};

static bool au_ctx_map_register(void *ctx, MooseAUv2 *inst) {
    for (int i = 0; i < kMaxAUInstances; i++) {
        if (!g_au_ctx_keys[i]) {
            g_au_ctx_keys[i] = ctx;
            g_au_ctx_vals[i] = inst;
            return true;
        }
    }
    return false;
}

static void au_ctx_map_unregister(void *ctx) {
    for (int i = 0; i < kMaxAUInstances; i++) {
        if (g_au_ctx_keys[i] == ctx) {
            g_au_ctx_keys[i] = NULL;
            g_au_ctx_vals[i] = NULL;
            return;
        }
    }
}

static MooseAUv2 *au_ctx_map_lookup(void *ctx) {
    for (int i = 0; i < kMaxAUInstances; i++) {
        if (g_au_ctx_keys[i] == ctx) return g_au_ctx_vals[i];
    }
    return NULL;
}

/* Build the AudioUnitEvent the gesture / value-change helpers all share. */
static void fill_param_event(AudioUnitEvent *event, AudioUnit unit,
                             uint32_t param_id, AudioUnitEventType type) {
    memset(event, 0, sizeof(*event));
    event->mEventType = type;
    event->mArgument.mParameter.mAudioUnit = unit;
    event->mArgument.mParameter.mParameterID = param_id;
    event->mArgument.mParameter.mScope = kAudioUnitScope_Global;
    event->mArgument.mParameter.mElement = 0;
}

/* Called from Rust GUI callback to notify the AU host of a parameter change.
 *
 * Sets the value via `AudioUnitSetParameter` (which updates the host's
 * cached value) and broadcasts a `kAudioUnitEvent_ParameterValueChange`
 * via `AUEventListenerNotify` so any registered AUEventListener sees
 * the change and records automation. `AudioUnitSetParameter` alone does
 * not synthesise the listener notification - hosts that thin / record
 * automation rely on the explicit broadcast. */
void moose_au_v2_host_set_param(void *ctx, uint32_t param_id, float value) {
    MooseAUv2 *inst = au_ctx_map_lookup(ctx);
    if (!inst || !inst->componentInstance) return;
    AudioUnitSetParameter(inst->componentInstance, param_id,
                          kAudioUnitScope_Global, 0, value, 0);
    AudioUnitEvent event;
    fill_param_event(&event, inst->componentInstance, param_id,
                     kAudioUnitEvent_ParameterValueChange);
    AUEventListenerNotify(NULL, NULL, &event);
}

/* Called from Rust GUI callback when the user starts dragging a control.
 *
 * Posts `kAudioUnitEvent_BeginParameterChangeGesture` so hosts (Logic,
 * Live, Reaper) group the subsequent value changes into a single undo
 * step and start gesture-aware automation recording. Without this, every
 * sample of a knob drag becomes a separate undo entry. */
void moose_au_v2_host_begin_param_gesture(void *ctx, uint32_t param_id) {
    MooseAUv2 *inst = au_ctx_map_lookup(ctx);
    if (!inst || !inst->componentInstance) return;
    AudioUnitEvent event;
    fill_param_event(&event, inst->componentInstance, param_id,
                     kAudioUnitEvent_BeginParameterChangeGesture);
    AUEventListenerNotify(NULL, NULL, &event);
}

/* Called from Rust GUI callback when the user releases a control.
 *
 * Posts `kAudioUnitEvent_EndParameterChangeGesture` to close the
 * gesture started by `..._begin_param_gesture`. Hosts use the End event
 * to commit the undo group and stop the automation-recording window. */
void moose_au_v2_host_end_param_gesture(void *ctx, uint32_t param_id) {
    MooseAUv2 *inst = au_ctx_map_lookup(ctx);
    if (!inst || !inst->componentInstance) return;
    AudioUnitEvent event;
    fill_param_event(&event, inst->componentInstance, param_id,
                     kAudioUnitEvent_EndParameterChangeGesture);
    AUEventListenerNotify(NULL, NULL, &event);
}

/* Called from the Rust latency notifier when the plugin's reported
 * latency changed. Broadcasts a `kAudioUnitProperty_Latency` property
 * change so the host re-reads `kAudioUnitProperty_Latency` and updates
 * its delay compensation. Runs on the notifier thread, off the audio
 * thread, exactly like the parameter notifications above. */
void moose_au_v2_host_latency_changed(void *ctx) {
    MooseAUv2 *inst = au_ctx_map_lookup(ctx);
    if (!inst || !inst->componentInstance) return;
    AudioUnitEvent event;
    memset(&event, 0, sizeof(event));
    event.mEventType = kAudioUnitEvent_PropertyChange;
    event.mArgument.mProperty.mAudioUnit = inst->componentInstance;
    event.mArgument.mProperty.mPropertyID = kAudioUnitProperty_Latency;
    event.mArgument.mProperty.mScope = kAudioUnitScope_Global;
    event.mArgument.mProperty.mElement = 0;
    AUEventListenerNotify(NULL, NULL, &event);
}

/* Whether the host should hand this plugin MIDI input. Gated on the
 * `accepts_midi_in` capability (category default, overridable via
 * `midi_input` in moose.toml), not the component type - so an `aumu`
 * instrument and an `aumf` MusicEffect both get the
 * `MusicDeviceMIDIEvent` handler, while a plain `aufx` effect doesn't. */
static int accepts_midi_input(void) {
    return g_descriptor && g_descriptor->accepts_midi_in;
}

// ---------------------------------------------------------------------------
// Factory presets
// ---------------------------------------------------------------------------

// Build (once) the AUPreset table the factory-presets property serves.
// Returns the entry count; 0 means the plugin ships no factory presets
// and callers report kAudioUnitProperty_FactoryPresets as invalid.
static uint32_t ensure_factory_presets(MooseAUv2 *inst) {
    if (inst->factoryPresets) return inst->factoryPresetCount;
    if (!g_callbacks || !inst->rustCtx || !g_callbacks->factory_preset_count)
        return 0;
    uint32_t n = g_callbacks->factory_preset_count(inst->rustCtx);
    if (n == 0) return 0;
    AUPreset *table = (AUPreset *)calloc(n, sizeof(AUPreset));
    if (!table) return 0;
    for (uint32_t i = 0; i < n; i++) {
        const char *name = g_callbacks->factory_preset_name(inst->rustCtx, i);
        table[i].presetNumber = (SInt32)i;
        table[i].presetName = CFStringCreateWithCString(
            NULL, name ? name : "Preset", kCFStringEncodingUTF8);
    }
    inst->factoryPresets = table;
    inst->factoryPresetCount = n;
    return n;
}

// ---------------------------------------------------------------------------
// Open / Close
// ---------------------------------------------------------------------------

static OSStatus au_v2_open(void *self_, AudioComponentInstance instance) {
    MooseAUv2 *inst = (MooseAUv2 *)self_;
    inst->componentInstance = instance;

    if (!g_callbacks) return kAudioUnitErr_FailedInitialization;
    inst->rustCtx = g_callbacks->create();
    if (!inst->rustCtx) return kAudioUnitErr_FailedInitialization;
    inst->paramFeedbackAvailable = au_ctx_map_register(inst->rustCtx, inst);

    inst->sampleRate = 44100.0;
    inst->maxFramesPerSlice = 1024;

    if (g_descriptor->num_outputs > 0)
        build_asbd(&inst->outputFormat, inst->sampleRate, g_descriptor->num_outputs);
    if (g_descriptor->num_inputs > 0)
        build_asbd(&inst->inputFormat, inst->sampleRate, g_descriptor->num_inputs);
    if (g_descriptor->sidechain_in_channels > 0)
        build_asbd(&inst->sidechainFormat, inst->sampleRate,
                   g_descriptor->sidechain_in_channels);

    // Default preset. presetNumber = -1 is Apple's "no factory preset
    // selected, this is a user / default state" sentinel. The name is
    // CFRetained here and released in au_v2_close.
    inst->currentPreset.presetNumber = -1;
    inst->currentPreset.presetName = CFSTR("Untitled");
    CFRetain(inst->currentPreset.presetName);

    return noErr;
}

static OSStatus au_v2_close(void *self_) {
    MooseAUv2 *inst = (MooseAUv2 *)self_;
    if (inst->rustCtx && g_callbacks) {
        void *ctx = inst->rustCtx;
        // Destroy joins the Rust notifier and flushes its queue. Keep the map
        // live until that join completes so accepted feedback cannot vanish
        // during teardown.
        g_callbacks->destroy(ctx);
        au_ctx_map_unregister(ctx);
        inst->rustCtx = NULL;
        inst->paramFeedbackAvailable = false;
    }
    for (int c = 0; c < 32; c++) {
        free(inst->outputBuffers[c]);
        inst->outputBuffers[c] = NULL;
    }
    free(inst->sysexPacketBuf);
    inst->sysexPacketBuf = NULL;
    free(inst->sysexFrameScratch);
    inst->sysexFrameScratch = NULL;
    if (inst->midiOutputEventListBlock) {
        Block_release(inst->midiOutputEventListBlock);
        inst->midiOutputEventListBlock = NULL;
    }
    if (inst->currentPreset.presetName) {
        CFRelease(inst->currentPreset.presetName);
        inst->currentPreset.presetName = NULL;
    }
    if (inst->factoryPresets) {
        for (uint32_t i = 0; i < inst->factoryPresetCount; i++) {
            if (inst->factoryPresets[i].presetName) {
                CFRelease(inst->factoryPresets[i].presetName);
            }
        }
        free(inst->factoryPresets);
        inst->factoryPresets = NULL;
    }
    free(inst);
    return noErr;
}

// ---------------------------------------------------------------------------
// Initialize / Uninitialize / Reset
// ---------------------------------------------------------------------------

static OSStatus au_v2_initialize(void *self_) {
    MooseAUv2 *inst = (MooseAUv2 *)self_;

    if (inst->midiOutputEventListBlock &&
        inst->hostMIDIProtocol != kMIDIProtocol_1_0 &&
        inst->hostMIDIProtocol != kMIDIProtocol_2_0)
        return kAudioUnitErr_FormatNotSupported;

    // Reject a channel pairing the host assembled from independently-set
    // scopes that isn't one of the declared layouts (audio-less plugins
    // keep num_inputs == 0, matching the single (0, num_outputs) layout).
    uint32_t numOut = inst->outputFormat.mChannelsPerFrame;
    uint32_t numIn = g_descriptor->num_inputs > 0 ? inst->inputFormat.mChannelsPerFrame : 0;
    if (!layout_pair_supported(numIn, numOut))
        return kAudioUnitErr_FormatNotSupported;

    if (g_callbacks && inst->rustCtx)
        g_callbacks->reset(inst->rustCtx, inst->sampleRate, inst->maxFramesPerSlice);

    // Allocate internal buffers for the negotiated channel count (a
    // multi-layout plugin may run at any declared width, not just the
    // first layout's num_outputs). Main input and output share buffers
    // [0, base): that overlap is intended in-place aliasing the Rust
    // bridge snapshots. The sidechain, though, must NOT overlap the
    // output region - with numOut > numIn (a mono-in/stereo-out plugin) a
    // sidechain staged right after the main input would land on an output
    // buffer and get clobbered mid-block. Stage it past BOTH directions,
    // at [base, base + scCh), and size the allocation to match.
    uint32_t base = numIn > numOut ? numIn : numOut;
    uint32_t numBuf = base + g_descriptor->sidechain_in_channels;
    for (uint32_t c = 0; c < numBuf && c < 32; c++) {
        free(inst->outputBuffers[c]);
        inst->outputBuffers[c] = (float *)calloc(inst->maxFramesPerSlice, sizeof(float));
    }
    inst->outputBufferSize = inst->maxFramesPerSlice;

    inst->initialized = true;
    return noErr;
}

static OSStatus au_v2_uninitialize(void *self_) {
    MooseAUv2 *inst = (MooseAUv2 *)self_;
    inst->initialized = false;
    return noErr;
}

static OSStatus au_v2_reset(void *self_, AudioUnitScope scope, AudioUnitElement elem) {
    (void)scope; (void)elem;
    MooseAUv2 *inst = (MooseAUv2 *)self_;
    if (g_callbacks && inst->rustCtx)
        g_callbacks->reset(inst->rustCtx, inst->sampleRate, inst->maxFramesPerSlice);
    inst->midiCount = 0;
    inst->midiOverflow = 0;
    inst->paramEventCount = 0;
    inst->paramOverflow = 0;
    return noErr;
}

// ---------------------------------------------------------------------------
// GetPropertyInfo / GetProperty / SetProperty
// ---------------------------------------------------------------------------

// Count parameters that declare a default MIDI-learn binding; sizes the
// kAudioUnitProperty_AllParameterMIDIMappings array.
static uint32_t count_midi_mapped_params(void) {
    uint32_t n = 0;
    for (uint32_t i = 0; i < g_num_params; i++)
        if (g_param_descriptors[i].midi_status != 0) n++;
    return n;
}

static OSStatus au_v2_get_property_info(void *self_, AudioUnitPropertyID prop,
                                         AudioUnitScope scope, AudioUnitElement elem,
                                         UInt32 *outSize, Boolean *outWritable) {
    (void)self_; (void)elem;
    UInt32 size = 0;
    Boolean writable = false;

    switch (prop) {
        case kAudioUnitProperty_StreamFormat:
            size = sizeof(AudioStreamBasicDescription); writable = true; break;
        case kAudioUnitProperty_ElementCount:
            size = sizeof(UInt32); break;
        case kAudioUnitProperty_SampleRate:
            size = sizeof(Float64); writable = true; break;
        case kAudioUnitProperty_MaximumFramesPerSlice:
            size = sizeof(UInt32); writable = true; break;
        case kAudioUnitProperty_OfflineRender:
            size = sizeof(UInt32); writable = true; break;
        case kAudioUnitProperty_ParameterList:
            if (scope == kAudioUnitScope_Global)
                size = g_num_params * sizeof(AudioUnitParameterID);
            else
                size = 0; // params only on Global scope
            break;
        case kAudioUnitProperty_ParameterInfo:
            size = sizeof(AudioUnitParameterInfo); break;
        case kAudioUnitProperty_ParameterStringFromValue:
            size = sizeof(AudioUnitParameterStringFromValue); writable = true; break;
        case kAudioUnitProperty_ParameterValueFromString:
            size = sizeof(AudioUnitParameterValueFromString); writable = true; break;
        case kAudioUnitProperty_ParameterValueStrings:
            size = sizeof(CFArrayRef); writable = false; break;
        case kAudioUnitProperty_ParameterClumpName:
            size = sizeof(AudioUnitParameterNameInfo); writable = false; break;
        case kAudioUnitProperty_SetRenderCallback:
            if (scope == kAudioUnitScope_Input) { size = sizeof(AURenderCallbackStruct); writable = true; }
            else return kAudioUnitErr_InvalidScope;
            break;
        case kAudioUnitProperty_MakeConnection:
            if (scope == kAudioUnitScope_Input) { size = sizeof(AudioUnitConnection); writable = true; }
            else return kAudioUnitErr_InvalidScope;
            break;
        case kAudioUnitProperty_ShouldAllocateBuffer:
            size = sizeof(UInt32); writable = true; break;
        case kAudioUnitProperty_HostCallbacks:
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            size = sizeof(HostCallbackInfo); writable = true; break;
        case kAudioUnitProperty_MIDIOutputCallbackInfo:
            // Advertise a MIDI output port only when the plugin emits
            // MIDI (`emits_midi`; note-effect default, overridable via
            // `midi_output`), so a plain effect shows no phantom port.
            if (!g_descriptor || !g_descriptor->has_midi_output)
                return kAudioUnitErr_InvalidProperty;
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            size = sizeof(CFArrayRef); writable = false; break;
        case kAudioUnitProperty_MIDIOutputCallback:
            if (!g_descriptor || !g_descriptor->has_midi_output)
                return kAudioUnitErr_InvalidProperty;
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            size = sizeof(AUMIDIOutputCallbackStruct); writable = true; break;
        case kAudioUnitProperty_MIDIOutputEventListCallback:
            if (!g_descriptor || !g_descriptor->has_midi_output)
                return kAudioUnitErr_InvalidProperty;
            if (scope != kAudioUnitScope_Global || elem != 0)
                return kAudioUnitErr_InvalidScope;
            size = sizeof(AUMIDIEventListBlock); writable = true; break;
        case kAudioUnitProperty_AudioUnitMIDIProtocol:
            if (!g_descriptor || !g_descriptor->accepts_midi_in)
                return kAudioUnitErr_InvalidProperty;
            if (scope != kAudioUnitScope_Global || elem != 0)
                return kAudioUnitErr_InvalidScope;
            size = sizeof(SInt32); break;
        case kAudioUnitProperty_HostMIDIProtocol:
            if (!g_descriptor || !g_descriptor->has_midi_output)
                return kAudioUnitErr_InvalidProperty;
            if (scope != kAudioUnitScope_Global || elem != 0)
                return kAudioUnitErr_InvalidScope;
            size = sizeof(SInt32); writable = true; break;
        case kAudioUnitProperty_ClassInfo:
            size = sizeof(CFPropertyListRef); writable = true; break;
        case kAudioUnitProperty_PresentPreset:
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            size = sizeof(AUPreset); writable = true; break;
        case kAudioUnitProperty_FactoryPresets: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            // AUs with no factory presets don't expose the property at
            // all - that's what hosts (and auval) expect.
            if (ensure_factory_presets((MooseAUv2 *)self_) == 0)
                return kAudioUnitErr_InvalidProperty;
            size = sizeof(CFArrayRef); break;
        }
        case kAudioUnitProperty_AllParameterMIDIMappings: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            // Defaults-only: report the declared bindings, nothing to
            // report (and so no property) when none are declared.
            uint32_t n = count_midi_mapped_params();
            if (n == 0) return kAudioUnitErr_InvalidProperty;
            size = n * sizeof(AUParameterMIDIMapping);
            break;
        }
        case kAudioUnitProperty_Latency:
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            size = sizeof(Float64); break;
        case kAudioUnitProperty_TailTime:
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            size = sizeof(Float64); break;
        case kAudioUnitProperty_BypassEffect:
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            size = sizeof(UInt32); writable = true; break;
        case kAudioUnitProperty_LastRenderError:
            size = sizeof(OSStatus); break;
        case kAudioUnitProperty_InPlaceProcessing:
            size = sizeof(UInt32); break;
        case kAudioUnitProperty_SupportedNumChannels: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            UInt32 nCfg = g_descriptor->num_layouts > 0 ? g_descriptor->num_layouts : 1;
            size = nCfg * sizeof(AUChannelInfo); break;
        }
        case kAudioUnitProperty_CocoaUI: {
            MooseAUv2 *inst = (MooseAUv2 *)self_;
            if (!g_callbacks || !inst->rustCtx) return kAudioUnitErr_InvalidProperty;
            if (!g_callbacks->gui_has_editor(inst->rustCtx)) return kAudioUnitErr_InvalidProperty;
            size = sizeof(AudioUnitCocoaViewInfo); break;
        }
        case 64000: /* kMoosePrivateProperty_RustContext */
        case 64001: /* kMoosePrivateProperty_AuCallbacks */
            size = sizeof(void*); break;
        default:
            return kAudioUnitErr_InvalidProperty;
    }

    if (outSize) *outSize = size;
    if (outWritable) *outWritable = writable;
    return noErr;
}

// Parameter clumps group params under a heading in the host UI. A
// param's clumpID is the 1-based ordinal of its group among the distinct
// non-empty group names (declaration order); 0 means ungrouped. Stable
// across calls so a param's clumpID never changes. O(n^2), negligible for
// real param counts.
static uint32_t au_v2_clump_id(const char *group) {
    if (!group || !group[0]) return 0;
    uint32_t ordinal = 0;
    for (uint32_t i = 0; i < g_num_params; i++) {
        const char *g = g_param_descriptors[i].group;
        if (!g || !g[0]) continue;
        int first = 1;
        for (uint32_t j = 0; j < i; j++) {
            const char *pg = g_param_descriptors[j].group;
            if (pg && pg[0] && strcmp(pg, g) == 0) { first = 0; break; }
        }
        if (first) ordinal++;
        if (strcmp(g, group) == 0) return ordinal;
    }
    return 0;
}

// Reverse of au_v2_clump_id: the group name for a clumpID, for
// kAudioUnitProperty_ParameterClumpName. NULL if the id is out of range.
static const char *au_v2_clump_name(uint32_t clumpID) {
    if (clumpID == 0) return NULL;
    uint32_t ordinal = 0;
    for (uint32_t i = 0; i < g_num_params; i++) {
        const char *g = g_param_descriptors[i].group;
        if (!g || !g[0]) continue;
        int first = 1;
        for (uint32_t j = 0; j < i; j++) {
            const char *pg = g_param_descriptors[j].group;
            if (pg && pg[0] && strcmp(pg, g) == 0) { first = 0; break; }
        }
        if (first && ++ordinal == clumpID) return g;
    }
    return NULL;
}

static OSStatus au_v2_get_property(void *self_, AudioUnitPropertyID prop,
                                    AudioUnitScope scope, AudioUnitElement elem,
                                    void *outData, UInt32 *ioSize) {
    MooseAUv2 *inst = (MooseAUv2 *)self_;

    switch (prop) {
        case kAudioUnitProperty_StreamFormat: {
            if (*ioSize < sizeof(AudioStreamBasicDescription))
                return kAudioUnitErr_InvalidPropertyValue;
            AudioStreamBasicDescription *asbd = (AudioStreamBasicDescription *)outData;
            if (scope == kAudioUnitScope_Output) {
                if (g_descriptor->num_outputs == 0)
                    return kAudioUnitErr_InvalidElement;
                *asbd = inst->outputFormat;
            } else if (scope == kAudioUnitScope_Input) {
                if (g_descriptor->num_inputs == 0)
                    return kAudioUnitErr_InvalidElement;
                if (elem == 1) {
                    if (g_descriptor->sidechain_in_channels == 0)
                        return kAudioUnitErr_InvalidElement;
                    *asbd = inst->sidechainFormat;
                } else if (elem == 0) {
                    *asbd = inst->inputFormat;
                } else {
                    return kAudioUnitErr_InvalidElement;
                }
            } else {
                return kAudioUnitErr_InvalidScope;
            }
            *ioSize = sizeof(AudioStreamBasicDescription);
            return noErr;
        }

        case kAudioUnitProperty_ElementCount: {
            UInt32 *count = (UInt32 *)outData;
            if (scope == kAudioUnitScope_Input)
                // One element for the main input, a second for the
                // sidechain when the plugin declares one.
                *count = (g_descriptor->num_inputs > 0)
                    ? (g_descriptor->sidechain_in_channels > 0 ? 2 : 1) : 0;
            else if (scope == kAudioUnitScope_Output)
                *count = (g_descriptor->num_outputs > 0) ? 1 : 0;
            else if (scope == kAudioUnitScope_Global)
                *count = 1;
            else return kAudioUnitErr_InvalidScope;
            *ioSize = sizeof(UInt32);
            return noErr;
        }

        case kAudioUnitProperty_SampleRate: {
            *(Float64 *)outData = inst->sampleRate;
            *ioSize = sizeof(Float64);
            return noErr;
        }

        case kAudioUnitProperty_MaximumFramesPerSlice: {
            *(UInt32 *)outData = inst->maxFramesPerSlice;
            *ioSize = sizeof(UInt32);
            return noErr;
        }

        case kAudioUnitProperty_OfflineRender: {
            *(UInt32 *)outData = inst->offlineRender;
            *ioSize = sizeof(UInt32);
            return noErr;
        }

        case kAudioUnitProperty_ParameterList: {
            if (scope != kAudioUnitScope_Global) {
                // Return empty list for non-global scopes
                *ioSize = 0;
                return noErr;
            }
            UInt32 needed = g_num_params * sizeof(AudioUnitParameterID);
            if (*ioSize < needed) return kAudioUnitErr_InvalidPropertyValue;
            AudioUnitParameterID *ids = (AudioUnitParameterID *)outData;
            for (uint32_t i = 0; i < g_num_params; i++)
                ids[i] = g_param_descriptors[i].id;
            *ioSize = needed;
            return noErr;
        }

        case kAudioUnitProperty_ParameterInfo: {
            if (*ioSize < sizeof(AudioUnitParameterInfo))
                return kAudioUnitErr_InvalidPropertyValue;
            AudioUnitParameterInfo *info = (AudioUnitParameterInfo *)outData;
            memset(info, 0, sizeof(*info));
            for (uint32_t i = 0; i < g_num_params; i++) {
                if (g_param_descriptors[i].id == elem) {
                    const AuParamDescriptor *pd = &g_param_descriptors[i];
                    strlcpy(info->name, pd->name, sizeof(info->name));
                    info->cfNameString = CFStringCreateWithCString(NULL, pd->name,
                                            kCFStringEncodingUTF8);
                    /* step_count is values-1: 1 => a 2-value (boolean-like)
                     * param, >=2 => an indexed/enumerated list. Mapping the
                     * unit (rather than leaving Generic) is what makes the
                     * host quantize automation to the steps, offer step
                     * navigation, and query kAudioUnitProperty_ParameterValueName
                     * for the per-index names. Continuous params stay Generic. */
                    if (pd->step_count == 1)
                        info->unit = kAudioUnitParameterUnit_Boolean;
                    else if (pd->step_count >= 2)
                        info->unit = kAudioUnitParameterUnit_Indexed;
                    else
                        info->unit = kAudioUnitParameterUnit_Generic;
                    info->minValue = (AudioUnitParameterValue)pd->min;
                    info->maxValue = (AudioUnitParameterValue)pd->max;
                    info->defaultValue = (AudioUnitParameterValue)pd->default_value;
                    /* ValuesHaveStrings routes generic views and
                     * control-surface displays through
                     * kAudioUnitProperty_ParameterStringFromValue, so
                     * hosts show "50%" / "-12.0 dB" instead of the raw
                     * plain value. */
                    info->flags = kAudioUnitParameterFlag_IsReadable |
                                  kAudioUnitParameterFlag_IsWritable |
                                  kAudioUnitParameterFlag_HasCFNameString |
                                  kAudioUnitParameterFlag_CFNameRelease |
                                  kAudioUnitParameterFlag_ValuesHaveStrings;
                    /* Continuous params can be ramped: au_v2_schedule_parameters
                     * accepts ramped automation and the plugin's smoother
                     * interpolates. Stepped (bool/enum/discrete) params step,
                     * so they get no ramp flag. */
                    if (pd->step_count == 0)
                        info->flags |= kAudioUnitParameterFlag_CanRamp;
                    /* Map the param's group to a clump so the host UI nests
                     * grouped params under a heading (named via
                     * kAudioUnitProperty_ParameterClumpName). */
                    uint32_t clump = au_v2_clump_id(pd->group);
                    if (clump != 0) {
                        info->clumpID = clump;
                        info->flags |= kAudioUnitParameterFlag_HasClump;
                    }
                    *ioSize = sizeof(AudioUnitParameterInfo);
                    return noErr;
                }
            }
            return kAudioUnitErr_InvalidParameter;
        }

        case kAudioUnitProperty_ParameterStringFromValue: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (*ioSize < sizeof(AudioUnitParameterStringFromValue))
                return kAudioUnitErr_InvalidPropertyValue;
            if (!g_callbacks || !inst->rustCtx) return kAudioUnitErr_Uninitialized;
            AudioUnitParameterStringFromValue *sfv =
                (AudioUnitParameterStringFromValue *)outData;
            /* A NULL inValue means "format the parameter's current
             * value" per AudioUnitProperties.h. */
            double value = sfv->inValue
                ? (double)*sfv->inValue
                : g_callbacks->param_get_value(inst->rustCtx, sfv->inParamID);
            char buf[128];
            uint32_t len = g_callbacks->param_format_value(
                inst->rustCtx, sfv->inParamID, value, buf, sizeof(buf));
            if (len == 0) return kAudioUnitErr_InvalidParameter;
            /* Ownership transfers to the caller per the property's
             * Copy rule. Creation can fail (invalid UTF-8); noErr with
             * a NULL outString would hand the host a string it can't
             * use but believes exists. */
            CFStringRef str = CFStringCreateWithCString(NULL, buf,
                                                        kCFStringEncodingUTF8);
            if (!str) return kAudioUnitErr_InvalidParameter;
            sfv->outString = str;
            *ioSize = sizeof(AudioUnitParameterStringFromValue);
            return noErr;
        }

        case kAudioUnitProperty_ParameterValueFromString: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (*ioSize < sizeof(AudioUnitParameterValueFromString))
                return kAudioUnitErr_InvalidPropertyValue;
            if (!g_callbacks || !g_callbacks->param_parse_value || !inst->rustCtx)
                return kAudioUnitErr_Uninitialized;
            AudioUnitParameterValueFromString *vfs =
                (AudioUnitParameterValueFromString *)outData;
            if (!vfs->inString) return kAudioUnitErr_InvalidParameter;
            /* AU hands us a CFString; the Rust parser wants UTF-8. A
             * plain-numeric or enum-name entry fits well under 256. */
            char buf[256];
            if (!CFStringGetCString(vfs->inString, buf, sizeof(buf),
                                    kCFStringEncodingUTF8))
                return kAudioUnitErr_InvalidParameter;
            double plain = 0.0;
            if (!g_callbacks->param_parse_value(inst->rustCtx, vfs->inParamID,
                                                buf, &plain))
                return kAudioUnitErr_InvalidParameter;
            /* AU parameter values are plain (Float32) in the param's
             * own range - no normalize step. */
            vfs->outValue = (AudioUnitParameterValue)plain;
            *ioSize = sizeof(AudioUnitParameterValueFromString);
            return noErr;
        }

        case kAudioUnitProperty_ParameterValueStrings: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (!g_callbacks || !inst->rustCtx) return kAudioUnitErr_Uninitialized;
            /* `elem` is the AudioUnitParameterID. Only indexed params
             * (step_count > 0) have a value-name list; a host reads this
             * to build the enum's menu. Names come from the plugin's own
             * format_value, so an enum shows "Sine"/"Square". */
            const AuParamDescriptor *pd = NULL;
            for (uint32_t i = 0; i < g_num_params; i++)
                if (g_param_descriptors[i].id == elem) { pd = &g_param_descriptors[i]; break; }
            if (!pd || pd->step_count == 0) return kAudioUnitErr_InvalidProperty;
            if (*ioSize < sizeof(CFArrayRef)) return kAudioUnitErr_InvalidPropertyValue;
            /* step_count is values-1, so there are step_count + 1 values. */
            CFMutableArrayRef arr = CFArrayCreateMutable(NULL, pd->step_count + 1,
                                                         &kCFTypeArrayCallBacks);
            if (!arr) return kAudioUnitErr_InvalidParameter;
            for (uint32_t i = 0; i <= pd->step_count; i++) {
                char buf[128];
                uint32_t len = g_callbacks->param_format_value(
                    inst->rustCtx, elem, pd->min + (double)i, buf, sizeof(buf));
                CFStringRef s = CFStringCreateWithCString(
                    NULL, len > 0 ? buf : "", kCFStringEncodingUTF8);
                if (s) { CFArrayAppendValue(arr, s); CFRelease(s); }
            }
            /* Ownership transfers to the caller per the property's Copy rule. */
            *(CFArrayRef *)outData = arr;
            *ioSize = sizeof(CFArrayRef);
            return noErr;
        }

        case kAudioUnitProperty_ParameterClumpName: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (*ioSize < sizeof(AudioUnitParameterNameInfo))
                return kAudioUnitErr_InvalidPropertyValue;
            AudioUnitParameterNameInfo *cn = (AudioUnitParameterNameInfo *)outData;
            /* inID is the clumpID; hand back the param group's display name. */
            const char *name = au_v2_clump_name((uint32_t)cn->inID);
            if (!name) return kAudioUnitErr_InvalidPropertyValue;
            cn->outName = CFStringCreateWithCString(NULL, name, kCFStringEncodingUTF8);
            if (!cn->outName) return kAudioUnitErr_InvalidParameter;
            *ioSize = sizeof(AudioUnitParameterNameInfo);
            return noErr;
        }

        case kAudioUnitProperty_ShouldAllocateBuffer: {
            *(UInt32 *)outData = 1; // AU allocates its own buffers
            *ioSize = sizeof(UInt32);
            return noErr;
        }

        case kAudioUnitProperty_Latency: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            // Report the plugin's latency in seconds (samples / rate)
            // so the host's delay compensation aligns the track. The
            // Rust callback reads a cache refreshed each block, so this
            // main-thread query never takes the plugin lock. Staticlib
            // and shim are one binary, so `latency_samples` is always
            // present (no cross-binary version gate needed here).
            double secs = 0.0;
            if (g_callbacks && g_callbacks->latency_samples && inst->rustCtx &&
                inst->sampleRate > 0.0)
                secs = g_callbacks->latency_samples(inst->rustCtx) / inst->sampleRate;
            *(Float64 *)outData = secs;
            *ioSize = sizeof(Float64);
            return noErr;
        }

        case kAudioUnitProperty_TailTime: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            double secs = 0.0;
            if (g_callbacks && g_callbacks->tail_samples && inst->rustCtx &&
                inst->sampleRate > 0.0)
                secs = g_callbacks->tail_samples(inst->rustCtx) / inst->sampleRate;
            *(Float64 *)outData = secs;
            *ioSize = sizeof(Float64);
            return noErr;
        }

        case kAudioUnitProperty_BypassEffect: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            UInt32 bypassed = 0;
            if (g_descriptor->bypass_param_id != UINT32_MAX
                && g_callbacks && inst->rustCtx) {
                /* IS_BYPASS-flagged param: 0 = inactive, 1 = bypassed.
                 * Plain value is 0/1 for BoolParam (the common case). */
                double v = g_callbacks->param_get_value(
                    inst->rustCtx, g_descriptor->bypass_param_id);
                bypassed = (v >= 0.5) ? 1 : 0;
            }
            *(UInt32 *)outData = bypassed;
            *ioSize = sizeof(UInt32);
            return noErr;
        }

        case kAudioUnitProperty_LastRenderError: {
            *(OSStatus *)outData = noErr;
            *ioSize = sizeof(OSStatus);
            return noErr;
        }

        case kAudioUnitProperty_MIDIOutputCallbackInfo: {
            /* Hosts that read this expect one name per logical MIDI output;
             * `midiOutNum` on the callback carries the matching index. */
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (*ioSize < sizeof(CFArrayRef))
                return kAudioUnitErr_InvalidPropertyValue;
            uint32_t count = g_descriptor && g_descriptor->midi_output_ports
                                 ? g_descriptor->midi_output_ports : 1;
            CFMutableArrayRef arr = CFArrayCreateMutable(
                kCFAllocatorDefault, count, &kCFTypeArrayCallBacks);
            if (!arr) return kAudioUnitErr_FailedInitialization;
            for (uint32_t i = 0; i < count; i++) {
                CFStringRef name = CFStringCreateWithFormat(
                    kCFAllocatorDefault, NULL, CFSTR("Moose MIDI Out %u"), i + 1);
                if (!name) {
                    CFRelease(arr);
                    return kAudioUnitErr_FailedInitialization;
                }
                CFArrayAppendValue(arr, name);
                CFRelease(name);
            }
            *(CFArrayRef *)outData = arr;
            *ioSize = sizeof(CFArrayRef);
            return noErr;
        }

        case kAudioUnitProperty_MIDIOutputCallback: {
            /* Hosts typically only set this property; AU validators
             * sometimes read it back. Return what we have stored. */
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (*ioSize < sizeof(AUMIDIOutputCallbackStruct))
                return kAudioUnitErr_InvalidPropertyValue;
            AUMIDIOutputCallbackStruct *cb = (AUMIDIOutputCallbackStruct *)outData;
            cb->midiOutputCallback = inst->midiOutputCallback;
            cb->userData = inst->midiOutputUserData;
            *ioSize = sizeof(AUMIDIOutputCallbackStruct);
            return noErr;
        }

        case kAudioUnitProperty_AudioUnitMIDIProtocol: {
            if (scope != kAudioUnitScope_Global || elem != 0)
                return kAudioUnitErr_InvalidScope;
            if (*ioSize < sizeof(SInt32))
                return kAudioUnitErr_InvalidPropertyValue;
            *(SInt32 *)outData = g_descriptor->midi2_input
                ? kMIDIProtocol_2_0 : kMIDIProtocol_1_0;
            *ioSize = sizeof(SInt32);
            return noErr;
        }

        case kAudioUnitProperty_HostMIDIProtocol: {
            if (scope != kAudioUnitScope_Global || elem != 0)
                return kAudioUnitErr_InvalidScope;
            if (*ioSize < sizeof(SInt32))
                return kAudioUnitErr_InvalidPropertyValue;
            *(SInt32 *)outData = inst->hostMIDIProtocol;
            *ioSize = sizeof(SInt32);
            return noErr;
        }

        case kAudioUnitProperty_InPlaceProcessing: {
            // Only effects (with inputs) support in-place processing
            *(UInt32 *)outData = (g_descriptor->num_inputs > 0) ? 1 : 0;
            *ioSize = sizeof(UInt32);
            return noErr;
        }

        case kAudioUnitProperty_SupportedNumChannels: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            UInt32 nCfg = g_descriptor->num_layouts > 0 ? g_descriptor->num_layouts : 1;
            if (*ioSize < nCfg * sizeof(AUChannelInfo))
                return kAudioUnitErr_InvalidPropertyValue;
            AUChannelInfo *info = (AUChannelInfo *)outData;
            if (g_descriptor->num_layouts > 0) {
                // One AUChannelInfo per bus_layouts() entry, so the host
                // can pick any declared (in, out) config.
                for (UInt32 i = 0; i < nCfg; i++) {
                    info[i].inChannels = g_descriptor->layout_in_channels[i];
                    info[i].outChannels = g_descriptor->layout_out_channels[i];
                }
            } else {
                info[0].inChannels = (SInt16)g_descriptor->num_inputs;
                info[0].outChannels = (SInt16)g_descriptor->num_outputs;
            }
            *ioSize = nCfg * sizeof(AUChannelInfo);
            return noErr;
        }

        case kAudioUnitProperty_CocoaUI: {
            if (*ioSize < sizeof(AudioUnitCocoaViewInfo))
                return kAudioUnitErr_InvalidPropertyValue;

            AudioUnitCocoaViewInfo *viewInfo = (AudioUnitCocoaViewInfo *)outData;

            // Get our .component bundle URL via the AudioUnit's component
            AudioComponent comp = AudioComponentInstanceGetComponent(inst->componentInstance);
            CFStringRef bundleID = NULL;
            AudioComponentCopyName(comp, &bundleID);

            // Use dladdr to find our bundle path
            Dl_info dlInfo;
            if (dladdr((void*)au_v2_get_property, &dlInfo)) {
                // Walk up from the binary path to the .component bundle
                char path[2048];
                strncpy(path, dlInfo.dli_fname, sizeof(path));
                // Go up 3 levels: MacOS → Contents → *.component
                for (int i = 0; i < 3; i++) {
                    char *last = strrchr(path, '/');
                    if (last) *last = 0;
                }
                CFStringRef pathStr = CFStringCreateWithCString(NULL, path, kCFStringEncodingUTF8);
                viewInfo->mCocoaAUViewBundleLocation = CFURLCreateWithFileSystemPath(
                    NULL, pathStr, kCFURLPOSIXPathStyle, true);
                CFRelease(pathStr);
            }
            if (bundleID) CFRelease(bundleID);

            // The Cocoa view factory class is defined in
            // `au_v2_view.m` and registered with the ObjC runtime
            // automatically when the dylib loads (via the compiler's
            // `__objc_classlist`). `moose_au_view_factory_class_name`
            // returns this dylib's unique class name - see au_v2_view.m
            // for why each plugin needs its own.
            extern const char *moose_au_view_factory_class_name(void);
            viewInfo->mCocoaAUViewClass[0] = CFStringCreateWithCString(
                NULL, moose_au_view_factory_class_name(), kCFStringEncodingUTF8);

            *ioSize = sizeof(AudioUnitCocoaViewInfo);
            return noErr;
        }

        case 64000: { /* kMoosePrivateProperty_RustContext */
            *(void **)outData = inst->rustCtx;
            *ioSize = sizeof(void*);
            return noErr;
        }

        case 64001: { /* kMoosePrivateProperty_AuCallbacks */
            /* The Cocoa view methods (see au_v2_view.m) read `rustCtx`
             * and the callbacks table through the AU dispatch table
             * rather than touching dylib globals directly. That keeps
             * the view shim source identical across plugins, even
             * though each plugin compiles its own uniquely-named
             * class - every property fetch lands in the dylib that
             * owns the AU instance, which is always the correct one. */
            *(const AuCallbacks **)outData = g_callbacks;
            *ioSize = sizeof(void*);
            return noErr;
        }

        case kAudioUnitProperty_PresentPreset: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (*ioSize < sizeof(AUPreset)) return kAudioUnitErr_InvalidPropertyValue;
            // Hand the caller a struct copy. The host receives ownership
            // of the name reference and is expected to CFRelease it,
            // so we balance with an extra CFRetain here.
            *(AUPreset *)outData = inst->currentPreset;
            if (inst->currentPreset.presetName) {
                CFRetain(inst->currentPreset.presetName);
            }
            *ioSize = sizeof(AUPreset);
            return noErr;
        }

        case kAudioUnitProperty_FactoryPresets: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (!g_callbacks || !inst->rustCtx) return kAudioUnitErr_Uninitialized;
            uint32_t n = ensure_factory_presets(inst);
            if (n == 0) return kAudioUnitErr_InvalidProperty;
            if (*ioSize < sizeof(CFArrayRef)) return kAudioUnitErr_InvalidPropertyValue;
            // Borrowed-pointer array (no CF callbacks): the host
            // releases the array, the entries stay ours until Close.
            CFMutableArrayRef arr = CFArrayCreateMutable(NULL, n, NULL);
            if (!arr) return kAudioUnitErr_InvalidProperty;
            for (uint32_t i = 0; i < n; i++) {
                CFArrayAppendValue(arr, &inst->factoryPresets[i]);
            }
            *(CFArrayRef *)outData = arr;
            *ioSize = sizeof(CFArrayRef);
            return noErr;
        }

        case kAudioUnitProperty_AllParameterMIDIMappings: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            uint32_t n = count_midi_mapped_params();
            if (n == 0) return kAudioUnitErr_InvalidProperty;
            UInt32 needed = n * sizeof(AUParameterMIDIMapping);
            if (*ioSize < needed) return kAudioUnitErr_InvalidPropertyValue;
            AUParameterMIDIMapping *maps = (AUParameterMIDIMapping *)outData;
            uint32_t w = 0;
            for (uint32_t i = 0; i < g_num_params; i++) {
                const AuParamDescriptor *pd = &g_param_descriptors[i];
                if (pd->midi_status == 0) continue;
                AUParameterMIDIMapping *m = &maps[w++];
                memset(m, 0, sizeof(*m));
                m->mScope = kAudioUnitScope_Global;
                m->mParameterID = pd->id;
                if (pd->midi_channel < 0) {
                    // Any-channel: the channel nibble of mStatus is
                    // ignored when the flag is set.
                    m->mFlags = kAUParameterMIDIMapping_AnyChannelFlag;
                    m->mStatus = pd->midi_status;
                } else {
                    m->mStatus = (UInt8)(pd->midi_status | (pd->midi_channel & 0x0F));
                }
                m->mData1 = pd->midi_data1;
            }
            *ioSize = needed;
            return noErr;
        }

        case kAudioUnitProperty_ClassInfo: {
            // State save: build a CFDictionary using Apple's standard
            // preset keys. The "name" slot carries the current preset
            // name (set by the host via kAudioUnitProperty_PresentPreset
            // or by a prior ClassInfo round-trip), not the component
            // name. auval's "preset name is not retained" check reads
            // this slot.
            if (!g_callbacks || !inst->rustCtx) return kAudioUnitErr_Uninitialized;

            CFMutableDictionaryRef dict = CFDictionaryCreateMutable(NULL, 0,
                &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);

            // Standard AU keys
            SInt32 compType = (SInt32)FOURCC(g_descriptor->component_type);
            SInt32 compSubType = (SInt32)FOURCC(g_descriptor->component_subtype);
            SInt32 compMfr = (SInt32)FOURCC(g_descriptor->component_manufacturer);
            SInt32 compVer = (SInt32)g_descriptor->version;

            CFNumberRef nType = CFNumberCreate(NULL, kCFNumberSInt32Type, &compType);
            CFNumberRef nSub = CFNumberCreate(NULL, kCFNumberSInt32Type, &compSubType);
            CFNumberRef nMfr = CFNumberCreate(NULL, kCFNumberSInt32Type, &compMfr);
            CFNumberRef nVer = CFNumberCreate(NULL, kCFNumberSInt32Type, &compVer);
            CFStringRef sName = inst->currentPreset.presetName
                ? (CFStringRef)CFRetain(inst->currentPreset.presetName)
                : CFSTR("Untitled");

            CFDictionarySetValue(dict, CFSTR("type"), nType);
            CFDictionarySetValue(dict, CFSTR("subtype"), nSub);
            CFDictionarySetValue(dict, CFSTR("manufacturer"), nMfr);
            CFDictionarySetValue(dict, CFSTR("version"), nVer);
            CFDictionarySetValue(dict, CFSTR("name"), sName);

            CFRelease(nType); CFRelease(nSub); CFRelease(nMfr); CFRelease(nVer);
            if (inst->currentPreset.presetName) CFRelease(sName);

            // Plugin state blob
            uint8_t *data = NULL; uint32_t len = 0;
            g_callbacks->state_save(inst->rustCtx, &data, &len);
            if (data && len > 0) {
                CFDataRef cfData = CFDataCreate(NULL, data, len);
                CFDictionarySetValue(dict, CFSTR("truce_state"), cfData);
                CFRelease(cfData);
                g_callbacks->state_free(data, len);
            }

            *(CFPropertyListRef *)outData = dict;
            *ioSize = sizeof(CFPropertyListRef);
            return noErr;
        }

        default:
            return kAudioUnitErr_InvalidProperty;
    }
}

static OSStatus au_v2_set_property(void *self_, AudioUnitPropertyID prop,
                                    AudioUnitScope scope, AudioUnitElement elem,
                                    const void *inData, UInt32 inSize) {
    MooseAUv2 *inst = (MooseAUv2 *)self_;

    switch (prop) {
        case kAudioUnitProperty_StreamFormat: {
            if (inSize < sizeof(AudioStreamBasicDescription))
                return kAudioUnitErr_InvalidPropertyValue;
            const AudioStreamBasicDescription *asbd = (const AudioStreamBasicDescription *)inData;
            // Validate: must be non-interleaved float32 PCM
            if (asbd->mFormatID != kAudioFormatLinearPCM) return kAudioUnitErr_FormatNotSupported;
            if (!(asbd->mFormatFlags & kAudioFormatFlagIsFloat)) return kAudioUnitErr_FormatNotSupported;
            if (!(asbd->mFormatFlags & kAudioFormatFlagIsNonInterleaved)) return kAudioUnitErr_FormatNotSupported;

            // Validate channel count matches our bus configuration
            if (scope == kAudioUnitScope_Output) {
                if (g_descriptor->num_outputs == 0)
                    return kAudioUnitErr_InvalidElement;
                if (!channel_count_supported(asbd->mChannelsPerFrame, scope))
                    return kAudioUnitErr_FormatNotSupported;
                inst->outputFormat = *asbd;
                inst->sampleRate = asbd->mSampleRate;
            } else if (scope == kAudioUnitScope_Input) {
                if (g_descriptor->num_inputs == 0)
                    return kAudioUnitErr_InvalidElement;
                if (elem == 1) {
                    if (g_descriptor->sidechain_in_channels == 0)
                        return kAudioUnitErr_InvalidElement;
                    // The sidechain element is fixed at its declared width.
                    if (asbd->mChannelsPerFrame != g_descriptor->sidechain_in_channels)
                        return kAudioUnitErr_FormatNotSupported;
                    inst->sidechainFormat = *asbd;
                } else if (elem == 0) {
                    if (!channel_count_supported(asbd->mChannelsPerFrame, scope))
                        return kAudioUnitErr_FormatNotSupported;
                    inst->inputFormat = *asbd;
                } else {
                    return kAudioUnitErr_InvalidElement;
                }
            } else {
                return kAudioUnitErr_InvalidScope;
            }
            return noErr;
        }

        case kAudioUnitProperty_SampleRate: {
            inst->sampleRate = *(const Float64 *)inData;
            inst->outputFormat.mSampleRate = inst->sampleRate;
            inst->inputFormat.mSampleRate = inst->sampleRate;
            inst->sidechainFormat.mSampleRate = inst->sampleRate;
            return noErr;
        }

        case kAudioUnitProperty_MaximumFramesPerSlice: {
            inst->maxFramesPerSlice = *(const UInt32 *)inData;
            notify_listeners(inst, kAudioUnitProperty_MaximumFramesPerSlice,
                           kAudioUnitScope_Global, 0);
            return noErr;
        }

        case kAudioUnitProperty_OfflineRender: {
            if (inSize < sizeof(UInt32)) return kAudioUnitErr_InvalidPropertyValue;
            inst->offlineRender = *(const UInt32 *)inData;
            // Forward to the plugin as a ProcessMode discriminant (0
            // realtime, 2 offline). The Rust side reads it in reset (the
            // host re-initializes after setting this) and per process
            // block. Gate on the callback pointer so a plugin binary
            // built before this ABI field is never called through it.
            if (g_callbacks && g_callbacks->set_render_mode && inst->rustCtx)
                g_callbacks->set_render_mode(inst->rustCtx, inst->offlineRender ? 2u : 0u);
            return noErr;
        }

        case kAudioUnitProperty_SetRenderCallback: {
            if (scope != kAudioUnitScope_Input) return kAudioUnitErr_InvalidScope;
            const AURenderCallbackStruct *cb = (const AURenderCallbackStruct *)inData;
            if (elem == 1) {
                if (g_descriptor->sidechain_in_channels == 0)
                    return kAudioUnitErr_InvalidElement;
                inst->sidechainInputCallback = cb->inputProc;
                inst->sidechainInputCallbackRefCon = cb->inputProcRefCon;
                inst->sidechainSourceUnit = NULL;
            } else {
                inst->inputCallback = cb->inputProc;
                inst->inputCallbackRefCon = cb->inputProcRefCon;
                inst->sourceUnit = NULL;
            }
            return noErr;
        }

        case kAudioUnitProperty_MakeConnection: {
            if (scope != kAudioUnitScope_Input) return kAudioUnitErr_InvalidScope;
            const AudioUnitConnection *conn = (const AudioUnitConnection *)inData;
            if (elem == 1) {
                if (g_descriptor->sidechain_in_channels == 0)
                    return kAudioUnitErr_InvalidElement;
                inst->sidechainSourceUnit = conn->sourceAudioUnit;
                inst->sidechainSourceOutputBus = conn->sourceOutputNumber;
                inst->sidechainInputCallback = NULL;
                inst->sidechainInputCallbackRefCon = NULL;
            } else {
                inst->sourceUnit = conn->sourceAudioUnit;
                inst->sourceOutputBus = conn->sourceOutputNumber;
                inst->inputCallback = NULL;
                inst->inputCallbackRefCon = NULL;
            }
            return noErr;
        }

        case kAudioUnitProperty_ShouldAllocateBuffer:
            return noErr; // accept but ignore

        case kAudioUnitProperty_HostCallbacks: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            // Host may pass a shorter struct than sizeof(HostCallbackInfo);
            // copy only what was supplied. The unused function pointers
            // stay NULL, which is what the SDK sentinel value means.
            memset(&inst->hostCallbacks, 0, sizeof(inst->hostCallbacks));
            UInt32 copy = inSize < sizeof(inst->hostCallbacks) ? inSize : sizeof(inst->hostCallbacks);
            memcpy(&inst->hostCallbacks, inData, copy);
            inst->hasHostCallbacks = (inst->hostCallbacks.beatAndTempoProc ||
                                      inst->hostCallbacks.musicalTimeLocationProc ||
                                      inst->hostCallbacks.transportStateProc ||
                                      inst->hostCallbacks.transportStateProc2);
            return noErr;
        }

        case kAudioUnitProperty_MIDIOutputCallback: {
            /* Host registers its MIDI output callback. We stash it
             * and call it after each render block with whatever
             * events the plugin pushed into `output_events`. */
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (!inData || inSize < sizeof(AUMIDIOutputCallbackStruct))
                return kAudioUnitErr_InvalidPropertyValue;
            const AUMIDIOutputCallbackStruct *cb =
                (const AUMIDIOutputCallbackStruct *)inData;
            inst->midiOutputCallback = cb->midiOutputCallback;
            inst->midiOutputUserData = cb->userData;
            return noErr;
        }

        case kAudioUnitProperty_MIDIOutputEventListCallback: {
            if (scope != kAudioUnitScope_Global || elem != 0)
                return kAudioUnitErr_InvalidScope;
            if (!g_descriptor || !g_descriptor->has_midi_output)
                return kAudioUnitErr_InvalidProperty;
            if (!inData || inSize != sizeof(AUMIDIEventListBlock))
                return kAudioUnitErr_InvalidPropertyValue;
            AUMIDIEventListBlock next = *(const AUMIDIEventListBlock *)inData;
            if (next) next = Block_copy(next);
            if (inst->midiOutputEventListBlock)
                Block_release(inst->midiOutputEventListBlock);
            inst->midiOutputEventListBlock = next;
            return noErr;
        }

        case kAudioUnitProperty_HostMIDIProtocol: {
            if (scope != kAudioUnitScope_Global || elem != 0)
                return kAudioUnitErr_InvalidScope;
            if (!g_descriptor || !g_descriptor->has_midi_output)
                return kAudioUnitErr_InvalidProperty;
            if (inst->initialized) return kAudioUnitErr_Initialized;
            if (!inData || inSize != sizeof(SInt32))
                return kAudioUnitErr_InvalidPropertyValue;
            SInt32 protocol = *(const SInt32 *)inData;
            if (protocol != kMIDIProtocol_1_0 && protocol != kMIDIProtocol_2_0)
                return kAudioUnitErr_InvalidPropertyValue;
            inst->hostMIDIProtocol = protocol;
            return noErr;
        }

        case kAudioUnitProperty_BypassEffect: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (!inData || inSize < sizeof(UInt32))
                return kAudioUnitErr_InvalidPropertyValue;
            UInt32 bypassed = *(const UInt32 *)inData;
            if (g_descriptor->bypass_param_id != UINT32_MAX
                && g_callbacks && inst->rustCtx) {
                g_callbacks->param_set_value(
                    inst->rustCtx,
                    g_descriptor->bypass_param_id,
                    bypassed ? 1.0 : 0.0);
            }
            return noErr;
        }
        case kAudioUnitProperty_PresentPreset: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (!inData || inSize < sizeof(AUPreset))
                return kAudioUnitErr_InvalidPropertyValue;
            const AUPreset *src = (const AUPreset *)inData;
            // Factory recall: a non-negative number selects from the
            // factory table and applies through the same state path
            // session restore uses. Validate + load before mutating
            // currentPreset so a bad index leaves state untouched.
            if (src->presetNumber >= 0) {
                uint32_t n = ensure_factory_presets(inst);
                if (!g_callbacks || !inst->rustCtx ||
                    (uint32_t)src->presetNumber >= n ||
                    !g_callbacks->factory_preset_load(
                        inst->rustCtx, (uint32_t)src->presetNumber))
                    return kAudioUnitErr_InvalidPropertyValue;
            }
            // Release the old name and retain the incoming one. The
            // host owns its copy independently of ours.
            if (inst->currentPreset.presetName) {
                CFRelease(inst->currentPreset.presetName);
                inst->currentPreset.presetName = NULL;
            }
            inst->currentPreset.presetNumber = src->presetNumber;
            if (src->presetName) {
                inst->currentPreset.presetName = (CFStringRef)CFRetain(src->presetName);
            } else if (src->presetNumber >= 0) {
                // Hosts may pass just the number; fall back to the
                // table's canonical name so ClassInfo keeps a name.
                inst->currentPreset.presetName = (CFStringRef)CFRetain(
                    inst->factoryPresets[src->presetNumber].presetName);
            }
            notify_listeners(inst, kAudioUnitProperty_PresentPreset,
                             kAudioUnitScope_Global, 0);
            return noErr;
        }

        case kAudioUnitProperty_ClassInfo: {
            // State load
            if (!g_callbacks || !inst->rustCtx) return kAudioUnitErr_Uninitialized;
            if (!inData || inSize < sizeof(CFPropertyListRef))
                return kAudioUnitErr_InvalidPropertyValue;

            CFPropertyListRef plist = *(const CFPropertyListRef *)inData;
            if (!plist) return kAudioUnitErr_InvalidPropertyValue;

            // Verify it's actually a dictionary
            if (CFGetTypeID(plist) != CFDictionaryGetTypeID())
                return kAudioUnitErr_InvalidPropertyValue;

            CFDictionaryRef dict = (CFDictionaryRef)plist;

            // Round-trip the preset name through the standard "name"
            // slot so auval's "preset name is not retained" check
            // passes. We accept whatever the host wrote into ClassInfo
            // even if it never called PresentPreset.
            CFStringRef savedName = CFDictionaryGetValue(dict, CFSTR("name"));
            if (savedName && CFGetTypeID(savedName) == CFStringGetTypeID()) {
                if (inst->currentPreset.presetName) {
                    CFRelease(inst->currentPreset.presetName);
                }
                inst->currentPreset.presetName = (CFStringRef)CFRetain(savedName);
                // A user-loaded blob isn't tied to a factory preset
                // index, so reset the number to -1 per Apple convention.
                inst->currentPreset.presetNumber = -1;
                notify_listeners(inst, kAudioUnitProperty_PresentPreset,
                                 kAudioUnitScope_Global, 0);
            }

            CFDataRef cfData = CFDictionaryGetValue(dict, CFSTR("truce_state"));
            if (cfData && CFGetTypeID(cfData) == CFDataGetTypeID()) {
                const uint8_t *bytes = CFDataGetBytePtr(cfData);
                uint32_t len = (uint32_t)CFDataGetLength(cfData);
                g_callbacks->state_load(inst->rustCtx, bytes, len);
            } else if (g_callbacks->legacy_state_key_count
                       && g_callbacks->state_load_foreign) {
                /* No moose entry: a pre-moose build stored its state
                 * under its own dictionary key. Probe the keys the
                 * plugin declared in moose.toml's [plugin.legacy_state]
                 * (first present + accepted wins) so its migrate_state
                 * hook can translate the old session. */
                uint32_t n = g_callbacks->legacy_state_key_count(inst->rustCtx);
                for (uint32_t i = 0; i < n; i++) {
                    const char *keyName =
                        g_callbacks->legacy_state_key_at(inst->rustCtx, i);
                    if (!keyName) continue;
                    CFStringRef k = CFStringCreateWithCString(
                        NULL, keyName, kCFStringEncodingUTF8);
                    if (!k) continue;
                    CFTypeRef v = CFDictionaryGetValue(dict, k);
                    CFRelease(k);
                    if (!v || CFGetTypeID(v) != CFDataGetTypeID()) continue;
                    const uint8_t *bytes = CFDataGetBytePtr((CFDataRef)v);
                    uint32_t len = (uint32_t)CFDataGetLength((CFDataRef)v);
                    if (g_callbacks->state_load_foreign(inst->rustCtx,
                                                        keyName, bytes, len))
                        break;
                }
            }
            return noErr;
        }

        default:
            return kAudioUnitErr_InvalidProperty;
    }
}

// ---------------------------------------------------------------------------
// Get/Set Parameter
// ---------------------------------------------------------------------------

static OSStatus au_v2_get_parameter(void *self_, AudioUnitParameterID id,
                                     AudioUnitScope scope, AudioUnitElement elem,
                                     AudioUnitParameterValue *outValue) {
    (void)scope; (void)elem;
    MooseAUv2 *inst = (MooseAUv2 *)self_;
    if (!g_callbacks || !inst->rustCtx) return kAudioUnitErr_Uninitialized;
    *outValue = (AudioUnitParameterValue)g_callbacks->param_get_value(inst->rustCtx, id);
    return noErr;
}

/* Current thread's kernel id. AU is macOS-only, so pthread_threadid_np
 * is always available. */
static uint64_t au_current_tid(void) {
    uint64_t tid = 0;
    pthread_threadid_np(pthread_self(), &tid);
    return tid;
}

/* True when the caller runs on the instance's audio (render) thread and
 * so may touch the lock-free paramEvents queue. */
static int au_on_audio_thread(const MooseAUv2 *inst) {
    return au_current_tid() ==
           __atomic_load_n(&inst->audioThreadId, __ATOMIC_RELAXED);
}

/* Append a host parameter-automation event to the render-block queue.
 * MUST be called on the audio thread (callers gate on
 * au_on_audio_thread). When the queue is full, coalesce onto the most
 * recent event for the same parameter (latest-value-wins) so dense
 * automation stays responsive rather than dropping the newest value. */
static void enqueue_param_event(MooseAUv2 *inst, AudioUnitParameterID id,
                                AudioUnitParameterValue value,
                                UInt32 sampleOffset) {
    if (inst->paramEventCount >= AU_PARAM_EVENT_CAP) {
        inst->paramOverflow = 1;
        for (uint32_t i = inst->paramEventCount; i > 0; i--) {
            if (inst->paramEvents[i - 1].param_id == id) {
                inst->paramEvents[i - 1].sample_offset = sampleOffset;
                inst->paramEvents[i - 1].value = (float)value;
                return;
            }
        }
        return;
    }
    AuParamEvent *ev = &inst->paramEvents[inst->paramEventCount++];
    ev->sample_offset = sampleOffset;
    ev->param_id = id;
    ev->value = (float)value;
}

static OSStatus au_v2_set_parameter(void *self_, AudioUnitParameterID id,
                                     AudioUnitScope scope, AudioUnitElement elem,
                                     AudioUnitParameterValue value, UInt32 bufferOffset) {
    (void)scope; (void)elem;
    MooseAUv2 *inst = (MooseAUv2 *)self_;
    if (!g_callbacks || !inst->rustCtx) return kAudioUnitErr_Uninitialized;
    g_callbacks->param_set_value(inst->rustCtx, id, (double)value);
    /* Also surface the change to cb_process as a ParamChange so the
     * editor follows host automation - but only when the host called us
     * on the audio thread (automation playback). A main-thread call
     * (generic view, preset apply) races the render-thread drain, and
     * the value it just set through param_set_value is already live for
     * the DSP; the editor reads that value directly. */
    if (au_on_audio_thread(inst)) {
        enqueue_param_event(inst, id, value, bufferOffset);
    }
    return noErr;
}

static OSStatus au_v2_schedule_parameters(void *self_,
                                           const AudioUnitParameterEvent *events,
                                           UInt32 numEvents) {
    MooseAUv2 *inst = (MooseAUv2 *)self_;
    if (!g_callbacks || !inst->rustCtx) return kAudioUnitErr_Uninitialized;
    /* The host calls this on the audio thread as part of the render
     * pull, immediately before au_v2_render. Record the thread so
     * set_parameter can recognize audio-thread automation, then enqueue
     * directly - same thread as the drain, no lock. */
    __atomic_store_n(&inst->audioThreadId, au_current_tid(), __ATOMIC_RELAXED);
    for (UInt32 i = 0; i < numEvents; i++) {
        if (events[i].eventType == kParameterEvent_Immediate) {
            g_callbacks->param_set_value(inst->rustCtx,
                events[i].parameter, (double)events[i].eventValues.immediate.value);
            enqueue_param_event(inst, events[i].parameter,
                events[i].eventValues.immediate.value,
                events[i].eventValues.immediate.bufferOffset);
        } else if (events[i].eventType == kParameterEvent_Ramped) {
            /* Deliver the ramp's end value at the start of this slice.
             * Moose's per-param smoother interpolates from the current
             * value toward the target; this isn't sample-accurate AU ramp
             * reproduction but matches how moose-vst3 treats VST3
             * parameter ramps. startBufferOffset is signed and may be
             * negative (ramp began in an earlier slice) or past this
             * slice, so it can't be used as an in-block offset (auval's
             * ramped-scheduling test fails otherwise). ponytail: ramp lands at
             * offset 0, honor startBufferOffset if ramp timing matters. */
            AudioUnitParameterValue value = events[i].eventValues.ramp.endValue;
            UInt32 offset = 0;
            g_callbacks->param_set_value(inst->rustCtx,
                events[i].parameter, (double)value);
            enqueue_param_event(inst, events[i].parameter, value, offset);
        }
    }
    return noErr;
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

/* Fill `out` from HostCallbackInfo. Each proc is optional - missing
 * callbacks leave their corresponding fields at zero. `valid` is set
 * to 1 as long as at least one proc returned successfully. */
static void fill_transport_snapshot(MooseAUv2 *inst,
                                     const AudioTimeStamp *ts,
                                     AuTransportSnapshot *out) {
    memset(out, 0, sizeof(*out));
    if (!inst->hasHostCallbacks) return;
    void *ud = inst->hostCallbacks.hostUserData;
    int ok = 0;

    if (inst->hostCallbacks.beatAndTempoProc) {
        Float64 beat = 0.0, tempo = 0.0;
        if (inst->hostCallbacks.beatAndTempoProc(ud, &beat, &tempo) == noErr) {
            out->position_beats = beat;
            out->tempo = tempo;
            ok = 1;
        }
    }
    if (inst->hostCallbacks.musicalTimeLocationProc) {
        UInt32 delta = 0;
        Float32 tsig_num = 0.0f;
        UInt32 tsig_den = 0;
        Float64 downbeat = 0.0;
        if (inst->hostCallbacks.musicalTimeLocationProc(ud, &delta, &tsig_num,
                                                         &tsig_den, &downbeat) == noErr) {
            out->time_sig_num = (int32_t)tsig_num;
            out->time_sig_den = (int32_t)tsig_den;
            out->bar_start_beats = downbeat;
            ok = 1;
        }
    }
    if (inst->hostCallbacks.transportStateProc2) {
        Boolean playing = false, recording = false, cycling = false, changed = false;
        Float64 samplePos = 0.0, cycleStart = 0.0, cycleEnd = 0.0;
        if (inst->hostCallbacks.transportStateProc2(ud, &playing, &recording,
                                                     &changed, &samplePos,
                                                     &cycling, &cycleStart, &cycleEnd) == noErr) {
            out->playing = playing ? 1 : 0;
            out->recording = recording ? 1 : 0;
            out->loop_active = cycling ? 1 : 0;
            out->position_samples = samplePos;
            out->loop_start_beats = cycleStart;
            out->loop_end_beats = cycleEnd;
            ok = 1;
        }
    } else if (inst->hostCallbacks.transportStateProc) {
        Boolean playing = false, changed = false, cycling = false;
        Float64 samplePos = 0.0, cycleStart = 0.0, cycleEnd = 0.0;
        if (inst->hostCallbacks.transportStateProc(ud, &playing, &changed,
                                                    &samplePos, &cycling,
                                                    &cycleStart, &cycleEnd) == noErr) {
            out->playing = playing ? 1 : 0;
            out->loop_active = cycling ? 1 : 0;
            out->position_samples = samplePos;
            out->loop_start_beats = cycleStart;
            out->loop_end_beats = cycleEnd;
            ok = 1;
        }
    }
    if (ok == 0 && ts && (ts->mFlags & kAudioTimeStampSampleTimeValid)) {
        // Fall back to the render timestamp if the host has no transport
        // procs - at least gives the plugin a sample position.
        out->position_samples = ts->mSampleTime;
        ok = 1;
    }
    out->valid = ok;
}

static void au_v2_fire_render_notifies(MooseAUv2 *inst,
                                       AudioUnitRenderActionFlags action,
                                       AudioUnitRenderActionFlags *ioFlags,
                                       const AudioTimeStamp *ts, UInt32 bus,
                                       UInt32 frames, AudioBufferList *ioData);

static OSStatus au_v2_render(void *self_,
                              AudioUnitRenderActionFlags *ioFlags,
                              const AudioTimeStamp *inTimeStamp,
                              UInt32 inBusNumber,
                              UInt32 inFrameCount,
                              AudioBufferList *ioData) {
    (void)inBusNumber;
    MooseAUv2 *inst = (MooseAUv2 *)self_;
    if (!inst->initialized || !g_callbacks || !inst->rustCtx)
        return kAudioUnitErr_Uninitialized;

    // Record the audio thread so au_v2_set_parameter can tell whether a
    // host param call is audio-thread automation (safe to queue) or a
    // main-thread set (must not touch paramEvents).
    __atomic_store_n(&inst->audioThreadId, au_current_tid(), __ATOMIC_RELAXED);

    // Clear the output-is-silence flag - we produce audio
    if (ioFlags) *ioFlags &= ~kAudioUnitRenderAction_OutputIsSilence;

    if (inFrameCount > inst->maxFramesPerSlice)
        return kAudioUnitErr_TooManyFramesToProcess;

    // Pre-render notify (before the input pull), so a metering / offline
    // harness observing the graph sees the block start.
    au_v2_fire_render_notifies(inst, kAudioUnitRenderAction_PreRender,
                               ioFlags, inTimeStamp, inBusNumber, inFrameCount, ioData);


    // Pull input for effects. Channel counts come from the negotiated
    // per-instance stream format so a multi-layout plugin runs at the
    // width the host selected. Unlike AUv3, AUv2 does not need a dummy
    // output bus: an input-only or MIDI-only unit keeps numOut == 0.
    uint32_t numIn = g_descriptor->num_inputs > 0 ? inst->inputFormat.mChannelsPerFrame : 0;
    uint32_t numOut = inst->outputFormat.mChannelsPerFrame;
    // The plugin sees a flat input array [main..., sidechain...], but the
    // sidechain stages into buffers past BOTH the main-input and output
    // regions (which share [0, base)), so a wider output never clobbers
    // sidechain data mid-block. Clamp so base + scCh fits the buffer array.
    uint32_t base = numIn > numOut ? numIn : numOut;
    uint32_t scCh = g_descriptor->sidechain_in_channels;
    if (base + scCh > 32) scCh = base < 32 ? 32 - base : 0;
    const bool mainInputActive = numIn > 0 &&
        (inst->inputCallback != NULL || inst->sourceUnit != NULL);
    const bool sidechainActive = scCh > 0 &&
        (inst->sidechainInputCallback != NULL || inst->sidechainSourceUnit != NULL);

    if (numIn > 0) {
        // Build a temporary ABL pointing to our buffers for the input pull.
        // Pull all `numIn` main-input channels - do NOT clamp to
        // ioData->mNumberBuffers (the OUTPUT width). An N-in/M-out layout
        // with N>M (a 2->1 sum, a 4->2 downmix) would otherwise pull only M
        // channels and leave the rest stale/aliased. `outputBuffers` is
        // staged to base + scCh (base = max(numIn, numOut)), so there's
        // room for all numIn.
        UInt32 pullBufCount = numIn < 32 ? numIn : 32;
        // Use stack-allocated ABL
        char ablStorage[sizeof(AudioBufferList) + sizeof(AudioBuffer) * 31];
        AudioBufferList *pullABL = (AudioBufferList *)ablStorage;
        pullABL->mNumberBuffers = pullBufCount;
        for (UInt32 c = 0; c < pullBufCount; c++) {
            pullABL->mBuffers[c].mNumberChannels = 1;
            pullABL->mBuffers[c].mDataByteSize = inFrameCount * sizeof(float);
            pullABL->mBuffers[c].mData = inst->outputBuffers[c]; // pull into our buffers
        }

        if (inst->inputCallback) {
            AudioUnitRenderActionFlags pullFlags = 0;
            OSStatus err = inst->inputCallback(inst->inputCallbackRefCon,
                &pullFlags, inTimeStamp, 0, inFrameCount, pullABL);
            if (err != noErr) return err;
            // Copy back any pointer changes the callback may have made
            for (UInt32 c = 0; c < pullBufCount; c++) {
                if (pullABL->mBuffers[c].mData != inst->outputBuffers[c]) {
                    memcpy(inst->outputBuffers[c], pullABL->mBuffers[c].mData,
                           inFrameCount * sizeof(float));
                }
            }
        } else if (inst->sourceUnit) {
            AudioUnitRenderActionFlags pullFlags = 0;
            OSStatus err = AudioUnitRender(inst->sourceUnit, &pullFlags,
                inTimeStamp, inst->sourceOutputBus, inFrameCount, pullABL);
            if (err != noErr) return err;
            for (UInt32 c = 0; c < pullBufCount; c++) {
                if (pullABL->mBuffers[c].mData != inst->outputBuffers[c]) {
                    memcpy(inst->outputBuffers[c], pullABL->mBuffers[c].mData,
                           inFrameCount * sizeof(float));
                }
            }
        } else {
            for (uint32_t c = 0; c < pullBufCount; c++)
                memset(inst->outputBuffers[c], 0, inFrameCount * sizeof(float));
        }
    }

    // Pull the sidechain input element (element 1) into the buffers past
    // both the main-input and output regions (index `base + c`), so a
    // wider output never overwrites it mid-block. An unconnected sidechain
    // reads as silence.
    if (scCh > 0) {
        char scAblStorage[sizeof(AudioBufferList) + sizeof(AudioBuffer) * 31];
        AudioBufferList *scABL = (AudioBufferList *)scAblStorage;
        scABL->mNumberBuffers = scCh;
        for (UInt32 c = 0; c < scCh; c++) {
            scABL->mBuffers[c].mNumberChannels = 1;
            scABL->mBuffers[c].mDataByteSize = inFrameCount * sizeof(float);
            scABL->mBuffers[c].mData = inst->outputBuffers[base + c];
        }
        AudioUnitRenderActionFlags scFlags = 0;
        OSStatus scErr = -1;
        if (inst->sidechainInputCallback)
            scErr = inst->sidechainInputCallback(inst->sidechainInputCallbackRefCon,
                &scFlags, inTimeStamp, 1, inFrameCount, scABL);
        else if (inst->sidechainSourceUnit)
            scErr = AudioUnitRender(inst->sidechainSourceUnit, &scFlags,
                inTimeStamp, inst->sidechainSourceOutputBus, inFrameCount, scABL);
        if (scErr == noErr) {
            for (UInt32 c = 0; c < scCh; c++)
                if (scABL->mBuffers[c].mData != inst->outputBuffers[base + c])
                    memcpy(inst->outputBuffers[base + c], scABL->mBuffers[c].mData,
                           inFrameCount * sizeof(float));
        } else {
            // No source connected (or the pull failed): feed silence.
            for (uint32_t c = 0; c < scCh; c++)
                memset(inst->outputBuffers[base + c], 0, inFrameCount * sizeof(float));
        }
    }

    // Save host's original buffer pointers - we MUST write back to these
    void *hostBufs[32] = {0};
    for (uint32_t c = 0; c < ioData->mNumberBuffers && c < 32; c++)
        hostBufs[c] = ioData->mBuffers[c].mData;

    // Build channel pointers - process into our internal buffers
    const float *inPtrs[32];
    float *outPtrs[32];

    for (uint32_t c = 0; c < numIn && c < 32; c++)
        inPtrs[c] = (const float *)inst->outputBuffers[c];
    // Sidechain channels follow the main ones in the flat array the plugin
    // sees (index numIn + c), but read from the non-aliasing staging
    // buffers at base + c.
    for (uint32_t c = 0; c < scCh && numIn + c < 32; c++)
        inPtrs[numIn + c] = (const float *)inst->outputBuffers[base + c];
    for (uint32_t c = 0; c < numOut && c < 32; c++)
        outPtrs[c] = inst->outputBuffers[c];


    AuTransportSnapshot transport;
    fill_transport_snapshot(inst, inTimeStamp, &transport);

    /* Reject out-of-block timestamps instead of changing their meaning. */
    int invalidEventTime = 0;
    for (uint32_t i = 0; i < inst->midiCount; i++) {
        if (inst->midiBuffer[i].sample_offset >= inFrameCount)
            invalidEventTime = 1;
    }
    for (uint32_t i = 0; i < inst->paramEventCount; i++) {
        if (inst->paramEvents[i].sample_offset >= inFrameCount)
            invalidEventTime = 1;
    }
    if (invalidEventTime) {
        inst->midiCount = 0;
        inst->midiOverflow = 0;
        inst->paramEventCount = 0;
        inst->paramOverflow = 0;
        return kAudioUnitErr_InvalidParameter;
    }

    if (inst->midiOverflow || inst->paramOverflow) {
        inst->midiCount = 0;
        inst->midiOverflow = 0;
        inst->paramEventCount = 0;
        inst->paramOverflow = 0;
        return kAudioUnitErr_MIDIOutputBufferFull;
    }

    /* MIDI 1 byte events and native UMP event-list input share the same
     * bounded lane. Rust returns a boundary status before DSP runs if exact
     * storage cannot retain the complete block. */
    uint32_t processStatus = g_callbacks->process_native_v11(
        inst->rustCtx, inPtrs, outPtrs, numIn + scCh, numOut,
        (mainInputActive ? 1u : 0u) | (sidechainActive ? 2u : 0u),
        numOut > 0 ? 1u : 0u,
        inFrameCount,
        inst->midiBuffer, inst->midiCount, inst->midiOverflow,
        inst->paramEvents, inst->paramEventCount, inst->paramOverflow,
        &transport);
    inst->midiCount = 0;
    inst->midiOverflow = 0;
    inst->paramEventCount = 0;
    inst->paramOverflow = 0;
    if (processStatus != AU_PROCESS_OK && g_callbacks->finish_output_events)
        g_callbacks->finish_output_events(inst->rustCtx, AU_OUTPUT_INVALID);
    if (processStatus == AU_PROCESS_QUEUE_FULL)
        return kAudioUnitErr_MIDIOutputBufferFull;
    if (processStatus != AU_PROCESS_OK)
        return kAudioUnitErr_InvalidParameter;

    /* Drain the single sorted native lane. The legacy callback carries byte
     * MIDI/SysEx; the event-list block carries source UMP unchanged and the
     * Apple AU boundary owns any host-protocol conversion. */
    OSStatus midiOutputStatus = noErr;
    bool hostRefusedOutput = false;
    /* Events emitted with no host receiver (auval, or a host with no MIDI
     * out connected) are dropped: the plugin sees Unsupported, but the
     * audio render still succeeds. */
    bool noReceiver = false;
    const bool umpTimeValid =
        (inTimeStamp->mFlags & kAudioTimeStampSampleTimeValid) &&
        isfinite(inTimeStamp->mSampleTime) &&
        inTimeStamp->mSampleTime >= (double)INT64_MIN &&
        inTimeStamp->mSampleTime < (double)INT64_MAX &&
        trunc(inTimeStamp->mSampleTime) == inTimeStamp->mSampleTime;
    if (inst->midiOutputCallback || inst->midiOutputEventListBlock) {
        uint32_t carriers = 0;
        if (inst->midiOutputCallback) carriers |= AU_NATIVE_CARRIER_BYTES;
        if (inst->midiOutputEventListBlock) carriers |= AU_NATIVE_CARRIER_UMP;
        g_callbacks->begin_output_events_v10(
            inst->rustCtx, carriers, inFrameCount,
            (uint32_t)inst->hostMIDIProtocol, UINT32_MAX,
            umpTimeValid ? 1u : 0u,
            inst->paramFeedbackAvailable ? 1u : 0u);
        for (;;) {
            AuNativeEvent ev = {0};
            uint32_t status = g_callbacks->next_output_event(inst->rustCtx, &ev);
            if (status == AU_OUTPUT_END) break;
            if (status == AU_OUTPUT_QUEUE_FULL) {
                midiOutputStatus = kAudioUnitErr_MIDIOutputBufferFull;
                break;
            }
            if (status == AU_OUTPUT_UNSUPPORTED) {
                midiOutputStatus = kAudioUnitErr_FormatNotSupported;
                break;
            }
            if (status != AU_OUTPUT_EMITTED) {
                midiOutputStatus = kAudioUnitErr_InvalidParameter;
                break;
            }

            if (ev.kind == AU_NATIVE_EVENT_UMP) {
                uint32_t messageType = (ev.words[0] >> 28) & 0x0Fu;
                if (!inst->midiOutputEventListBlock ||
                    (ev.protocol != kMIDIProtocol_1_0 &&
                     ev.protocol != kMIDIProtocol_2_0) ||
                    (inst->hostMIDIProtocol != kMIDIProtocol_1_0 &&
                     inst->hostMIDIProtocol != kMIDIProtocol_2_0) ||
                    ev.data_len != au_ump_word_count(ev.words[0]) ||
                    !au_ump_protocol_accepts((MIDIProtocolID)ev.protocol, messageType) ||
                    !umpTimeValid) {
                    midiOutputStatus = kAudioUnitErr_InvalidParameter;
                    break;
                }
                MIDIEventList *list = (MIDIEventList *)inst->sysexPacketBuf;
                MIDIEventPacket *packet = MIDIEventListInit(
                    list, (MIDIProtocolID)ev.protocol);
                if (!MIDIEventListAdd(list, inst->sysexPacketBufSize, packet,
                                      ev.sample_offset, ev.data_len, ev.words)) {
                    midiOutputStatus = kAudioUnitErr_MIDIOutputBufferFull;
                    break;
                }
                OSStatus sent = inst->midiOutputEventListBlock(
                    (AUEventSampleTime)inTimeStamp->mSampleTime,
                    (uint8_t)ev.port, list);
                if (sent != noErr) {
                    midiOutputStatus = sent;
                    hostRefusedOutput = true;
                    break;
                }
                continue;
            }

            const Byte *data = NULL;
            ByteCount len = 0;
            if (!inst->midiOutputCallback) {
                midiOutputStatus = kAudioUnitErr_FormatNotSupported;
                break;
            } else if (ev.kind == AU_NATIVE_EVENT_MIDI1 &&
                ev.data_len >= 1 && ev.data_len <= 3) {
                data = ev.midi;
                len = ev.data_len;
            } else if (ev.kind == AU_NATIVE_EVENT_SYSEX &&
                       ev.data_len <= inst->sysexFrameScratchSize - 2 &&
                       (ev.sysex || ev.data_len == 0)) {
                inst->sysexFrameScratch[0] = 0xF0;
                if (ev.data_len > 0) {
                    memcpy(inst->sysexFrameScratch + 1, ev.sysex, ev.data_len);
                }
                inst->sysexFrameScratch[ev.data_len + 1] = 0xF7;
                data = inst->sysexFrameScratch;
                len = ev.data_len + 2;
            } else {
                midiOutputStatus = kAudioUnitErr_InvalidParameter;
                break;
            }

            MIDIPacketList *pktList = (MIDIPacketList *)inst->sysexPacketBuf;
            MIDIPacket *pkt = MIDIPacketListInit(pktList);
            if (!MIDIPacketListAdd(pktList, inst->sysexPacketBufSize, pkt,
                                   ev.sample_offset, len, data)) {
                midiOutputStatus = kAudioUnitErr_MIDIOutputBufferFull;
                break;
            }
            OSStatus sent = inst->midiOutputCallback(inst->midiOutputUserData,
                                                     inTimeStamp, ev.port,
                                                     pktList);
            if (sent != noErr) {
                midiOutputStatus = sent;
                hostRefusedOutput = true;
                break;
            }
        }
    } else {
        /* Probe the Rust lane so a plugin that emitted events without a host
         * receiver observes queue unavailability on its next block. */
        g_callbacks->begin_output_events_v10(
            inst->rustCtx, 0, inFrameCount,
            (uint32_t)inst->hostMIDIProtocol, UINT32_MAX,
            umpTimeValid ? 1u : 0u,
            inst->paramFeedbackAvailable ? 1u : 0u);
        AuNativeEvent ev = {0};
        uint32_t status = g_callbacks->next_output_event(inst->rustCtx, &ev);
        if (status == AU_OUTPUT_INVALID)
            midiOutputStatus = kAudioUnitErr_InvalidParameter;
        else if (status != AU_OUTPUT_END)
            noReceiver = true;
    }

    if (midiOutputStatus == noErr) {
        uint32_t paramStatus = g_callbacks->commit_output_params(inst->rustCtx);
        if (paramStatus == AU_OUTPUT_QUEUE_FULL)
            midiOutputStatus = kAudioUnitErr_MIDIOutputBufferFull;
        else if (paramStatus == AU_OUTPUT_UNSUPPORTED)
            midiOutputStatus = kAudioUnitErr_FormatNotSupported;
        else if (paramStatus != AU_OUTPUT_END && paramStatus != AU_OUTPUT_EMITTED)
            midiOutputStatus = kAudioUnitErr_InvalidParameter;
    }

    if (g_callbacks->finish_output_events) {
        uint32_t status = AU_OUTPUT_EMITTED;
        if (hostRefusedOutput || midiOutputStatus == kAudioUnitErr_MIDIOutputBufferFull)
            status = AU_OUTPUT_QUEUE_FULL;
        else if (noReceiver || midiOutputStatus == kAudioUnitErr_FormatNotSupported)
            status = AU_OUTPUT_UNSUPPORTED;
        else if (midiOutputStatus != noErr)
            status = AU_OUTPUT_INVALID;
        g_callbacks->finish_output_events(inst->rustCtx, status);
    }

    // Copy our processed audio to the host's original buffers
    for (uint32_t c = 0; c < numOut && c < ioData->mNumberBuffers; c++) {
        if (hostBufs[c] && hostBufs[c] != inst->outputBuffers[c]) {
            memcpy(hostBufs[c], inst->outputBuffers[c], inFrameCount * sizeof(float));
            ioData->mBuffers[c].mData = hostBufs[c];
        } else {
            ioData->mBuffers[c].mData = inst->outputBuffers[c];
        }
        ioData->mBuffers[c].mDataByteSize = inFrameCount * sizeof(float);
    }

    // Post-render notify: `ioData` now holds the output the host will
    // consume, which is what a metering tap reads.
    au_v2_fire_render_notifies(inst, kAudioUnitRenderAction_PostRender,
                               ioFlags, inTimeStamp, inBusNumber, inFrameCount, ioData);

    return midiOutputStatus;
}

// ---------------------------------------------------------------------------
// MIDI (instruments only)
// ---------------------------------------------------------------------------

static OSStatus au_v2_midi_event(void *self_, UInt32 status,
                                  UInt32 data1, UInt32 data2,
                                  UInt32 sampleOffset) {
    MooseAUv2 *inst = (MooseAUv2 *)self_;

    if (status > UINT8_MAX || data1 > UINT8_MAX || data2 > UINT8_MAX) {
        return kAudioUnitErr_InvalidParameter;
    }
    if (inst->midiCount >= 256) {
        inst->midiOverflow = 1;
        return kAudioUnitErr_MIDIOutputBufferFull;
    }

    AuNativeEvent *ev = &inst->midiBuffer[inst->midiCount++];
    memset(ev, 0, sizeof(*ev));
    ev->sample_offset = sampleOffset;
    ev->kind = AU_NATIVE_EVENT_MIDI1;
    ev->data_len = 3;
    if ((status & 0xF0) == 0xC0 || (status & 0xF0) == 0xD0 ||
        status == 0xF1 || status == 0xF3) {
        ev->data_len = 2;
    } else if (status >= 0xF0 && status != 0xF2) {
        ev->data_len = 1;
    }
    ev->midi[0] = (uint8_t)status;
    ev->midi[1] = (uint8_t)data1;
    ev->midi[2] = (uint8_t)data2;
    ev->port = 0; /* AU v2 is single-stream MIDI input */
    return noErr;
}

static uint32_t au_ump_word_count(uint32_t word0) {
    switch ((word0 >> 28) & 0x0Fu) {
        case 0x0: case 0x1: case 0x2: case 0x6: case 0x7:
            return 1;
        case 0x3: case 0x4: case 0x8: case 0x9: case 0xA:
            return 2;
        case 0xB: case 0xC:
            return 3;
        default:
            return 4;
    }
}

static int au_ump_protocol_accepts(MIDIProtocolID protocol, uint32_t messageType) {
    if (messageType == 0x2) return protocol == kMIDIProtocol_1_0;
    if (messageType == 0x4)
        return protocol == kMIDIProtocol_2_0;
    return protocol == kMIDIProtocol_1_0 || protocol == kMIDIProtocol_2_0;
}

/* kMusicDeviceMIDIEventListSelect (263). Preflight the complete variable-size
 * list before appending anything so malformed widths or queue exhaustion
 * cannot leave a partial native block staged for the next render. */
static OSStatus au_v2_midi_event_list(void *self_, UInt32 inOffsetSampleFrame,
                                      const MIDIEventList *eventList) {
    MooseAUv2 *inst = (MooseAUv2 *)self_;
    if (!eventList) return kAudio_ParamError;

    MIDIProtocolID protocol = eventList->protocol;
    MIDIProtocolID expected = g_descriptor->midi2_input
        ? kMIDIProtocol_2_0 : kMIDIProtocol_1_0;
    if (protocol != expected) return kAudioUnitErr_FormatNotSupported;

    uint32_t eventCount = 0;
    const MIDIEventPacket *packet = &eventList->packet[0];
    for (uint32_t packetIndex = 0; packetIndex < eventList->numPackets; packetIndex++) {
        if (packet->wordCount == 0 ||
            packet->timeStamp > (MIDITimeStamp)(UINT32_MAX - inOffsetSampleFrame))
            return kAudioUnitErr_InvalidParameter;
        uint32_t wordIndex = 0;
        while (wordIndex < packet->wordCount) {
            uint32_t messageType = (packet->words[wordIndex] >> 28) & 0x0Fu;
            uint32_t width = au_ump_word_count(packet->words[wordIndex]);
            if (width > packet->wordCount - wordIndex ||
                !au_ump_protocol_accepts(protocol, messageType))
                return kAudioUnitErr_InvalidParameter;
            if (eventCount == UINT32_MAX) return kAudioUnitErr_InvalidParameter;
            eventCount++;
            wordIndex += width;
        }
        packet = MIDIEventPacketNext(packet);
    }

    if (eventCount > 256u - inst->midiCount) {
        inst->midiOverflow = 1;
        return kAudioUnitErr_MIDIOutputBufferFull;
    }

    packet = &eventList->packet[0];
    for (uint32_t packetIndex = 0; packetIndex < eventList->numPackets; packetIndex++) {
        uint32_t sampleOffset = inOffsetSampleFrame + (uint32_t)packet->timeStamp;
        uint32_t wordIndex = 0;
        while (wordIndex < packet->wordCount) {
            uint32_t width = au_ump_word_count(packet->words[wordIndex]);
            AuNativeEvent *ev = &inst->midiBuffer[inst->midiCount++];
            memset(ev, 0, sizeof(*ev));
            ev->sample_offset = sampleOffset;
            ev->port = 0;
            ev->kind = AU_NATIVE_EVENT_UMP;
            ev->protocol = (uint8_t)protocol;
            ev->data_len = width;
            for (uint32_t i = 0; i < width; i++)
                ev->words[i] = packet->words[wordIndex + i];
            wordIndex += width;
        }
        packet = MIDIEventPacketNext(packet);
    }
    return noErr;
}

/* kMusicDeviceSysExSelect handler. The host hands us one complete
 * SysEx message (0xF0-framed) per call, before the render pulls
 * audio. moose's EventBody::SysEx stores inner bytes only, so strip
 * the 0xF0 start / 0xF7 end framing. MusicDeviceSysEx carries no
 * timestamp, so the message lands at block start (sample_offset 0). */
static OSStatus au_v2_sysex(void *self_, const UInt8 *inData, UInt32 inLength) {
    MooseAUv2 *inst = (MooseAUv2 *)self_;
    if (!g_callbacks || !g_callbacks->push_sysex_input_native || !inst->rustCtx)
        return kAudioUnitErr_Uninitialized;
    if (!inData || inLength < 2 || inData[0] != 0xF0 ||
        inData[inLength - 1] != 0xF7)
        return kAudioUnitErr_InvalidParameter;
    const uint8_t *p = (const uint8_t *)inData;
    uint32_t n = inLength;
    p++;
    n -= 2;
    if (!g_callbacks->push_sysex_input_native(inst->rustCtx, 0, p, n)) {
        return kAudioUnitErr_TooManyFramesToProcess;
    }
    return noErr;
}

// ---------------------------------------------------------------------------
// Property listeners / render notify (stubs)
// ---------------------------------------------------------------------------

static OSStatus au_v2_add_property_listener(void *self_, AudioUnitPropertyID prop,
                                             AudioUnitPropertyListenerProc proc,
                                             void *userData) {
    MooseAUv2 *inst = (MooseAUv2 *)self_;
    if (inst->listenerCount >= 32) return noErr;
    inst->listeners[inst->listenerCount].prop = prop;
    inst->listeners[inst->listenerCount].proc = proc;
    inst->listeners[inst->listenerCount].userData = userData;
    inst->listenerCount++;
    return noErr;
}

static OSStatus au_v2_remove_property_listener_with_data(void *self_, AudioUnitPropertyID prop,
                                                          AudioUnitPropertyListenerProc proc,
                                                          void *userData) {
    MooseAUv2 *inst = (MooseAUv2 *)self_;
    for (uint32_t i = 0; i < inst->listenerCount; i++) {
        if (inst->listeners[i].prop == prop && inst->listeners[i].proc == proc &&
            inst->listeners[i].userData == userData) {
            inst->listeners[i] = inst->listeners[--inst->listenerCount];
            break;
        }
    }
    return noErr;
}

// Seqlock write-side bracket. Writers (host threads, assumed serialized by
// the AU property contract) bump the sequence odd before mutating and even
// after, with release fences so the render-thread reader observes either
// the pre- or post-mutation state, never a torn slot or count.
static inline void au_v2_rn_write_begin(MooseAUv2 *inst) {
    uint32_t s = __atomic_load_n(&inst->renderNotifySeq, __ATOMIC_RELAXED);
    __atomic_store_n(&inst->renderNotifySeq, s + 1, __ATOMIC_RELAXED);
    __atomic_thread_fence(__ATOMIC_RELEASE);
}
static inline void au_v2_rn_write_end(MooseAUv2 *inst) {
    uint32_t s = __atomic_load_n(&inst->renderNotifySeq, __ATOMIC_RELAXED);
    __atomic_thread_fence(__ATOMIC_RELEASE);
    __atomic_store_n(&inst->renderNotifySeq, s + 1, __ATOMIC_RELAXED);
}

static OSStatus au_v2_add_render_notify(void *self_, AURenderCallback proc, void *userData) {
    MooseAUv2 *inst = (MooseAUv2 *)self_;
    if (!proc) return kAudioUnitErr_InvalidParameter;
    // Ignore an exact duplicate (proc + refcon), matching the SDK.
    for (uint32_t i = 0; i < inst->renderNotifyCount; i++)
        if (inst->renderNotifies[i].proc == proc &&
            inst->renderNotifies[i].userData == userData)
            return noErr;
    if (inst->renderNotifyCount >= 32) return kAudio_MemFullError;
    au_v2_rn_write_begin(inst);
    inst->renderNotifies[inst->renderNotifyCount].proc = proc;
    inst->renderNotifies[inst->renderNotifyCount].userData = userData;
    inst->renderNotifyCount++;
    au_v2_rn_write_end(inst);
    return noErr;
}

static OSStatus au_v2_remove_render_notify(void *self_, AURenderCallback proc, void *userData) {
    MooseAUv2 *inst = (MooseAUv2 *)self_;
    for (uint32_t i = 0; i < inst->renderNotifyCount; i++) {
        if (inst->renderNotifies[i].proc == proc &&
            inst->renderNotifies[i].userData == userData) {
            au_v2_rn_write_begin(inst);
            // Compact the tail down over the removed slot.
            for (uint32_t j = i + 1; j < inst->renderNotifyCount; j++)
                inst->renderNotifies[j - 1] = inst->renderNotifies[j];
            inst->renderNotifyCount--;
            au_v2_rn_write_end(inst);
            return noErr;
        }
    }
    return noErr;
}

// Fire every registered render-notify with `action` (PreRender / PostRender)
// OR'd onto the current flags - the AUGraph metering / offline convention.
// Reads the list through the seqlock: copy a consistent snapshot, retrying
// if a host add/remove straddled the copy, then fire from the copy. Never
// blocks; if a writer keeps the list unstable it skips this block (a tap
// misses one block, which is recoverable), and it never calls through a
// torn/half-removed slot.
static void au_v2_fire_render_notifies(MooseAUv2 *inst,
                                       AudioUnitRenderActionFlags action,
                                       AudioUnitRenderActionFlags *ioFlags,
                                       const AudioTimeStamp *ts, UInt32 bus,
                                       UInt32 frames, AudioBufferList *ioData) {
    struct { AURenderCallback proc; void *userData; } local[32];
    uint32_t count = 0;
    bool stable = false;
    for (int attempt = 0; attempt < 4 && !stable; attempt++) {
        uint32_t s1 = __atomic_load_n(&inst->renderNotifySeq, __ATOMIC_RELAXED);
        if (s1 & 1u) continue; // a writer is mid-update
        __atomic_thread_fence(__ATOMIC_ACQUIRE);
        count = inst->renderNotifyCount;
        if (count > 32) count = 32;
        memcpy(local, inst->renderNotifies, count * sizeof(local[0]));
        __atomic_thread_fence(__ATOMIC_ACQUIRE);
        uint32_t s2 = __atomic_load_n(&inst->renderNotifySeq, __ATOMIC_RELAXED);
        stable = (s1 == s2);
    }
    if (!stable) return; // couldn't snapshot a stable list; skip this block
    for (uint32_t i = 0; i < count; i++) {
        AudioUnitRenderActionFlags f = (ioFlags ? *ioFlags : 0) | action;
        local[i].proc(local[i].userData, &f, ts, bus, frames, ioData);
    }
}

// ---------------------------------------------------------------------------
// Lookup - maps selectors to method function pointers
// ---------------------------------------------------------------------------

static AudioComponentMethod au_v2_lookup(SInt16 selector) {
    switch (selector) {
        case kAudioUnitInitializeSelect:
            return (AudioComponentMethod)au_v2_initialize;
        case kAudioUnitUninitializeSelect:
            return (AudioComponentMethod)au_v2_uninitialize;
        case kAudioUnitGetPropertyInfoSelect:
            return (AudioComponentMethod)au_v2_get_property_info;
        case kAudioUnitGetPropertySelect:
            return (AudioComponentMethod)au_v2_get_property;
        case kAudioUnitSetPropertySelect:
            return (AudioComponentMethod)au_v2_set_property;
        case kAudioUnitGetParameterSelect:
            return (AudioComponentMethod)au_v2_get_parameter;
        case kAudioUnitSetParameterSelect:
            return (AudioComponentMethod)au_v2_set_parameter;
        case kAudioUnitRenderSelect:
            return (AudioComponentMethod)au_v2_render;
        case kAudioUnitResetSelect:
            return (AudioComponentMethod)au_v2_reset;
        case kAudioUnitAddPropertyListenerSelect:
            return (AudioComponentMethod)au_v2_add_property_listener;
        case kAudioUnitRemovePropertyListenerWithUserDataSelect:
            return (AudioComponentMethod)au_v2_remove_property_listener_with_data;
        case kAudioUnitScheduleParametersSelect:
            return (AudioComponentMethod)au_v2_schedule_parameters;
        case kAudioUnitAddRenderNotifySelect:
            return (AudioComponentMethod)au_v2_add_render_notify;
        case kAudioUnitRemoveRenderNotifySelect:
            return (AudioComponentMethod)au_v2_remove_render_notify;
        case kMusicDeviceMIDIEventSelect:
            return accepts_midi_input() ? (AudioComponentMethod)au_v2_midi_event : NULL;
        case 258: // kMusicDeviceSysExSelect
            return accepts_midi_input() ? (AudioComponentMethod)au_v2_sysex : NULL;
        case kMusicDeviceMIDIEventListSelect:
            return accepts_midi_input() ? (AudioComponentMethod)au_v2_midi_event_list : NULL;
        // Return NULL for optional selectors (system probes for capabilities)
        case 11: // kAudioUnitRemovePropertyListenerSelect (legacy)
        case 19: case 20: case 21: // misc component selectors
        case 513: case 514: // kAudioOutputUnitStart/Stop
        case 259: // kMusicDevicePrepareInstrumentSelect
        case 260: // kMusicDeviceReleaseInstrumentSelect
        case 261: // kMusicDeviceStartNoteSelect
        case 262: // kMusicDeviceStopNoteSelect
            return NULL;
        default:
            return NULL;
    }
}

// ---------------------------------------------------------------------------
// Factory function - exported symbol, referenced by Info.plist factoryFunction.
// Returns an AudioComponentPlugInInterface* (AU v2 interface). The
// real definition is in the consumer cdylib via the Rust `export_au!`
// macro - it forwards to `moose_au_v2_factory_bridge` defined below.
// ---------------------------------------------------------------------------

static void *moose_au_v2_factory(const AudioComponentDescription *desc) {
    (void)desc;
    MooseAUv2 *inst = (MooseAUv2 *)calloc(1, sizeof(MooseAUv2));
    if (!inst) return NULL;

    inst->interface.Open = au_v2_open;
    inst->interface.Close = au_v2_close;
    inst->interface.Lookup = au_v2_lookup;
    inst->interface.reserved = NULL;

    /* 132 KiB: matches moose_core::SYSEX_POOL_PREALLOC (128 KiB)
     * plus headroom for per-packet headers (≈14 B × ≤256 events).
     * Heap-allocated to keep the MooseAUv2 struct itself small;
     * never reallocated after this point. */
    inst->sysexPacketBufSize = 132 * 1024;
    inst->sysexPacketBuf = (Byte *)malloc(inst->sysexPacketBufSize);
    inst->sysexFrameScratchSize = 128 * 1024 + 2; /* SYSEX_POOL_PREALLOC + framing */
    inst->sysexFrameScratch = (Byte *)malloc(inst->sysexFrameScratchSize);
    if (!inst->sysexPacketBuf || !inst->sysexFrameScratch) {
        free(inst->sysexPacketBuf);
        free(inst->sysexFrameScratch);
        free(inst);
        return NULL;
    }

    return &inst->interface;
}

// Called from the Rust-exported MooseAUFactory symbol.
void *moose_au_v2_factory_bridge(const void *desc) {
    return moose_au_v2_factory((const AudioComponentDescription *)desc);
}
