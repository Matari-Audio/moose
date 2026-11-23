#pragma once

#include "AAX_CMonolithicParameters.h"
#include "MooseAAX_Bridge.h"

#include <atomic>
#include <cstdint>
#include <string>
#include <vector>

class MooseAAX_GUI;

struct MooseAaxExtendedRenderInfo {
    AAX_SInstrumentRenderInfo base;
    AAX_IMIDINode* mOutputNode;
    int32_t* mSideChainP;
};

class MooseAAX_Parameters : public AAX_CMonolithicParameters {
public:
    MooseAAX_Parameters();
    ~MooseAAX_Parameters() override;

    static AAX_CEffectParameters* AAX_CALLBACK Create();

    AAX_Result EffectInit() override;

    void RenderAudio(
        AAX_SInstrumentRenderInfo* ioRenderInfo,
        const TParamValPair* inSynchronizedParamValues[],
        int32_t inNumSynchronizedParamValues) override;

    AAX_Result NotificationReceived(
        AAX_CTypeID inNotificationType,
        const void* inNotificationData,
        uint32_t inNotificationDataSize) override;

    // Drains the Rust HostQueue + GestureQueue via MooseAaxHostSink.
    AAX_Result TimerWakeup() override;

    // State chunks + dirty comparison.
    AAX_Result GetChunkSize(AAX_CTypeID chunkID, uint32_t* oSize) const override;
    AAX_Result GetChunk(AAX_CTypeID chunkID, AAX_SPlugInChunk* oChunk) const override;
    AAX_Result SetChunk(AAX_CTypeID chunkID, const AAX_SPlugInChunk* iChunk) override;
    AAX_Result CompareActiveChunk(
        const AAX_SPlugInChunk* iChunkP, AAX_CBoolean* oIsEqual) const override;
    AAX_Result GetNumberOfChunks(int32_t* oNumChunks) const override;
    AAX_Result GetChunkIDFromIndex(int32_t index, AAX_CTypeID* oChunkID) const override;

    void* GetRustCtx() const { return mRustCtx; }
    void SetActiveGUI(MooseAAX_GUI* gui) { mActiveGUI = gui; }
    std::string ParamIDString(uint32_t paramID) const;

private:
    static void SinkTouchParam(void* user_data, uint32_t param_id);
    static void SinkSetParamNormalized(void* user_data, uint32_t param_id, double normalized);
    static void SinkReleaseParam(void* user_data, uint32_t param_id);
    static void SinkLatencyChanged(void* user_data, uint32_t samples);
    void SubmitLatency();
    static void SinkStateDirty(void* user_data);
    static void SinkParamValuesChanged(void* user_data);
    static void SinkParamPresentationChanged(void* user_data);
    static int32_t SinkRequestResize(void* user_data, uint32_t width, uint32_t height);

    void* mRustCtx;
    MooseAAX_GUI* mActiveGUI;
    std::vector<uint32_t> mParamIDs;
    mutable std::vector<uint8_t> mPendingChunk;
    uint32_t mMaxBlockSize = 0;
    uint32_t mNumInputChannels = 0;
    uint32_t mNumOutputChannels = 0;
    std::vector<float> mSilence;
    std::vector<MooseAaxNativeEvent> mNativeEvents;
    std::vector<uint8_t> mNativeSysex;
    MooseAaxLatency mLatency;
    std::atomic<uint64_t> mStateGeneration{0};
    mutable std::atomic<uint64_t> mSavedGeneration{0};
};
