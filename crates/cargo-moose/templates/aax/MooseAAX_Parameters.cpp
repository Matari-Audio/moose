#include "MooseAAX_Parameters.h"
#include "MooseAAX_GUI.h"

#include "AAX_CBinaryDisplayDelegate.h"
#include "AAX_CBinaryTaperDelegate.h"
#include "AAX_CLinearTaperDelegate.h"
#include "AAX_CLogTaperDelegate.h"
#include "AAX_Enums.h"
#include "AAX_IController.h"
#include "AAX_IDisplayDelegate.h"
#include "AAX_IMIDINode.h"
#include "AAX_ITaperDelegate.h"
#include "AAX_ITransport.h"

#include <climits>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <sstream>

// ---------------------------------------------------------------------------
// Display & custom taper delegates
// ---------------------------------------------------------------------------

class MooseAAXDisplayDelegate : public AAX_IDisplayDelegate<float> {
public:
    MooseAAXDisplayDelegate(void* ctx, uint32_t paramID)
        : mCtx(ctx), mParamID(paramID) {}

    AAX_IDisplayDelegate<float>* Clone() const AAX_OVERRIDE {
        return new MooseAAXDisplayDelegate(*this);
    }

    bool ValueToString(float value, AAX_CString* valueString) const AAX_OVERRIDE {
        const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
        if (!cbs || !mCtx || !valueString) return false;
        char buf[128] = {};
        if (cbs->param_format(mCtx, mParamID, static_cast<double>(value), buf, sizeof(buf)) == 0) {
            return false;
        }
        *valueString = AAX_CString(buf);
        return true;
    }

    bool ValueToString(float value, int32_t /*maxNumChars*/,
                       AAX_CString* valueString) const AAX_OVERRIDE {
        return ValueToString(value, valueString);
    }

    bool StringToValue(const AAX_CString& valueString, float* value) const AAX_OVERRIDE {
        const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
        if (!cbs || !mCtx || !value) return false;
        double plain = 0.0;
        if (!cbs->param_parse(mCtx, mParamID, valueString.Get(), &plain)) {
            return false;
        }
        *value = static_cast<float>(plain);
        return true;
    }

private:
    void* mCtx;
    uint32_t mParamID;
};

class MooseAAXTaperDelegate : public AAX_ITaperDelegate<float> {
public:
    MooseAAXTaperDelegate(void* ctx, uint32_t paramID, float minV, float maxV)
        : mCtx(ctx), mParamID(paramID), mMin(minV), mMax(maxV) {}

    AAX_ITaperDelegate<float>* Clone() const AAX_OVERRIDE {
        return new MooseAAXTaperDelegate(*this);
    }
    float GetMinimumValue() const AAX_OVERRIDE { return mMin; }
    float GetMaximumValue() const AAX_OVERRIDE { return mMax; }
    float ConstrainRealValue(float value) const AAX_OVERRIDE {
        const float lo = mMin < mMax ? mMin : mMax;
        const float hi = mMin < mMax ? mMax : mMin;
        return value < lo ? lo : (value > hi ? hi : value);
    }
    float NormalizedToReal(double n) const AAX_OVERRIDE {
        const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
        if (!cbs || !mCtx) return static_cast<float>(mMin + n * (mMax - mMin));
        return static_cast<float>(cbs->param_denormalize(mCtx, mParamID, n));
    }
    double RealToNormalized(float real) const AAX_OVERRIDE {
        const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
        if (!cbs || !mCtx) {
            return mMax != mMin ? static_cast<double>((real - mMin) / (mMax - mMin)) : 0.0;
        }
        return cbs->param_normalize(mCtx, mParamID, static_cast<double>(real));
    }

private:
    void* mCtx;
    uint32_t mParamID;
    float mMin;
    float mMax;
};

// ---------------------------------------------------------------------------
// Factory / lifecycle
// ---------------------------------------------------------------------------

AAX_CEffectParameters* AAX_CALLBACK MooseAAX_Parameters::Create() {
    return new MooseAAX_Parameters();
}

MooseAAX_Parameters::MooseAAX_Parameters()
    : AAX_CMonolithicParameters()
    , mRustCtx(nullptr)
    , mActiveGUI(nullptr) {}

MooseAAX_Parameters::~MooseAAX_Parameters() {
    const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
    if (mRustCtx && cbs) {
        cbs->destroy(mRustCtx);
        mRustCtx = nullptr;
    }
}

std::string MooseAAX_Parameters::ParamIDString(uint32_t paramID) const {
    const MooseAaxDescriptor* d = MooseAax_Descriptor();
    if (d && paramID == d->bypass_param_id) {
        return std::string(cDefaultMasterBypassID);
    }
    std::ostringstream oss;
    oss << "moose_p" << paramID;
    return oss.str();
}

// ---------------------------------------------------------------------------
// MooseAaxHostSink callbacks (invoked on host UI thread during TimerWakeup)
// ---------------------------------------------------------------------------

void MooseAAX_Parameters::SinkTouchParam(void* user_data, uint32_t param_id) {
    auto* self = static_cast<MooseAAX_Parameters*>(user_data);
    if (!self) return;
    const std::string idStr = self->ParamIDString(param_id);
    self->TouchParameter(idStr.c_str());
}

void MooseAAX_Parameters::SinkSetParamNormalized(
    void* user_data, uint32_t param_id, double normalized)
{
    auto* self = static_cast<MooseAAX_Parameters*>(user_data);
    if (!self) return;
    const std::string idStr = self->ParamIDString(param_id);
    self->SetParameterNormalizedValue(idStr.c_str(), normalized);
}

void MooseAAX_Parameters::SinkReleaseParam(void* user_data, uint32_t param_id) {
    auto* self = static_cast<MooseAAX_Parameters*>(user_data);
    if (!self) return;
    const std::string idStr = self->ParamIDString(param_id);
    self->ReleaseParameter(idStr.c_str());
}

void MooseAAX_Parameters::SinkLatencyChanged(void* user_data, uint32_t samples) {
    auto* self = static_cast<MooseAAX_Parameters*>(user_data);
    if (!self) return;
    self->mLatency.Request(samples);
}

void MooseAAX_Parameters::SubmitLatency() {
    mLatency.Flush([this](uint32_t samples) {
        AAX_IController* controller = Controller();
        return controller && controller->SetSignalLatency(static_cast<int32_t>(samples)) == AAX_SUCCESS;
    });
}

void MooseAAX_Parameters::SinkStateDirty(void* user_data) {
    auto* self = static_cast<MooseAAX_Parameters*>(user_data);
    if (!self) return;
    self->mStateGeneration.fetch_add(1, std::memory_order_release);
}

void MooseAAX_Parameters::SinkParamValuesChanged(void* user_data) {
    auto* self = static_cast<MooseAAX_Parameters*>(user_data);
    const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
    if (!self || !cbs || !self->mRustCtx) return;
    for (uint32_t id : self->mParamIDs) {
        const double plain = cbs->param_get(self->mRustCtx, id);
        const double norm = cbs->param_normalize(self->mRustCtx, id, plain);
        const std::string idStr = self->ParamIDString(id);
        self->SetParameterNormalizedValue(idStr.c_str(), norm);
    }
}

void MooseAAX_Parameters::SinkParamPresentationChanged(void* /*user_data*/) {
    // Pro Tools queries display delegates live on paint.
}

int32_t MooseAAX_Parameters::SinkRequestResize(
    void* user_data, uint32_t width, uint32_t height)
{
    auto* self = static_cast<MooseAAX_Parameters*>(user_data);
    if (!self || !self->mActiveGUI) return 0;
    return self->mActiveGUI->RequestHostResize(width, height) ? 1 : 0;
}

// ---------------------------------------------------------------------------
// EffectInit - instantiate Rust runtime + populate live parameter catalog
// ---------------------------------------------------------------------------

AAX_Result MooseAAX_Parameters::EffectInit() {
    const MooseAaxDescriptor* d = MooseAax_Descriptor();
    const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
    if (!d || !cbs) return AAX_ERROR_NULL_OBJECT;

    MooseAaxHostSink sink = {};
    sink.size = static_cast<uint32_t>(sizeof(MooseAaxHostSink));
    sink.user_data = this;
    sink.touch_param = SinkTouchParam;
    sink.set_param_normalized = SinkSetParamNormalized;
    sink.release_param = SinkReleaseParam;
    sink.latency_changed = SinkLatencyChanged;
    sink.state_dirty = SinkStateDirty;
    sink.param_values_changed = SinkParamValuesChanged;
    sink.param_presentation_changed = SinkParamPresentationChanged;
    sink.request_resize = SinkRequestResize;

    mRustCtx = cbs->create_with_sink(&sink);
    if (!mRustCtx) return AAX_ERROR_NULL_OBJECT;

    AAX_CSampleRate sr = 44100.0;
    Controller()->GetSampleRate(&sr);
    mMaxBlockSize = 8192;
    cbs->reset(mRustCtx, static_cast<double>(sr), mMaxBlockSize);
    mSilence.assign(mMaxBlockSize, 0.0f);
    mNativeEvents.reserve(MOOSE_AAX_NATIVE_EVENT_CAP);
    mNativeSysex.reserve(MOOSE_AAX_SYSEX_POOL_CAP);

    AAX_EStemFormat inStem = AAX_eStemFormat_None;
    AAX_EStemFormat outStem = AAX_eStemFormat_None;
    Controller()->GetInputStemFormat(&inStem);
    Controller()->GetOutputStemFormat(&outStem);
    mNumInputChannels = inStem != AAX_eStemFormat_None
        ? static_cast<uint32_t>(AAX_STEM_FORMAT_CHANNEL_COUNT(inStem))
        : d->num_inputs;
    mNumOutputChannels = outStem != AAX_eStemFormat_None
        ? static_cast<uint32_t>(AAX_STEM_FORMAT_CHANNEL_COUNT(outStem))
        : d->num_outputs;

    // Query live parameter catalog (no static global param table).
    const uint32_t count = cbs->param_count(mRustCtx);
    mParamIDs.resize(count);
    if (count > 0) {
        cbs->param_ids(mRustCtx, mParamIDs.data(), count);
    }

    for (uint32_t id : mParamIDs) {
        MooseAaxParamInfo info = {};
        if (!cbs->param_info_by_id(mRustCtx, id, &info)) continue;

        const std::string idStr = ParamIDString(info.id);
        AAX_CString paramID(idStr.c_str());
        std::unique_ptr<AAX_IParameter> param;

        if (d->bypass_param_id != MOOSE_AAX_NO_BYPASS_PARAM &&
            info.id == d->bypass_param_id) {
            param.reset(new AAX_CParameter<bool>(
                paramID,
                AAX_CString(info.name),
                info.default_value >= 0.5,
                AAX_CBinaryTaperDelegate<bool>(),
                AAX_CBinaryDisplayDelegate<bool>("active", "bypass"),
                (info.flags & MOOSE_AAX_PARAM_FLAG_AUTOMATABLE) != 0));
            param->SetNumberOfSteps(2);
            param->SetType(AAX_eParameterType_Discrete);
        } else {
            MooseAAXDisplayDelegate display(mRustCtx, info.id);
            if (info.range_type == MOOSE_AAX_RANGE_CUSTOM) {
                param.reset(new AAX_CParameter<float>(
                    paramID,
                    AAX_CString(info.name),
                    static_cast<float>(info.default_value),
                    MooseAAXTaperDelegate(mRustCtx, info.id,
                                          static_cast<float>(info.min_value),
                                          static_cast<float>(info.max_value)),
                    display,
                    (info.flags & MOOSE_AAX_PARAM_FLAG_AUTOMATABLE) != 0));
            } else if (info.range_type == MOOSE_AAX_RANGE_LOG) {
                param.reset(new AAX_CParameter<float>(
                    paramID,
                    AAX_CString(info.name),
                    static_cast<float>(info.default_value),
                    AAX_CLogTaperDelegate<float>(
                        static_cast<float>(info.min_value),
                        static_cast<float>(info.max_value)),
                    display,
                    (info.flags & MOOSE_AAX_PARAM_FLAG_AUTOMATABLE) != 0));
            } else {
                param.reset(new AAX_CParameter<float>(
                    paramID,
                    AAX_CString(info.name),
                    static_cast<float>(info.default_value),
                    AAX_CLinearTaperDelegate<float>(
                        static_cast<float>(info.min_value),
                        static_cast<float>(info.max_value)),
                    display,
                    (info.flags & MOOSE_AAX_PARAM_FLAG_AUTOMATABLE) != 0));
            }
            param->SetNumberOfSteps(info.step_count > 0 ? info.step_count + 1 : 128);
            param->SetType(info.step_count > 0
                               ? AAX_eParameterType_Discrete
                               : AAX_eParameterType_Continuous);
        }

        AAX_IParameter* rawParam = param.release();
        mParameterManager.AddParameter(rawParam);
        AddSynchronizedParameter(*rawParam);
    }

    // The prepared value is available before the first render or timer turn.
    cbs->drain_host_queue(mRustCtx);
    mLatency.Request(cbs->latency_samples(mRustCtx));
    SubmitLatency();
    return AAX_SUCCESS;
}

AAX_Result MooseAAX_Parameters::NotificationReceived(
    AAX_CTypeID inNotificationType,
    const void* inNotificationData,
    uint32_t inNotificationDataSize)
{
    const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
    if (mRustCtx && cbs &&
        (inNotificationType == AAX_eNotificationEvent_EnteringOfflineMode ||
         inNotificationType == AAX_eNotificationEvent_ExitingOfflineMode)) {
        const uint32_t mode =
            (inNotificationType == AAX_eNotificationEvent_EnteringOfflineMode) ? 2u : 0u;
        cbs->set_render_mode(mRustCtx, mode);
    }
    return AAX_CMonolithicParameters::NotificationReceived(
        inNotificationType, inNotificationData, inNotificationDataSize);
}

AAX_Result MooseAAX_Parameters::TimerWakeup() {
    const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
    if (mRustCtx && cbs) {
        cbs->drain_host_queue(mRustCtx);
        SubmitLatency();
    }
    return AAX_CMonolithicParameters::TimerWakeup();
}

// ---------------------------------------------------------------------------
// RenderAudio - real-time audio & strict MIDI transaction
// ---------------------------------------------------------------------------

void MooseAAX_Parameters::RenderAudio(
    AAX_SInstrumentRenderInfo* ioRenderInfo,
    const TParamValPair* inSynchronizedParamValues[],
    int32_t inNumSynchronizedParamValues)
{
    const MooseAaxDescriptor* d = MooseAax_Descriptor();
    const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
    if (!mRustCtx || !d || !cbs) return;

    for (int32_t i = 0; i < inNumSynchronizedParamValues; i++) {
        const TParamValPair& pv = *inSynchronizedParamValues[i];
        const char* idStr = pv.first;
        uint32_t id = MOOSE_AAX_NO_BYPASS_PARAM;
        if (std::strcmp(idStr, cDefaultMasterBypassID) == 0 &&
            d->bypass_param_id != MOOSE_AAX_NO_BYPASS_PARAM) {
            id = d->bypass_param_id;
        } else if (std::strncmp(idStr, "moose_p", 7) == 0) {
            id = static_cast<uint32_t>(std::strtoul(idStr + 7, nullptr, 10));
        }
        if (id != MOOSE_AAX_NO_BYPASS_PARAM && pv.second) {
            float val = 0.0f;
            bool bval = false;
            if (pv.second->GetValueAsFloat(&val)) {
                cbs->param_set(mRustCtx, id, static_cast<double>(val));
            } else if (pv.second->GetValueAsBool(&bval)) {
                cbs->param_set(mRustCtx, id, bval ? 1.0 : 0.0);
            }
        }
    }

    const int32_t bufferSize = *ioRenderInfo->mNumSamples;
    if (bufferSize <= 0) return;

    if (static_cast<uint32_t>(bufferSize) > mMaxBlockSize) {
        if (ioRenderInfo->mAudioOutputs) {
            for (uint32_t ch = 0; ch < mNumOutputChannels; ch++) {
                float* output = ioRenderInfo->mAudioOutputs[ch];
                if (output) std::memset(output, 0, static_cast<size_t>(bufferSize) * sizeof(float));
            }
        }
        return;
    }

    constexpr uint32_t kMaxChannels = 8;
    constexpr uint32_t kMaxInputs = kMaxChannels + kMaxChannels;
    const float* inputs[kMaxInputs] = {};
    float* outputs[kMaxChannels] = {};

    uint32_t numIn = mNumInputChannels < kMaxChannels ? mNumInputChannels : kMaxChannels;
    uint32_t numOut = mNumOutputChannels < kMaxChannels ? mNumOutputChannels : kMaxChannels;

    if (ioRenderInfo->mAudioInputs) {
        for (uint32_t ch = 0; ch < numIn; ch++) {
            inputs[ch] = ioRenderInfo->mAudioInputs[ch];
        }
    }

    bool sidechainActive = false;
    if (d->sidechain_in_channels > 0 && !mSilence.empty()) {
        auto* extInfo = reinterpret_cast<MooseAaxExtendedRenderInfo*>(ioRenderInfo);
        const float* scBuf = nullptr;
        if (ioRenderInfo->mAudioInputs && extInfo->mSideChainP && *extInfo->mSideChainP != 0) {
            scBuf = ioRenderInfo->mAudioInputs[*extInfo->mSideChainP];
        }
        sidechainActive = (scBuf != nullptr);
        for (uint32_t c = 0; c < d->sidechain_in_channels && numIn < kMaxInputs; c++) {
            inputs[numIn++] = scBuf ? scBuf : mSilence.data();
        }
    }

    if (ioRenderInfo->mAudioOutputs) {
        for (uint32_t ch = 0; ch < numOut; ch++) {
            outputs[ch] = ioRenderInfo->mAudioOutputs[ch];
        }
    }

    mNativeEvents.clear();
    mNativeSysex.clear();
    uint32_t inputStatus = MOOSE_AAX_EVENT_END;
    bool sysexInProgress = false;
    uint32_t sysexStart = 0;
    uint32_t sysexDeltaFrames = 0;

    auto shortMessageLength = [](uint8_t status) -> uint32_t {
        if (status >= 0x80 && status <= 0xEF) {
            const uint8_t type = status & 0xF0;
            return (type == 0xC0 || type == 0xD0) ? 2u : 3u;
        }
        switch (status) {
            case 0xF1: case 0xF3: return 2;
            case 0xF2: return 3;
            case 0xF6: case 0xF8: case 0xFA: case 0xFB:
            case 0xFC: case 0xFE: case 0xFF: return 1;
            default: return 0;
        }
    };

    auto appendSysex = [&]() -> bool {
        if (mNativeEvents.size() >= MOOSE_AAX_NATIVE_EVENT_CAP) return false;
        MooseAaxNativeEvent ev = {};
        ev.sample_offset = sysexDeltaFrames;
        ev.kind = MOOSE_AAX_NATIVE_EVENT_SYSEX;
        ev.data_len = static_cast<uint32_t>(mNativeSysex.size()) - sysexStart;
        ev.sysex = ev.data_len == 0 ? nullptr : mNativeSysex.data() + sysexStart;
        mNativeEvents.push_back(ev);
        return true;
    };

    if (d->wants_input_midi && ioRenderInfo->mInputNode) {
        AAX_CMidiStream* stream = ioRenderInfo->mInputNode->GetNodeBuffer();
        if (stream && stream->mBufferSize > 0) {
            uint32_t previousTimestamp = 0;
            bool haveTimestamp = false;
            auto ingestSysexBytes = [&](const AAX_CMidiPacket& pkt, uint32_t start) {
                for (uint32_t j = start; j < pkt.mLength; j++) {
                    const uint8_t b = pkt.mData[j];
                    if (b == 0xF7) {
                        if (j + 1 != pkt.mLength || !appendSysex()) {
                            inputStatus = (j + 1 != pkt.mLength)
                                ? MOOSE_AAX_EVENT_INVALID
                                : MOOSE_AAX_EVENT_QUEUE_FULL;
                        }
                        sysexInProgress = false;
                        return inputStatus == MOOSE_AAX_EVENT_END;
                    }
                    if (b & 0x80) {
                        sysexInProgress = false;
                        inputStatus = MOOSE_AAX_EVENT_INVALID;
                        return false;
                    }
                    if (mNativeSysex.size() >= MOOSE_AAX_SYSEX_POOL_CAP) {
                        inputStatus = MOOSE_AAX_EVENT_QUEUE_FULL;
                        sysexInProgress = false;
                        return false;
                    }
                    mNativeSysex.push_back(b);
                }
                return true;
            };

            for (uint32_t i = 0; i < stream->mBufferSize; i++) {
                const AAX_CMidiPacket& pkt = stream->mBuffer[i];
                if (inputStatus != MOOSE_AAX_EVENT_END) break;
                if (pkt.mLength < 1 || pkt.mLength > 4 ||
                    pkt.mTimestamp >= static_cast<uint32_t>(bufferSize) ||
                    (haveTimestamp && pkt.mTimestamp < previousTimestamp)) {
                    inputStatus = MOOSE_AAX_EVENT_INVALID;
                    break;
                }
                previousTimestamp = pkt.mTimestamp;
                haveTimestamp = true;
                const uint8_t status = pkt.mData[0];
                if (sysexInProgress) {
                    ingestSysexBytes(pkt, 0);
                    continue;
                }
                if (status == 0xF0) {
                    sysexInProgress = true;
                    sysexStart = static_cast<uint32_t>(mNativeSysex.size());
                    sysexDeltaFrames = pkt.mTimestamp;
                    ingestSysexBytes(pkt, 1);
                    continue;
                }
                const uint32_t expected = shortMessageLength(status);
                bool valid = (expected == pkt.mLength);
                for (uint32_t j = 1; valid && j < pkt.mLength; j++) {
                    valid = (pkt.mData[j] & 0x80) == 0;
                }
                if (!valid) {
                    inputStatus = MOOSE_AAX_EVENT_INVALID;
                    break;
                }
                if (mNativeEvents.size() >= MOOSE_AAX_NATIVE_EVENT_CAP) {
                    inputStatus = MOOSE_AAX_EVENT_QUEUE_FULL;
                    break;
                }
                MooseAaxNativeEvent ev = {};
                ev.sample_offset = pkt.mTimestamp;
                ev.kind = MOOSE_AAX_NATIVE_EVENT_MIDI1;
                ev.data_len = pkt.mLength;
                for (uint32_t j = 0; j < pkt.mLength; j++) ev.midi[j] = pkt.mData[j];
                mNativeEvents.push_back(ev);
            }
            if (inputStatus == MOOSE_AAX_EVENT_END && sysexInProgress) {
                inputStatus = MOOSE_AAX_EVENT_INVALID;
            }
        }
    }

    if (inputStatus != MOOSE_AAX_EVENT_END) {
        mNativeEvents.clear();
        mNativeSysex.clear();
    }

    MooseAaxTransportSnapshot transport = {};
    AAX_ITransport* trans = Transport();
    if (trans) {
        bool playing = false;
        if (trans->IsTransportPlaying(&playing) == AAX_SUCCESS) {
            transport.playing = playing ? 1 : 0;
            transport.valid = 1;
        }
        double tempo = 0.0;
        if (trans->GetCurrentTempo(&tempo) == AAX_SUCCESS && tempo > 0.0) {
            transport.tempo = tempo;
            transport.valid = 1;
        }
        int32_t num = 0, den = 0;
        if (trans->GetCurrentMeter(&num, &den) == AAX_SUCCESS) {
            transport.time_sig_num = num;
            transport.time_sig_den = den;
            transport.valid = 1;
        }
        int64_t sampleLoc = 0;
        if (trans->GetCurrentNativeSampleLocation(&sampleLoc) == AAX_SUCCESS) {
            transport.position_samples = static_cast<double>(sampleLoc);
            transport.valid = 1;
        }
        int64_t tickPos = 0;
        if (trans->GetCurrentTickPosition(&tickPos) == AAX_SUCCESS) {
            transport.position_beats = static_cast<double>(tickPos) / 960000.0;
            transport.valid = 1;
        }
        int32_t bars = 0, beats = 0;
        int64_t barDisplayTicks = 0;
        if (trans->GetBarBeatPosition(&bars, &beats, &barDisplayTicks,
                                      static_cast<int64_t>(transport.position_samples)) == AAX_SUCCESS &&
            transport.time_sig_num > 0) {
            transport.bar_start_beats = static_cast<double>(bars) * static_cast<double>(transport.time_sig_num);
            transport.valid = 1;
        }
        bool loop = false;
        int64_t loopStart = 0, loopEnd = 0;
        if (trans->GetCurrentLoopPosition(&loop, &loopStart, &loopEnd) == AAX_SUCCESS) {
            transport.loop_active = loop ? 1 : 0;
            transport.loop_start_beats = static_cast<double>(loopStart) / 960000.0;
            transport.loop_end_beats = static_cast<double>(loopEnd) / 960000.0;
            transport.valid = 1;
        }
    }

    const uint32_t processStatus = cbs->process_native(
        mRustCtx,
        inputs,
        outputs,
        numIn,
        numOut,
        (ioRenderInfo->mAudioInputs ? 1u : 0u) | (sidechainActive ? 2u : 0u),
        ioRenderInfo->mAudioOutputs ? 1u : 0u,
        static_cast<uint32_t>(bufferSize),
        mNativeEvents.data(),
        static_cast<uint32_t>(mNativeEvents.size()),
        inputStatus,
        transport.valid ? &transport : nullptr);

    if (processStatus != MOOSE_AAX_EVENT_END) {
        cbs->finish_output_events(mRustCtx, MOOSE_AAX_EVENT_INVALID);
        return;
    }

    auto* extendedInfo = reinterpret_cast<MooseAaxExtendedRenderInfo*>(ioRenderInfo);
    AAX_IMIDINode* outputNode = extendedInfo->mOutputNode;
    cbs->begin_output_events(mRustCtx, static_cast<uint32_t>(bufferSize));
    if (outputNode) {
        for (;;) {
            MooseAaxNativeEvent ev = {};
            const uint32_t status = cbs->next_output_event(mRustCtx, &ev);
            if (status == MOOSE_AAX_EVENT_END) {
                cbs->finish_output_events(mRustCtx, MOOSE_AAX_EVENT_EMITTED);
                break;
            }
            if (status != MOOSE_AAX_EVENT_EMITTED) {
                cbs->finish_output_events(mRustCtx, status);
                return;
            }
            if (ev.kind == MOOSE_AAX_NATIVE_EVENT_MIDI1) {
                AAX_CMidiPacket pkt = {};
                pkt.mTimestamp = ev.sample_offset;
                pkt.mLength = ev.data_len;
                for (uint32_t j = 0; j < ev.data_len; j++) pkt.mData[j] = ev.midi[j];
                if (outputNode->PostMIDIPacket(&pkt) != AAX_SUCCESS) {
                    cbs->finish_output_events(mRustCtx, MOOSE_AAX_EVENT_QUEUE_FULL);
                    return;
                }
                continue;
            }
            const uint32_t totalLen = ev.data_len + 2;
            uint32_t pos = 0;
            while (pos < totalLen) {
                AAX_CMidiPacket pkt = {};
                pkt.mTimestamp = ev.sample_offset;
                pkt.mLength = (totalLen - pos < 4) ? (totalLen - pos) : 4;
                for (uint32_t j = 0; j < pkt.mLength; j++) {
                    const uint32_t p = pos + j;
                    pkt.mData[j] = (p == 0) ? 0xF0 : ((p + 1 == totalLen) ? 0xF7 : ev.sysex[p - 1]);
                }
                if (outputNode->PostMIDIPacket(&pkt) != AAX_SUCCESS) {
                    cbs->finish_output_events(mRustCtx, MOOSE_AAX_EVENT_QUEUE_FULL);
                    return;
                }
                pos += pkt.mLength;
            }
        }
    } else {
        MooseAaxNativeEvent ev = {};
        uint32_t status = cbs->next_output_event(mRustCtx, &ev);
        if (status == MOOSE_AAX_EVENT_EMITTED) status = MOOSE_AAX_EVENT_UNSUPPORTED;
        cbs->finish_output_events(mRustCtx, status);
    }
}

// ---------------------------------------------------------------------------
// State chunks & CompareActiveChunk dirty tracking
// ---------------------------------------------------------------------------

constexpr AAX_CTypeID kMooseControlsChunkID = 'elck';

static bool is_legacy_chunk(AAX_CTypeID chunkID) {
    const MooseAaxDescriptor* d = MooseAax_Descriptor();
    if (!d || !d->legacy_chunk_ids) return false;
    for (uint32_t i = 0; i < d->num_legacy_chunk_ids; i++) {
        if (static_cast<AAX_CTypeID>(d->legacy_chunk_ids[i]) == chunkID) return true;
    }
    return false;
}

AAX_Result MooseAAX_Parameters::GetNumberOfChunks(int32_t* oNumChunks) const {
    if (!oNumChunks) return AAX_ERROR_NULL_ARGUMENT;
    // Advertise only moose's own state chunk for saving. Legacy chunk IDs
    // declared in `legacy_chunk_ids` are accepted on load in `SetChunk`
    // without writing empty legacy chunks into every saved Pro Tools session.
    *oNumChunks = 1;
    return AAX_SUCCESS;
}

AAX_Result MooseAAX_Parameters::GetChunkIDFromIndex(int32_t index, AAX_CTypeID* oChunkID) const {
    if (!oChunkID) return AAX_ERROR_NULL_ARGUMENT;
    if (index == 0) {
        *oChunkID = kMooseControlsChunkID;
        return AAX_SUCCESS;
    }
    *oChunkID = 0;
    return AAX_ERROR_INVALID_CHUNK_INDEX;
}

AAX_Result MooseAAX_Parameters::GetChunkSize(AAX_CTypeID chunkID, uint32_t* oSize) const {
    if (!oSize) return AAX_ERROR_NULL_ARGUMENT;
    const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
    if (!mRustCtx || !cbs) return AAX_ERROR_NULL_OBJECT;
    if (chunkID != kMooseControlsChunkID) {
        *oSize = 0;
        return AAX_ERROR_INVALID_CHUNK_ID;
    }
    uint8_t* data = nullptr;
    const uint32_t len = cbs->state_save(mRustCtx, &data);
    if (data && len > 0) {
        mPendingChunk.assign(data, data + len);
        cbs->state_free(data, len);
    } else {
        mPendingChunk.clear();
    }
    *oSize = len;
    return AAX_SUCCESS;
}

AAX_Result MooseAAX_Parameters::GetChunk(AAX_CTypeID chunkID, AAX_SPlugInChunk* oChunk) const {
    if (!oChunk) return AAX_ERROR_NULL_ARGUMENT;
    const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
    if (!mRustCtx || !cbs) return AAX_ERROR_NULL_OBJECT;
    if (chunkID != kMooseControlsChunkID) {
        return AAX_ERROR_INVALID_CHUNK_ID;
    }
    if (mPendingChunk.empty()) {
        uint8_t* data = nullptr;
        const uint32_t len = cbs->state_save(mRustCtx, &data);
        if (data && len > 0) {
            mPendingChunk.assign(data, data + len);
            cbs->state_free(data, len);
        }
    }
    const size_t size = mPendingChunk.size();
    if (size > static_cast<size_t>(INT32_MAX)) {
        mPendingChunk.clear();
        mPendingChunk.shrink_to_fit();
        return AAX_ERROR_INVALID_CHUNK_SIZE;
    }
    if (size > 0) {
        // Pro Tools allocates `oChunk` using the byte size returned by
        // `GetChunkSize`; it does not guarantee `oChunk->fSize` is pre-filled.
        std::memcpy(oChunk->fData, mPendingChunk.data(), size);
    }
    oChunk->fSize = static_cast<int32_t>(size);
    mPendingChunk.clear();
    mPendingChunk.shrink_to_fit();
    mSavedGeneration.store(
        mStateGeneration.load(std::memory_order_acquire),
        std::memory_order_release);
    return AAX_SUCCESS;
}

AAX_Result MooseAAX_Parameters::SetChunk(AAX_CTypeID chunkID, const AAX_SPlugInChunk* iChunk) {
    const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
    if (!mRustCtx || !cbs) return AAX_ERROR_NULL_OBJECT;
    if (is_legacy_chunk(chunkID)) {
        if (!iChunk || iChunk->fSize == 0) return AAX_SUCCESS;
        if (cbs->state_load_foreign(mRustCtx,
                                    static_cast<uint32_t>(chunkID),
                                    reinterpret_cast<const uint8_t*>(iChunk->fData),
                                    static_cast<uint32_t>(iChunk->fSize))) {
            mSavedGeneration.store(
                mStateGeneration.load(std::memory_order_acquire),
                std::memory_order_release);
            return AAX_SUCCESS;
        }
        return AAX_ERROR_INVALID_CHUNK_ID;
    }
    if (chunkID != kMooseControlsChunkID) {
        return AAX_ERROR_INVALID_CHUNK_ID;
    }
    if (!iChunk || iChunk->fSize <= 0) return AAX_ERROR_INVALID_CHUNK_SIZE;
    if (!cbs->state_load(mRustCtx,
                         reinterpret_cast<const uint8_t*>(iChunk->fData),
                         static_cast<uint32_t>(iChunk->fSize))) {
        return AAX_ERROR_INVALID_CHUNK_SIZE;
    }
    mSavedGeneration.store(
        mStateGeneration.load(std::memory_order_acquire),
        std::memory_order_release);
    return AAX_SUCCESS;
}

AAX_Result MooseAAX_Parameters::CompareActiveChunk(
    const AAX_SPlugInChunk* iChunkP, AAX_CBoolean* oIsEqual) const
{
    if (!oIsEqual) return AAX_ERROR_NULL_ARGUMENT;
    if (!iChunkP || !mRustCtx || iChunkP->fSize < 0) {
        *oIsEqual = false;
        return AAX_SUCCESS;
    }
    if (mStateGeneration.load(std::memory_order_acquire) !=
        mSavedGeneration.load(std::memory_order_acquire)) {
        *oIsEqual = false;
        return AAX_SUCCESS;
    }
    const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
    if (!cbs) {
        *oIsEqual = false;
        return AAX_SUCCESS;
    }
    // `kMooseControlsChunkID` holds moose's opaque OAST state envelope rather
    // than AAX's SDK chunk-parser format, so compare directly against live state.
    uint8_t* data = nullptr;
    const uint32_t len = cbs->state_save(mRustCtx, &data);
    bool equal = false;
    if (len == static_cast<uint32_t>(iChunkP->fSize)) {
        equal = (len == 0) || (data && std::memcmp(data, iChunkP->fData, len) == 0);
    }
    if (data) {
        cbs->state_free(data, len);
    }
    *oIsEqual = equal;
    return AAX_SUCCESS;
}
