#ifndef MOOSE_AAX_SHIM_TYPES_H
#define MOOSE_AAX_SHIM_TYPES_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* =========================================================================
 * Moose AAX C ABI (static-linked single-vtable bridge)
 *
 * Shared between `crates/moose-aax` (Rust runtime) and the C++
 * `AAX_CMonolithicParameters` template in `crates/cargo-moose/templates/aax`.
 *
 * Magic: 'MAx\0' (0x4D417800). Low byte is the monotonically increasing
 * revision counter. Any incompatible layout change increments it.
 * ========================================================================= */

#define MOOSE_AAX_ABI_MAGIC   0x4D417800u
#define MOOSE_AAX_ABI_VERSION 0x4D417801u

/* Parameter taper hints matching moose_params::ParamRange. */
#define MOOSE_AAX_RANGE_LINEAR   0u
#define MOOSE_AAX_RANGE_LOG      1u
#define MOOSE_AAX_RANGE_DISCRETE 2u
#define MOOSE_AAX_RANGE_CUSTOM   3u

/* Sentinel when the plugin declares no IS_BYPASS parameter. */
#define MOOSE_AAX_NO_BYPASS_PARAM 0xFFFFFFFFu

/* Parameter flag bits in MooseAaxParamInfo::flags. */
#define MOOSE_AAX_PARAM_FLAG_AUTOMATABLE (1u << 0)
#define MOOSE_AAX_PARAM_FLAG_READ_ONLY   (1u << 1)
#define MOOSE_AAX_PARAM_FLAG_HIDDEN      (1u << 2)
#define MOOSE_AAX_PARAM_FLAG_BYPASS      (1u << 3)

/* Native MIDI event kinds & transactional status codes. */
#define MOOSE_AAX_NATIVE_EVENT_MIDI1 0u
#define MOOSE_AAX_NATIVE_EVENT_SYSEX 1u

#define MOOSE_AAX_EVENT_END         0u
#define MOOSE_AAX_EVENT_EMITTED     1u
#define MOOSE_AAX_EVENT_QUEUE_FULL  2u
#define MOOSE_AAX_EVENT_INVALID     3u
#define MOOSE_AAX_EVENT_UNSUPPORTED 4u

#define MOOSE_AAX_NATIVE_EVENT_CAP 256u
#define MOOSE_AAX_SYSEX_POOL_CAP   4096u

/* Editor parent view platform discriminants matching AAX_EViewContainer_Type. */
#define MOOSE_AAX_VIEW_UNKNOWN 0
#define MOOSE_AAX_VIEW_NSVIEW  1
#define MOOSE_AAX_VIEW_HWND    2

/* One declared main-bus channel width pair (in_channels, out_channels). */
typedef struct MooseAaxBusLayout {
    int16_t in_channels;
    int16_t out_channels;
} MooseAaxBusLayout;

/* Immutable plugin descriptor returned by moose_aax_descriptor(). */
typedef struct MooseAaxDescriptor {
    uint32_t abi_version;
    uint32_t _reserved;
    const char* name;
    const char* vendor;
    const char* plugin_id;
    uint32_t version_packed;
    uint32_t manufacturer_id;
    uint32_t product_id;
    uint32_t plugin_type_id;
    uint32_t category;
    uint32_t num_inputs;
    uint32_t num_outputs;
    uint32_t sidechain_in_channels;
    uint32_t bypass_param_id;
    uint32_t has_editor;
    uint32_t wants_input_midi;
    uint32_t emits_midi;
    uint32_t num_layouts;
    const MooseAaxBusLayout* layouts;
    uint32_t num_legacy_chunk_ids;
    const uint32_t* legacy_chunk_ids;
} MooseAaxDescriptor;

/* Per-parameter metadata queried dynamically from the live catalog. */
typedef struct MooseAaxParamInfo {
    uint32_t id;
    char name[128];
    char unit[32];
    double min_value;
    double max_value;
    double default_value;
    uint32_t step_count;
    uint32_t flags;
    uint8_t range_type;
    uint8_t _pad[3];
} MooseAaxParamInfo;

/* One ordered MIDI 1.0 or SysEx event in a process block. */
typedef struct MooseAaxNativeEvent {
    uint32_t sample_offset;
    uint32_t kind;
    uint32_t data_len;
    uint8_t midi[4];
    const uint8_t* sysex;
} MooseAaxNativeEvent;

/* Transport snapshot read from AAX_ITransport once per RenderAudio block. */
typedef struct MooseAaxTransportSnapshot {
    int32_t valid;
    int32_t playing;
    double tempo;
    int32_t time_sig_num;
    int32_t time_sig_den;
    double position_samples;
    double position_beats;
    double bar_start_beats;
    int32_t loop_active;
    int32_t _pad;
    double loop_start_beats;
    double loop_end_beats;
} MooseAaxTransportSnapshot;

/* Host callback sink passed to create_with_sink().
 *
 * Every function pointer is invoked strictly on the host UI/main thread
 * during `drain_host_queue` (driven by AAX `TimerWakeup`) or editor hooks.
 * `size` must be set to `sizeof(MooseAaxHostSink)` for prefix-safe copies.
 */
typedef struct MooseAaxHostSink {
    uint32_t size;
    uint32_t _reserved;
    void* user_data;
    void (*release_user_data)(void* user_data);
    void (*touch_param)(void* user_data, uint32_t param_id);
    void (*set_param_normalized)(void* user_data, uint32_t param_id, double normalized);
    void (*release_param)(void* user_data, uint32_t param_id);
    void (*latency_changed)(void* user_data, uint32_t samples);
    void (*state_dirty)(void* user_data);
    void (*param_values_changed)(void* user_data);
    void (*param_presentation_changed)(void* user_data);
    int32_t (*request_resize)(void* user_data, uint32_t width, uint32_t height);
} MooseAaxHostSink;

/* Single callback vtable returned by moose_aax_callbacks(). */
typedef struct MooseAaxCallbacks {
    uint32_t abi_version;
    uint32_t _reserved;

    /* Lifecycle */
    void* (*create_with_sink)(const MooseAaxHostSink* sink);
    void (*destroy)(void* ctx);
    void (*reset)(void* ctx, double sample_rate, uint32_t max_frames);
    void (*set_render_mode)(void* ctx, uint32_t mode);

    /* Audio & strict MIDI event processing */
    uint32_t (*process_native)(
        void* ctx,
        const float* const* inputs,
        float* const* outputs,
        uint32_t num_input_channels,
        uint32_t num_output_channels,
        uint32_t input_bus_active,
        uint32_t output_bus_active,
        uint32_t num_frames,
        const MooseAaxNativeEvent* events,
        uint32_t num_events,
        uint32_t input_status,
        const MooseAaxTransportSnapshot* transport);
    void (*begin_output_events)(void* ctx, uint32_t num_frames);
    uint32_t (*next_output_event)(void* ctx, MooseAaxNativeEvent* out_event);
    void (*finish_output_events)(void* ctx, uint32_t status);

    /* Live parameter catalog & value/text/taper operations */
    uint32_t (*param_count)(void* ctx);
    uint32_t (*param_ids)(void* ctx, uint32_t* out_ids, uint32_t cap);
    int32_t (*param_info_by_id)(void* ctx, uint32_t param_id, MooseAaxParamInfo* out_info);
    double (*param_get)(void* ctx, uint32_t param_id);
    void (*param_set)(void* ctx, uint32_t param_id, double value);
    uint32_t (*param_format)(
        void* ctx, uint32_t param_id, double value, char* out, uint32_t out_len);
    int32_t (*param_parse)(void* ctx, uint32_t param_id, const char* text, double* out_plain);
    double (*param_normalize)(void* ctx, uint32_t param_id, double plain);
    double (*param_denormalize)(void* ctx, uint32_t param_id, double normalized);

    /* State chunks & foreign migration */
    uint32_t (*state_save)(void* ctx, uint8_t** out_data);
    int32_t (*state_load)(void* ctx, const uint8_t* data, uint32_t len);
    int32_t (*state_load_foreign)(
        void* ctx, uint32_t chunk_id, const uint8_t* data, uint32_t len);
    void (*state_free)(uint8_t* data, uint32_t len);

    /* Delay & main-thread HostQueue drain */
    uint32_t (*latency_samples)(void* ctx);
    uint32_t (*tail_samples)(void* ctx);
    void (*drain_host_queue)(void* ctx);

    /* Editor */
    int32_t (*editor_has)(void* ctx);
    int32_t (*editor_get_size)(void* ctx, uint32_t* width, uint32_t* height);
    int32_t (*editor_open)(void* ctx, void* parent_view, int32_t platform);
    void (*editor_close)(void* ctx);
    void (*editor_idle)(void* ctx);
} MooseAaxCallbacks;

/* Static-linked entry points exported by moose_aax::export_aax!(). */
extern uint32_t moose_aax_abi_version(void);
extern const MooseAaxDescriptor* moose_aax_descriptor(void);
extern const MooseAaxCallbacks* moose_aax_callbacks(void);

#ifdef __cplusplus
}
#endif

#endif /* MOOSE_AAX_SHIM_TYPES_H */
