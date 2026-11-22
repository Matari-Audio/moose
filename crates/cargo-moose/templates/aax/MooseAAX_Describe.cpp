/**
 * AAX plugin entry point - GetEffectDescriptions.
 *
 * Statically linked against the Rust plugin's `moose_aax` exports
 * (`moose_aax_abi_version`, `moose_aax_descriptor`, `moose_aax_callbacks`).
 * Zero dlopen indirection.
 */

#include "MooseAAX_Bridge.h"
#include "MooseAAX_GUI.h"
#include "MooseAAX_Parameters.h"

#include "AAX_CMonolithicParameters.h"
#include "AAX_Enums.h"
#include "AAX_ICollection.h"
#include "AAX_IComponentDescriptor.h"
#include "AAX_IEffectDescriptor.h"
#include "AAX_IPropertyMap.h"

#include <cstddef>
#include <cstdio>

static AAX_EStemFormat MooseStemFormat(int16_t channels) {
    switch (channels) {
        case 1:  return AAX_eStemFormat_Mono;
        case 2:  return AAX_eStemFormat_Stereo;
        case 3:  return AAX_eStemFormat_LCR;
        case 4:  return AAX_eStemFormat_Quad;
        case 5:  return AAX_eStemFormat_5_0;
        case 6:  return AAX_eStemFormat_5_1;
        case 7:  return AAX_eStemFormat_7_0_DTS;
        case 8:  return AAX_eStemFormat_7_1_DTS;
        default: return AAX_eStemFormat_None;
    }
}

static AAX_Result MooseDescribeOneConfig(
    AAX_IEffectDescriptor* ioDescriptor,
    const AAX_SInstrumentSetupInfo& setupInfo,
    bool needsOutputMIDI,
    const char* outputMIDIName,
    bool needsSidechain)
{
    AAX_IComponentDescriptor* const compDesc = ioDescriptor->NewComponentDescriptor();
    if (!compDesc) return AAX_ERROR_NULL_OBJECT;

    AAX_Result err = AAX_SUCCESS;

    const AAX_CFieldIndex globalNodeID =
        AAX_FIELD_INDEX(MooseAaxExtendedRenderInfo, base.mGlobalNode);
    const AAX_CFieldIndex localInputNodeID =
        AAX_FIELD_INDEX(MooseAaxExtendedRenderInfo, base.mInputNode);
    const AAX_CFieldIndex transportNodeID =
        AAX_FIELD_INDEX(MooseAaxExtendedRenderInfo, base.mTransportNode);
    const AAX_CFieldIndex outputNodeID =
        AAX_FIELD_INDEX(MooseAaxExtendedRenderInfo, mOutputNode);

    if (setupInfo.mNeedsGlobalMIDI) {
        err = compDesc->AddMIDINode(globalNodeID, AAX_eMIDINodeType_Global,
                                    setupInfo.mGlobalMIDINodeName, setupInfo.mGlobalMIDIEventMask);
    } else {
        err = compDesc->AddPrivateData(globalNodeID, sizeof(float),
                                       AAX_ePrivateDataOptions_DefaultOptions);
    }
    if (err != AAX_SUCCESS) return err;

    if (setupInfo.mNeedsInputMIDI) {
        err = compDesc->AddMIDINode(localInputNodeID, AAX_eMIDINodeType_LocalInput,
                                    setupInfo.mInputMIDINodeName, setupInfo.mInputMIDIChannelMask);
    } else {
        err = compDesc->AddPrivateData(localInputNodeID, sizeof(float),
                                       AAX_ePrivateDataOptions_DefaultOptions);
    }
    if (err != AAX_SUCCESS) return err;

    if (setupInfo.mNeedsTransport) {
        err = compDesc->AddMIDINode(transportNodeID, AAX_eMIDINodeType_Transport,
                                    "Transport", 0xffff);
    } else {
        err = compDesc->AddPrivateData(transportNodeID, sizeof(float),
                                       AAX_ePrivateDataOptions_DefaultOptions);
    }
    if (err != AAX_SUCCESS) return err;

    if (needsOutputMIDI) {
        err = compDesc->AddMIDINode(outputNodeID, AAX_eMIDINodeType_LocalOutput,
                                    outputMIDIName, 0xFFFF);
    } else {
        err = compDesc->AddPrivateData(outputNodeID, sizeof(float),
                                       AAX_ePrivateDataOptions_DefaultOptions);
    }
    if (err != AAX_SUCCESS) return err;

    err = compDesc->AddAudioIn(AAX_FIELD_INDEX(MooseAaxExtendedRenderInfo, base.mAudioInputs));
    if (err != AAX_SUCCESS) return err;
    err = compDesc->AddAudioOut(AAX_FIELD_INDEX(MooseAaxExtendedRenderInfo, base.mAudioOutputs));
    if (err != AAX_SUCCESS) return err;
    err = compDesc->AddAudioBufferLength(AAX_FIELD_INDEX(MooseAaxExtendedRenderInfo, base.mNumSamples));
    if (err != AAX_SUCCESS) return err;
    err = compDesc->AddClock(AAX_FIELD_INDEX(MooseAaxExtendedRenderInfo, base.mClock));
    if (err != AAX_SUCCESS) return err;

    if (needsSidechain) {
        err = compDesc->AddSideChainIn(AAX_FIELD_INDEX(MooseAaxExtendedRenderInfo, mSideChainP));
        if (err != AAX_SUCCESS) return err;
    }

    err = compDesc->AddPrivateData(AAX_FIELD_INDEX(MooseAaxExtendedRenderInfo, base.mMeters),
                                   sizeof(float), AAX_ePrivateDataOptions_DefaultOptions);
    if (err != AAX_SUCCESS) return err;

    err = compDesc->AddPrivateData(AAX_FIELD_INDEX(MooseAaxExtendedRenderInfo, base.mPrivateData),
                                   sizeof(AAX_SInstrumentPrivateData),
                                   AAX_ePrivateDataOptions_DefaultOptions);
    if (err != AAX_SUCCESS) return err;

    err = compDesc->AddDataInPort(AAX_FIELD_INDEX(MooseAaxExtendedRenderInfo, base.mCurrentStateNum),
                                  sizeof(uint64_t));
    if (err != AAX_SUCCESS) return err;

    AAX_IPropertyMap* const properties = compDesc->NewPropertyMap();
    if (!properties) return AAX_ERROR_NULL_OBJECT;

    if (setupInfo.mUseHostGeneratedGUI) {
        err = properties->AddProperty(AAX_eProperty_UsesClientGUI, true);
    }
    err = properties->AddProperty(AAX_eProperty_InputStemFormat,
                                  static_cast<int32_t>(setupInfo.mInputStemFormat));
    err = properties->AddProperty(AAX_eProperty_OutputStemFormat,
                                  static_cast<int32_t>(setupInfo.mOutputStemFormat));
    err = properties->AddProperty(AAX_eProperty_CanBypass, setupInfo.mCanBypass);
    err = properties->AddProperty(AAX_eProperty_Constraint_Location,
                                  0x0 | AAX_eConstraintLocationMask_DataModel);
    if (setupInfo.mNeedsTransport) {
        err = properties->AddProperty(AAX_eProperty_UsesTransport, true);
    }
    err = properties->AddProperty(AAX_eProperty_ManufacturerID,
                                  static_cast<int32_t>(setupInfo.mManufacturerID));
    err = properties->AddProperty(AAX_eProperty_ProductID,
                                  static_cast<int32_t>(setupInfo.mProductID));
    err = properties->AddProperty(AAX_eProperty_PlugInID_Native,
                                  static_cast<int32_t>(setupInfo.mPluginID));
    if (!setupInfo.mMultiMonoSupport) {
        err = properties->AddProperty(AAX_eProperty_Constraint_MultiMonoSupport, 0);
    }

    err = compDesc->AddProcessProc_Native(
        AAX_CMonolithicParameters::StaticRenderAudio, properties);
    if (err != AAX_SUCCESS) return err;

    return ioDescriptor->AddComponent(compDesc);
}

AAX_Result GetEffectDescriptions(AAX_ICollection* outCollection) {
    const MooseAaxDescriptor* d = MooseAax_Descriptor();
    const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
    if (!d || !cbs) {
        return AAX_ERROR_NULL_OBJECT;
    }

    AAX_IEffectDescriptor* desc = outCollection->NewDescriptor();
    if (!desc) return AAX_ERROR_NULL_OBJECT;

    desc->AddName(d->name);
    desc->AddCategory(d->category);

    AAX_SInstrumentSetupInfo setupInfo = {};
    setupInfo.mManufacturerID = d->manufacturer_id;
    setupInfo.mProductID = d->product_id;
    setupInfo.mCanBypass = (d->bypass_param_id != MOOSE_AAX_NO_BYPASS_PARAM);
    setupInfo.mNeedsTransport = true;
    setupInfo.mUseHostGeneratedGUI = (d->has_editor == 0);

    if (d->wants_input_midi) {
        setupInfo.mNeedsInputMIDI = true;
        setupInfo.mInputMIDINodeName = d->name;
        setupInfo.mInputMIDIChannelMask = 0xFFFF;
    }

    const bool stereo_instrument =
        d->category == static_cast<uint32_t>(AAX_ePlugInCategory_SWGenerators) &&
        d->num_outputs >= 2;

    bool declares_mono = false;
    bool declares_stereo = false;
    if (d->num_layouts > 0 && d->layouts) {
        for (uint32_t i = 0; i < d->num_layouts; i++) {
            if (d->layouts[i].out_channels == 1) declares_mono = true;
            if (d->layouts[i].out_channels == 2) declares_stereo = true;
        }
    } else {
        declares_mono = (d->num_outputs == 1);
        declares_stereo = (d->num_outputs == 2);
    }
    const bool channel_generic = (d->num_layouts == 0 && d->num_outputs <= 2);

    AAX_Result err = AAX_SUCCESS;

    if (!stereo_instrument && (declares_mono || channel_generic)) {
        setupInfo.mInputStemFormat = AAX_eStemFormat_Mono;
        setupInfo.mOutputStemFormat = AAX_eStemFormat_Mono;
        setupInfo.mPluginID = static_cast<int32_t>(d->plugin_type_id);
        err = MooseDescribeOneConfig(desc, setupInfo,
                                     d->emits_midi != 0,
                                     d->name,
                                     d->sidechain_in_channels > 0);
        if (err != AAX_SUCCESS) return err;
    }

    if (declares_stereo || (channel_generic && d->num_outputs >= 2)) {
        setupInfo.mInputStemFormat = d->num_inputs >= 2
            ? AAX_eStemFormat_Stereo : AAX_eStemFormat_Mono;
        setupInfo.mOutputStemFormat = AAX_eStemFormat_Stereo;
        setupInfo.mPluginID = static_cast<int32_t>(d->plugin_type_id ^ 0x00000002u);
        err = MooseDescribeOneConfig(desc, setupInfo,
                                     d->emits_midi != 0,
                                     d->name,
                                     d->sidechain_in_channels > 0);
        if (err != AAX_SUCCESS) return err;
    }

    const uint32_t layoutCount = (d->num_layouts > 0 && d->layouts) ? d->num_layouts : 1;
    for (uint32_t i = 0; i < layoutCount; i++) {
        const int16_t inCh = (d->num_layouts > 0 && d->layouts)
            ? d->layouts[i].in_channels
            : static_cast<int16_t>(d->num_inputs);
        const int16_t outCh = (d->num_layouts > 0 && d->layouts)
            ? d->layouts[i].out_channels
            : static_cast<int16_t>(d->num_outputs);
        if (outCh <= 2) continue;

        AAX_EStemFormat outStem = MooseStemFormat(outCh);
        if (outStem == AAX_eStemFormat_None) continue;
        AAX_EStemFormat inStem = MooseStemFormat(inCh);
        setupInfo.mInputStemFormat = (inStem == AAX_eStemFormat_None) ? outStem : inStem;
        setupInfo.mOutputStemFormat = outStem;
        setupInfo.mPluginID =
            static_cast<int32_t>(d->plugin_type_id ^ (0x00010000u + i));
        err = MooseDescribeOneConfig(desc, setupInfo,
                                     d->emits_midi != 0,
                                     d->name,
                                     d->sidechain_in_channels > 0);
        if (err != AAX_SUCCESS) return err;
    }

    desc->AddProcPtr(
        reinterpret_cast<void*>(MooseAAX_Parameters::Create),
        kAAX_ProcPtrID_Create_EffectParameters);

    if (d->has_editor) {
        desc->AddProcPtr(
            reinterpret_cast<void*>(MooseAAX_GUI::Create),
            kAAX_ProcPtrID_Create_EffectGUI);
    }

    outCollection->AddEffect(d->name, desc);
    outCollection->SetManufacturerName(d->vendor);
    outCollection->AddPackageName(d->name);
    outCollection->SetPackageVersion(d->version_packed);

    return AAX_SUCCESS;
}
