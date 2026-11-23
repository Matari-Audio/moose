#ifndef MOOSE_FAKE_AUDIOTOOLBOX_H
#define MOOSE_FAKE_AUDIOTOOLBOX_H

/*
 * Minimal Linux/POSIX mock of Apple AudioToolbox / CoreFoundation / CoreMIDI /
 * os_lock / Blocks so the real `au_v2_shim.c` compiles and executes under
 * `cargo test` on Linux with zero changes to the macOS build path.
 */

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>

/* Basic Apple scalar typedefs */
typedef int32_t OSStatus;
typedef uint8_t UInt8;
typedef int16_t SInt16;
typedef int32_t SInt32;
typedef uint32_t UInt32;
typedef uint64_t UInt64;
typedef float Float32;
typedef double Float64;
typedef bool Boolean;
typedef uint8_t Byte;
typedef size_t ByteCount;
typedef uint32_t FourCharCode;
typedef intptr_t CFIndex;
typedef uint32_t CFTypeID;
typedef uint32_t CFStringEncoding;

#define noErr ((OSStatus)0)
#define kAudio_ParamError ((OSStatus)-50)
#define kAudio_MemFullError ((OSStatus)-108)

/* os_unfair_lock -> pthread_mutex_t */
typedef pthread_mutex_t os_unfair_lock;
#define OS_UNFAIR_LOCK_INIT ((os_unfair_lock)PTHREAD_MUTEX_INITIALIZER)
static inline void os_unfair_lock_lock(os_unfair_lock *lock) {
    pthread_mutex_lock(lock);
}
static inline void os_unfair_lock_unlock(os_unfair_lock *lock) {
    pthread_mutex_unlock(lock);
}

/* pthread_threadid_np -> POSIX pthread_self cast */
static inline int pthread_threadid_np(pthread_t thread, uint64_t *thread_id) {
    if (thread_id) *thread_id = (uint64_t)(uintptr_t)thread;
    return 0;
}

/* Blocks -> plain function pointers on Linux */
#define Block_copy(b) (b)
#define Block_release(b) ((void)(b))

/* CoreFoundation minimal refcounted types */
typedef const void *CFTypeRef;
typedef const void *CFAllocatorRef;
typedef const void *CFPropertyListRef;
typedef const struct FakeCFString *CFStringRef;
typedef const struct FakeCFData *CFDataRef;
typedef const struct FakeCFNumber *CFNumberRef;
typedef const struct FakeCFArray *CFArrayRef;
typedef struct FakeCFArray *CFMutableArrayRef;
typedef const struct FakeCFDictionary *CFDictionaryRef;
typedef struct FakeCFDictionary *CFMutableDictionaryRef;
typedef const struct FakeCFURL *CFURLRef;

#define kCFAllocatorDefault ((CFAllocatorRef)NULL)
#define kCFStringEncodingUTF8 ((CFStringEncoding)0x08000100u)
#define kCFNumberSInt32Type 3
#define kCFURLPOSIXPathStyle 0

typedef struct { int unused; } CFArrayCallBacks;
typedef struct { int unused; } CFDictionaryKeyCallBacks;
typedef struct { int unused; } CFDictionaryValueCallBacks;
extern const CFArrayCallBacks kCFTypeArrayCallBacks;
extern const CFDictionaryKeyCallBacks kCFTypeDictionaryKeyCallBacks;
extern const CFDictionaryValueCallBacks kCFTypeDictionaryValueCallBacks;

CFTypeRef CFRetain(CFTypeRef cf);
void CFRelease(CFTypeRef cf);
CFTypeID CFGetTypeID(CFTypeRef cf);

CFTypeID CFStringGetTypeID(void);
CFTypeID CFDataGetTypeID(void);
CFTypeID CFDictionaryGetTypeID(void);

CFStringRef fake_cfstr_literal(const char *s);
#define CFSTR(s) fake_cfstr_literal(s)

CFStringRef CFStringCreateWithCString(CFAllocatorRef alloc, const char *cStr,
                                      CFStringEncoding encoding);
CFStringRef CFStringCreateWithFormat(CFAllocatorRef alloc, CFDictionaryRef opts,
                                     CFStringRef fmt, ...);
CFIndex CFStringGetLength(CFStringRef str);
CFIndex CFStringGetMaximumSizeForEncoding(CFIndex length, CFStringEncoding enc);
Boolean CFStringGetCString(CFStringRef str, char *buf, CFIndex bufSize,
                           CFStringEncoding encoding);

CFDataRef CFDataCreate(CFAllocatorRef alloc, const UInt8 *bytes, CFIndex length);
const UInt8 *CFDataGetBytePtr(CFDataRef data);
CFIndex CFDataGetLength(CFDataRef data);

CFNumberRef CFNumberCreate(CFAllocatorRef alloc, int type, const void *valuePtr);

CFMutableArrayRef CFArrayCreateMutable(CFAllocatorRef alloc, CFIndex cap,
                                       const CFArrayCallBacks *callbacks);
void CFArrayAppendValue(CFMutableArrayRef arr, const void *value);

CFMutableDictionaryRef CFDictionaryCreateMutable(
    CFAllocatorRef alloc, CFIndex cap,
    const CFDictionaryKeyCallBacks *keyCallbacks,
    const CFDictionaryValueCallBacks *valueCallbacks);
void CFDictionarySetValue(CFMutableDictionaryRef dict, const void *key,
                          const void *value);
const void *CFDictionaryGetValue(CFDictionaryRef dict, const void *key);

CFURLRef CFURLCreateWithFileSystemPath(CFAllocatorRef alloc, CFStringRef path,
                                       int style, Boolean isDirectory);

/* AudioToolbox / CoreAudio / CoreMIDI types & constants */
typedef uint32_t AudioUnitPropertyID;
typedef uint32_t AudioUnitScope;
typedef uint32_t AudioUnitElement;
typedef uint32_t AudioUnitParameterID;
typedef float AudioUnitParameterValue;
typedef uint32_t AudioUnitRenderActionFlags;
typedef uint32_t AudioUnitEventType;
typedef int64_t AUEventSampleTime;
typedef uint64_t MIDITimeStamp;
typedef int32_t MIDIProtocolID;
typedef void *AudioComponent;
typedef void *AudioComponentInstance;
typedef AudioComponentInstance AudioUnit;
typedef OSStatus (*AudioComponentMethod)(void *self, ...);

#define kMIDIProtocol_1_0 ((MIDIProtocolID)1)
#define kMIDIProtocol_2_0 ((MIDIProtocolID)2)

#define kAudioUnitScope_Global ((AudioUnitScope)0)
#define kAudioUnitScope_Input  ((AudioUnitScope)1)
#define kAudioUnitScope_Output ((AudioUnitScope)2)

#define kAudioFormatLinearPCM 0x6C70636Du
#define kAudioFormatFlagIsFloat (1u << 0)
#define kAudioFormatFlagIsNonInterleaved (1u << 5)
#define kAudioFormatFlagsNativeFloatPacked (kAudioFormatFlagIsFloat | (1u << 3))

#define kAudioTimeStampSampleTimeValid (1u << 0)

#define kAudioUnitRenderAction_PreRender (1u << 2)
#define kAudioUnitRenderAction_PostRender (1u << 3)
#define kAudioUnitRenderAction_OutputIsSilence (1u << 4)

/* Error codes */
#define kAudioUnitErr_CannotDoInCurrentContext ((OSStatus)-10863)
#define kAudioUnitErr_InvalidProperty ((OSStatus)-10879)
#define kAudioUnitErr_InvalidParameter ((OSStatus)-10878)
#define kAudioUnitErr_InvalidElement ((OSStatus)-10877)
#define kAudioUnitErr_InvalidScope ((OSStatus)-10866)
#define kAudioUnitErr_FormatNotSupported ((OSStatus)-10868)
#define kAudioUnitErr_Uninitialized ((OSStatus)-10867)
#define kAudioUnitErr_FailedInitialization ((OSStatus)-10875)
#define kAudioUnitErr_TooManyFramesToProcess ((OSStatus)-10874)
#define kAudioUnitErr_InvalidPropertyValue ((OSStatus)-10851)
#define kAudioUnitErr_Initialized ((OSStatus)-10849)
#define kAudioUnitErr_MIDIOutputBufferFull ((OSStatus)-66753)

/* Property IDs */
#define kAudioUnitProperty_ClassInfo 0u
#define kAudioUnitProperty_MakeConnection 1u
#define kAudioUnitProperty_SampleRate 2u
#define kAudioUnitProperty_ParameterList 3u
#define kAudioUnitProperty_ParameterInfo 4u
#define kAudioUnitProperty_StreamFormat 8u
#define kAudioUnitProperty_ElementCount 11u
#define kAudioUnitProperty_Latency 12u
#define kAudioUnitProperty_SupportedNumChannels 13u
#define kAudioUnitProperty_MaximumFramesPerSlice 14u
#define kAudioUnitProperty_ParameterValueStrings 16u
#define kAudioUnitProperty_AudioChannelLayout 19u
#define kAudioUnitProperty_TailTime 20u
#define kAudioUnitProperty_BypassEffect 21u
#define kAudioUnitProperty_LastRenderError 22u
#define kAudioUnitProperty_SetRenderCallback 23u
#define kAudioUnitProperty_FactoryPresets 24u
#define kAudioUnitProperty_ContextName 25u
#define kAudioUnitProperty_HostCallbacks 27u
#define kAudioUnitProperty_InPlaceProcessing 29u
#define kAudioUnitProperty_ParameterClumpName 31u
#define kAudioUnitProperty_ParameterStringFromValue 33u
#define kAudioUnitProperty_PresentPreset 36u
#define kAudioUnitProperty_OfflineRender 37u
#define kAudioUnitProperty_ParameterValueFromString 38u
#define kAudioUnitProperty_CocoaUI 31u + 1000u
#define kAudioUnitProperty_ShouldAllocateBuffer 51u
#define kAudioUnitProperty_MIDIOutputCallbackInfo 47u
#define kAudioUnitProperty_MIDIOutputCallback 48u
#define kAudioUnitProperty_AllParameterMIDIMappings 41u
#define kAudioUnitProperty_AudioUnitMIDIProtocol 59u
#define kAudioUnitProperty_HostMIDIProtocol 60u
#define kAudioUnitProperty_MIDIOutputEventListCallback 61u

/* Selectors */
#define kAudioUnitInitializeSelect 1
#define kAudioUnitUninitializeSelect 2
#define kAudioUnitGetPropertyInfoSelect 3
#define kAudioUnitGetPropertySelect 4
#define kAudioUnitSetPropertySelect 5
#define kAudioUnitGetParameterSelect 6
#define kAudioUnitSetParameterSelect 7
#define kAudioUnitRenderSelect 14
#define kAudioUnitResetSelect 9
#define kAudioUnitAddPropertyListenerSelect 10
#define kAudioUnitRemovePropertyListenerSelect 11
#define kAudioUnitRemovePropertyListenerWithUserDataSelect 18
#define kAudioUnitScheduleParametersSelect 17
#define kAudioUnitAddRenderNotifySelect 15
#define kAudioUnitRemoveRenderNotifySelect 16
#define kMusicDeviceMIDIEventSelect 257
#define kMusicDeviceMIDIEventListSelect 263

/* Event types */
#define kAudioUnitEvent_ParameterValueChange 0u
#define kAudioUnitEvent_BeginParameterChangeGesture 1u
#define kAudioUnitEvent_EndParameterChangeGesture 2u
#define kAudioUnitEvent_PropertyChange 3u
#define kAUParameterListener_AnyParameter 0xFFFFFFFFu

/* Parameter flags & units */
#define kAudioUnitParameterFlag_CFNameRelease (1u << 4)
#define kAudioUnitParameterFlag_HasClump (1u << 20)
#define kAudioUnitParameterFlag_HasCFNameString (1u << 22)
#define kAudioUnitParameterFlag_ValuesHaveStrings (1u << 21)
#define kAudioUnitParameterFlag_ExpertMode (1u << 24)
#define kAudioUnitParameterFlag_CanRamp (1u << 25)
#define kAudioUnitParameterFlag_IsHighResolution (1u << 29)
#define kAudioUnitParameterFlag_IsReadable (1u << 30)
#define kAudioUnitParameterFlag_IsWritable (1u << 31)

#define kAudioUnitParameterUnit_Generic 0u
#define kAudioUnitParameterUnit_Indexed 1u
#define kAudioUnitParameterUnit_Boolean 2u
#define kAudioUnitParameterUnit_Percent 3u
#define kAudioUnitParameterUnit_Seconds 4u
#define kAudioUnitParameterUnit_Hertz 8u
#define kAudioUnitParameterUnit_Cents 9u
#define kAudioUnitParameterUnit_RelativeSemiTones 10u
#define kAudioUnitParameterUnit_Decibels 13u
#define kAudioUnitParameterUnit_Degrees 17u
#define kAudioUnitParameterUnit_Rate 21u
#define kAudioUnitParameterUnit_Milliseconds 22u
#define kAudioUnitParameterUnit_Ratio 24u
#define kAudioUnitParameterUnit_Octaves 25u

#define kAUParameterMIDIMapping_AnyChannelFlag (1u << 0)

#define kParameterEvent_Immediate 1u
#define kParameterEvent_Ramped 2u

#define kAUPresetNameKey "name"

typedef struct {
    FourCharCode componentType;
    FourCharCode componentSubType;
    FourCharCode componentManufacturer;
    UInt32 componentFlags;
    UInt32 componentFlagsMask;
} AudioComponentDescription;

typedef struct {
    AudioComponentMethod (*Lookup)(SInt16 selector);
    OSStatus (*Open)(void *self, AudioComponentInstance instance);
    OSStatus (*Close)(void *self);
    void *reserved;
} AudioComponentPlugInInterface;

typedef struct {
    Float64 mSampleRate;
    UInt32 mFormatID;
    UInt32 mFormatFlags;
    UInt32 mBytesPerPacket;
    UInt32 mFramesPerPacket;
    UInt32 mBytesPerFrame;
    UInt32 mChannelsPerFrame;
    UInt32 mBitsPerChannel;
    UInt32 mReserved;
} AudioStreamBasicDescription;

typedef struct {
    UInt32 mNumberChannels;
    UInt32 mDataByteSize;
    void *mData;
} AudioBuffer;

typedef struct {
    UInt32 mNumberBuffers;
    AudioBuffer mBuffers[1];
} AudioBufferList;

typedef struct {
    Float64 mSampleTime;
    UInt64 mHostTime;
    Float64 mRateScalar;
    UInt64 mWordClockTime;
    UInt32 mFlags;
    UInt32 mReserved;
} AudioTimeStamp;

typedef struct {
    SInt16 inChannels;
    SInt16 outChannels;
} AUChannelInfo;

typedef struct {
    SInt32 presetNumber;
    CFStringRef presetName;
} AUPreset;

typedef struct {
    char name[52];
    CFStringRef unitName;
    UInt32 clumpID;
    CFStringRef cfNameString;
    UInt32 unit;
    AudioUnitParameterValue minValue;
    AudioUnitParameterValue maxValue;
    AudioUnitParameterValue defaultValue;
    UInt32 flags;
} AudioUnitParameterInfo;

typedef struct {
    AudioUnitParameterID inID;
    SInt32 inDesiredLength;
    CFStringRef outName;
} AudioUnitParameterNameInfo;

typedef struct {
    AudioUnitParameterID inParamID;
    const AudioUnitParameterValue *inValue;
    CFStringRef outString;
} AudioUnitParameterStringFromValue;

typedef struct {
    AudioUnitParameterID inParamID;
    CFStringRef inString;
    AudioUnitParameterValue outValue;
} AudioUnitParameterValueFromString;

typedef struct {
    AudioUnitScope mScope;
    AudioUnitElement mElement;
    AudioUnitParameterID mParameterID;
    UInt32 mFlags;
    UInt32 mSubRangeMin;
    UInt32 mSubRangeMax;
    UInt8 mStatus;
    UInt8 mData1;
    UInt8 mReserved1;
    UInt8 mReserved2;
    UInt32 mReserved3;
} AUParameterMIDIMapping;

typedef struct {
    AudioUnitScope scope;
    AudioUnitElement element;
    AudioUnitParameterID parameter;
    UInt32 eventType;
    union {
        struct {
            SInt32 startBufferOffset;
            UInt32 durationInFrames;
            AudioUnitParameterValue startValue;
            AudioUnitParameterValue endValue;
        } ramp;
        struct {
            UInt32 bufferOffset;
            AudioUnitParameterValue value;
        } immediate;
    } eventValues;
} AudioUnitParameterEvent;

typedef struct {
    AudioUnit mAudioUnit;
    AudioUnitParameterID mParameterID;
    AudioUnitScope mScope;
    AudioUnitElement mElement;
} AudioUnitParameter;

typedef struct {
    AudioUnit mAudioUnit;
    AudioUnitPropertyID mPropertyID;
    AudioUnitScope mScope;
    AudioUnitElement mElement;
} AudioUnitProperty;

typedef struct {
    AudioUnitEventType mEventType;
    union {
        AudioUnitParameter mParameter;
        AudioUnitProperty mProperty;
    } mArgument;
} AudioUnitEvent;

typedef void (*AudioUnitPropertyListenerProc)(void *inRefCon,
                                              AudioUnit inUnit,
                                              AudioUnitPropertyID inID,
                                              AudioUnitScope inScope,
                                              AudioUnitElement inElement);

typedef OSStatus (*AURenderCallback)(void *inRefCon,
                                     AudioUnitRenderActionFlags *ioActionFlags,
                                     const AudioTimeStamp *inTimeStamp,
                                     UInt32 inBusNumber,
                                     UInt32 inNumberFrames,
                                     AudioBufferList *ioData);

typedef struct {
    AURenderCallback inputProc;
    void *inputProcRefCon;
} AURenderCallbackStruct;

typedef struct {
    AudioUnit sourceAudioUnit;
    UInt32 sourceOutputNumber;
    UInt32 destInputNumber;
} AudioUnitConnection;

typedef struct {
    CFURLRef mCocoaAUViewBundleLocation;
    CFStringRef mCocoaAUViewClass[1];
} AudioUnitCocoaViewInfo;

typedef OSStatus (*HostCallback_GetBeatAndTempo)(void *inHostUserData,
                                                 Float64 *outCurrentBeat,
                                                 Float64 *outCurrentTempo);
typedef OSStatus (*HostCallback_GetMusicalTimeLocation)(
    void *inHostUserData, UInt32 *outDeltaSampleOffsetToNextBeat,
    Float32 *outTimeSig_Numerator, UInt32 *outTimeSig_Denominator,
    Float64 *outCurrentMeasureDownBeat);
typedef OSStatus (*HostCallback_GetTransportState)(
    void *inHostUserData, Boolean *outIsPlaying,
    Boolean *outTransportStateChanged, Float64 *outCurrentSampleInTimeLine,
    Boolean *outIsCycling, Float64 *outCycleStartBeat,
    Float64 *outCycleEndBeat);
typedef OSStatus (*HostCallback_GetTransportState2)(
    void *inHostUserData, Boolean *outIsPlaying, Boolean *outIsRecording,
    Boolean *outTransportStateChanged, Float64 *outCurrentSampleInTimeLine,
    Boolean *outIsCycling, Float64 *outCycleStartBeat,
    Float64 *outCycleEndBeat);

typedef struct {
    void *hostUserData;
    HostCallback_GetBeatAndTempo beatAndTempoProc;
    HostCallback_GetMusicalTimeLocation musicalTimeLocationProc;
    HostCallback_GetTransportState transportStateProc;
    HostCallback_GetTransportState2 transportStateProc2;
} HostCallbackInfo;

/* CoreMIDI minimal structs */
typedef struct {
    MIDITimeStamp timeStamp;
    uint16_t length;
    Byte data[256];
} MIDIPacket;

typedef struct {
    UInt32 numPackets;
    MIDIPacket packet[1];
} MIDIPacketList;

typedef struct {
    MIDITimeStamp timeStamp;
    UInt32 wordCount;
    UInt32 words[64];
} MIDIEventPacket;

typedef struct {
    MIDIProtocolID protocol;
    UInt32 numPackets;
    MIDIEventPacket packet[1];
} MIDIEventList;

typedef OSStatus (*AUMIDIOutputCallback)(void *userData,
                                         const AudioTimeStamp *timeStamp,
                                         UInt32 midiOutNum,
                                         const MIDIPacketList *pktlist);

typedef struct {
    AUMIDIOutputCallback midiOutputCallback;
    void *userData;
} AUMIDIOutputCallbackStruct;

typedef OSStatus (*AUMIDIEventListBlock)(AUEventSampleTime eventSampleTime,
                                         uint8_t cable,
                                         const MIDIEventList *eventList);

/* AudioToolbox / CoreMIDI mock functions */
OSStatus AUEventListenerNotify(void *inSendingListener, void *inSendingObject,
                               const AudioUnitEvent *inEvent);
OSStatus AudioUnitSetParameter(AudioUnit inUnit, AudioUnitParameterID inID,
                               AudioUnitScope inScope, AudioUnitElement inElement,
                               AudioUnitParameterValue inValue,
                               UInt32 inBufferOffsetInFrames);
OSStatus AudioUnitRender(AudioUnit inUnit,
                         AudioUnitRenderActionFlags *ioActionFlags,
                         const AudioTimeStamp *inTimeStamp,
                         UInt32 inOutputBusNumber,
                         UInt32 inNumberFrames,
                         AudioBufferList *ioData);
AudioComponent AudioComponentInstanceGetComponent(AudioComponentInstance inst);
OSStatus AudioComponentCopyName(AudioComponent comp, CFStringRef *outName);

MIDIPacket *MIDIPacketListInit(MIDIPacketList *pktlist);
MIDIPacket *MIDIPacketListAdd(MIDIPacketList *pktlist, ByteCount listSize,
                              MIDIPacket *curPacket, MIDITimeStamp time,
                              ByteCount nData, const Byte *data);
MIDIEventPacket *MIDIEventListInit(MIDIEventList *evtlist,
                                   MIDIProtocolID protocol);
MIDIEventPacket *MIDIEventListAdd(MIDIEventList *evtlist, ByteCount listSize,
                                  MIDIEventPacket *curPacket,
                                  MIDITimeStamp time, ByteCount wordCount,
                                  const UInt32 *words);
const MIDIEventPacket *MIDIEventPacketNext(const MIDIEventPacket *pkt);

/* Test-harness inspection helpers */
typedef struct {
    uint32_t event_type;
    void *audio_unit;
    uint32_t id;
    uint32_t scope;
    uint32_t element;
} FakeAuRecordedEvent;

void fake_au_reset_events(void);
uint32_t fake_au_take_events(FakeAuRecordedEvent *out, uint32_t cap);
void moose_au_test_reset_registration(void);

#endif /* MOOSE_FAKE_AUDIOTOOLBOX_H */
