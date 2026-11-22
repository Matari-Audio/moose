/*
 * Lightweight Linux/POSIX implementation of `fake_audiotoolbox.h` for testing
 * the real `au_v2_shim.c` under `cargo test` on Linux.
 */

#define _GNU_SOURCE
#include "fake_audiotoolbox.h"

#include <stdatomic.h>

enum {
    FAKE_CF_STRING = 1,
    FAKE_CF_DATA = 2,
    FAKE_CF_NUMBER = 3,
    FAKE_CF_ARRAY = 4,
    FAKE_CF_DICT = 5,
    FAKE_CF_URL = 6,
};

typedef struct {
    uint32_t type_id;
    atomic_int refcount;
    int is_static;
} FakeCFHeader;

struct FakeCFString {
    FakeCFHeader hdr;
    char *str;
    size_t len;
};

struct FakeCFData {
    FakeCFHeader hdr;
    uint8_t *bytes;
    size_t len;
};

struct FakeCFNumber {
    FakeCFHeader hdr;
    int32_t val;
};

struct FakeCFArray {
    FakeCFHeader hdr;
    const void **items;
    size_t len;
    size_t cap;
};

typedef struct {
    char *key;
    const void *val;
} FakeDictEntry;

struct FakeCFDictionary {
    FakeCFHeader hdr;
    FakeDictEntry *entries;
    size_t len;
    size_t cap;
};

struct FakeCFURL {
    FakeCFHeader hdr;
    char *path;
};

const CFArrayCallBacks kCFTypeArrayCallBacks = { 0 };
const CFDictionaryKeyCallBacks kCFTypeDictionaryKeyCallBacks = { 0 };
const CFDictionaryValueCallBacks kCFTypeDictionaryValueCallBacks = { 0 };

CFTypeRef CFRetain(CFTypeRef cf) {
    if (!cf) return NULL;
    FakeCFHeader *h = (FakeCFHeader *)cf;
    if (!h->is_static) {
        atomic_fetch_add_explicit(&h->refcount, 1, memory_order_relaxed);
    }
    return cf;
}

void CFRelease(CFTypeRef cf) {
    if (!cf) return;
    FakeCFHeader *h = (FakeCFHeader *)cf;
    if (h->is_static) return;
    if (atomic_fetch_sub_explicit(&h->refcount, 1, memory_order_acq_rel) != 1) {
        return;
    }
    switch (h->type_id) {
        case FAKE_CF_STRING: {
            struct FakeCFString *s = (struct FakeCFString *)cf;
            free(s->str);
            free(s);
            break;
        }
        case FAKE_CF_DATA: {
            struct FakeCFData *d = (struct FakeCFData *)cf;
            free(d->bytes);
            free(d);
            break;
        }
        case FAKE_CF_NUMBER: {
            free((void *)cf);
            break;
        }
        case FAKE_CF_ARRAY: {
            struct FakeCFArray *a = (struct FakeCFArray *)cf;
            for (size_t i = 0; i < a->len; i++) {
                CFRelease(a->items[i]);
            }
            free(a->items);
            free(a);
            break;
        }
        case FAKE_CF_DICT: {
            struct FakeCFDictionary *d = (struct FakeCFDictionary *)cf;
            for (size_t i = 0; i < d->len; i++) {
                free(d->entries[i].key);
                CFRelease(d->entries[i].val);
            }
            free(d->entries);
            free(d);
            break;
        }
        case FAKE_CF_URL: {
            struct FakeCFURL *u = (struct FakeCFURL *)cf;
            free(u->path);
            free(u);
            break;
        }
        default:
            free((void *)cf);
            break;
    }
}

CFTypeID CFGetTypeID(CFTypeRef cf) {
    if (!cf) return 0;
    return ((const FakeCFHeader *)cf)->type_id;
}

CFTypeID CFStringGetTypeID(void) { return FAKE_CF_STRING; }
CFTypeID CFDataGetTypeID(void) { return FAKE_CF_DATA; }
CFTypeID CFDictionaryGetTypeID(void) { return FAKE_CF_DICT; }

/* Interned static literals so CFSTR("...") never needs explicit release. */
static pthread_mutex_t g_lit_lock = PTHREAD_MUTEX_INITIALIZER;
static struct FakeCFString *g_lits[128];
static size_t g_lit_count = 0;

CFStringRef fake_cfstr_literal(const char *s) {
    if (!s) return NULL;
    pthread_mutex_lock(&g_lit_lock);
    for (size_t i = 0; i < g_lit_count; i++) {
        if (strcmp(g_lits[i]->str, s) == 0) {
            CFStringRef found = g_lits[i];
            pthread_mutex_unlock(&g_lit_lock);
            return found;
        }
    }
    struct FakeCFString *obj = (struct FakeCFString *)calloc(1, sizeof(*obj));
    obj->hdr.type_id = FAKE_CF_STRING;
    obj->hdr.is_static = 1;
    atomic_init(&obj->hdr.refcount, 1);
    obj->len = strlen(s);
    obj->str = strdup(s);
    if (g_lit_count < 128) {
        g_lits[g_lit_count++] = obj;
    }
    pthread_mutex_unlock(&g_lit_lock);
    return obj;
}

CFStringRef CFStringCreateWithCString(CFAllocatorRef alloc, const char *cStr,
                                      CFStringEncoding encoding) {
    (void)alloc; (void)encoding;
    if (!cStr) return NULL;
    struct FakeCFString *obj = (struct FakeCFString *)calloc(1, sizeof(*obj));
    if (!obj) return NULL;
    obj->hdr.type_id = FAKE_CF_STRING;
    obj->hdr.is_static = 0;
    atomic_init(&obj->hdr.refcount, 1);
    obj->len = strlen(cStr);
    obj->str = strdup(cStr);
    return obj;
}

CFStringRef CFStringCreateWithFormat(CFAllocatorRef alloc, CFDictionaryRef opts,
                                     CFStringRef fmt, ...) {
    (void)alloc; (void)opts;
    if (!fmt || !fmt->str) return NULL;
    char buf[256];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof(buf), fmt->str, ap);
    va_end(ap);
    return CFStringCreateWithCString(alloc, buf, kCFStringEncodingUTF8);
}

CFIndex CFStringGetLength(CFStringRef str) {
    return str ? (CFIndex)str->len : 0;
}

CFIndex CFStringGetMaximumSizeForEncoding(CFIndex length, CFStringEncoding enc) {
    (void)enc;
    return length * 4 + 1;
}

Boolean CFStringGetCString(CFStringRef str, char *buf, CFIndex bufSize,
                           CFStringEncoding encoding) {
    (void)encoding;
    if (!str || !buf || bufSize <= 0) return false;
    if (str->len + 1 > (size_t)bufSize) return false;
    memcpy(buf, str->str, str->len + 1);
    return true;
}

CFDataRef CFDataCreate(CFAllocatorRef alloc, const UInt8 *bytes, CFIndex length) {
    (void)alloc;
    if (length < 0) return NULL;
    struct FakeCFData *d = (struct FakeCFData *)calloc(1, sizeof(*d));
    if (!d) return NULL;
    d->hdr.type_id = FAKE_CF_DATA;
    atomic_init(&d->hdr.refcount, 1);
    d->len = (size_t)length;
    if (length > 0 && bytes) {
        d->bytes = (uint8_t *)malloc((size_t)length);
        memcpy(d->bytes, bytes, (size_t)length);
    }
    return d;
}

const UInt8 *CFDataGetBytePtr(CFDataRef data) {
    return data ? data->bytes : NULL;
}

CFIndex CFDataGetLength(CFDataRef data) {
    return data ? (CFIndex)data->len : 0;
}

CFNumberRef CFNumberCreate(CFAllocatorRef alloc, int type, const void *valuePtr) {
    (void)alloc; (void)type;
    struct FakeCFNumber *n = (struct FakeCFNumber *)calloc(1, sizeof(*n));
    if (!n) return NULL;
    n->hdr.type_id = FAKE_CF_NUMBER;
    atomic_init(&n->hdr.refcount, 1);
    if (valuePtr) n->val = *(const int32_t *)valuePtr;
    return n;
}

CFMutableArrayRef CFArrayCreateMutable(CFAllocatorRef alloc, CFIndex cap,
                                       const CFArrayCallBacks *callbacks) {
    (void)alloc; (void)callbacks;
    struct FakeCFArray *a = (struct FakeCFArray *)calloc(1, sizeof(*a));
    if (!a) return NULL;
    a->hdr.type_id = FAKE_CF_ARRAY;
    atomic_init(&a->hdr.refcount, 1);
    if (cap > 0) {
        a->items = (const void **)calloc((size_t)cap, sizeof(void *));
        a->cap = (size_t)cap;
    }
    return a;
}

void CFArrayAppendValue(CFMutableArrayRef arr, const void *value) {
    if (!arr) return;
    if (arr->len == arr->cap) {
        size_t next = arr->cap ? arr->cap * 2 : 8;
        arr->items = (const void **)realloc(arr->items, next * sizeof(void *));
        arr->cap = next;
    }
    arr->items[arr->len++] = CFRetain(value);
}

CFMutableDictionaryRef CFDictionaryCreateMutable(
    CFAllocatorRef alloc, CFIndex cap,
    const CFDictionaryKeyCallBacks *keyCallbacks,
    const CFDictionaryValueCallBacks *valueCallbacks) {
    (void)alloc; (void)keyCallbacks; (void)valueCallbacks;
    struct FakeCFDictionary *d = (struct FakeCFDictionary *)calloc(1, sizeof(*d));
    if (!d) return NULL;
    d->hdr.type_id = FAKE_CF_DICT;
    atomic_init(&d->hdr.refcount, 1);
    if (cap > 0) {
        d->entries = (FakeDictEntry *)calloc((size_t)cap, sizeof(FakeDictEntry));
        d->cap = (size_t)cap;
    }
    return d;
}

void CFDictionarySetValue(CFMutableDictionaryRef dict, const void *key,
                          const void *value) {
    if (!dict || !key) return;
    const struct FakeCFString *ks = (const struct FakeCFString *)key;
    for (size_t i = 0; i < dict->len; i++) {
        if (strcmp(dict->entries[i].key, ks->str) == 0) {
            CFRelease(dict->entries[i].val);
            dict->entries[i].val = CFRetain(value);
            return;
        }
    }
    if (dict->len == dict->cap) {
        size_t next = dict->cap ? dict->cap * 2 : 8;
        dict->entries = (FakeDictEntry *)realloc(dict->entries, next * sizeof(FakeDictEntry));
        dict->cap = next;
    }
    dict->entries[dict->len].key = strdup(ks->str);
    dict->entries[dict->len].val = CFRetain(value);
    dict->len++;
}

const void *CFDictionaryGetValue(CFDictionaryRef dict, const void *key) {
    if (!dict || !key) return NULL;
    const struct FakeCFString *ks = (const struct FakeCFString *)key;
    for (size_t i = 0; i < dict->len; i++) {
        if (strcmp(dict->entries[i].key, ks->str) == 0) {
            return dict->entries[i].val;
        }
    }
    return NULL;
}

CFURLRef CFURLCreateWithFileSystemPath(CFAllocatorRef alloc, CFStringRef path,
                                       int style, Boolean isDirectory) {
    (void)alloc; (void)style; (void)isDirectory;
    struct FakeCFURL *u = (struct FakeCFURL *)calloc(1, sizeof(*u));
    if (!u) return NULL;
    u->hdr.type_id = FAKE_CF_URL;
    atomic_init(&u->hdr.refcount, 1);
    u->path = path && path->str ? strdup(path->str) : strdup("");
    return u;
}

/* Recorded AUEventListenerNotify log */
#define FAKE_AU_EVENT_LOG_CAP 1024
static pthread_mutex_t g_ev_lock = PTHREAD_MUTEX_INITIALIZER;
static FakeAuRecordedEvent g_ev_log[FAKE_AU_EVENT_LOG_CAP];
static uint32_t g_ev_count = 0;

void fake_au_reset_events(void) {
    pthread_mutex_lock(&g_ev_lock);
    g_ev_count = 0;
    pthread_mutex_unlock(&g_ev_lock);
}

uint32_t fake_au_take_events(FakeAuRecordedEvent *out, uint32_t cap) {
    pthread_mutex_lock(&g_ev_lock);
    uint32_t n = g_ev_count < cap ? g_ev_count : cap;
    if (out && n > 0) {
        memcpy(out, g_ev_log, n * sizeof(FakeAuRecordedEvent));
    }
    g_ev_count = 0;
    pthread_mutex_unlock(&g_ev_lock);
    return n;
}

OSStatus AUEventListenerNotify(void *inSendingListener, void *inSendingObject,
                               const AudioUnitEvent *inEvent) {
    (void)inSendingListener; (void)inSendingObject;
    if (!inEvent) return noErr;
    pthread_mutex_lock(&g_ev_lock);
    if (g_ev_count < FAKE_AU_EVENT_LOG_CAP) {
        FakeAuRecordedEvent *rec = &g_ev_log[g_ev_count++];
        rec->event_type = inEvent->mEventType;
        if (inEvent->mEventType == kAudioUnitEvent_PropertyChange) {
            rec->audio_unit = inEvent->mArgument.mProperty.mAudioUnit;
            rec->id = inEvent->mArgument.mProperty.mPropertyID;
            rec->scope = inEvent->mArgument.mProperty.mScope;
            rec->element = inEvent->mArgument.mProperty.mElement;
        } else {
            rec->audio_unit = inEvent->mArgument.mParameter.mAudioUnit;
            rec->id = inEvent->mArgument.mParameter.mParameterID;
            rec->scope = inEvent->mArgument.mParameter.mScope;
            rec->element = inEvent->mArgument.mParameter.mElement;
        }
    }
    pthread_mutex_unlock(&g_ev_lock);
    return noErr;
}

typedef OSStatus (*SetParamFn)(void *, AudioUnitParameterID, AudioUnitScope,
                               AudioUnitElement, AudioUnitParameterValue, UInt32);

OSStatus AudioUnitSetParameter(AudioUnit inUnit, AudioUnitParameterID inID,
                               AudioUnitScope inScope, AudioUnitElement inElement,
                               AudioUnitParameterValue inValue,
                               UInt32 inBufferOffsetInFrames) {
    if (!inUnit) return kAudio_ParamError;
    AudioComponentPlugInInterface *iface = (AudioComponentPlugInInterface *)inUnit;
    if (!iface->Lookup) return kAudio_ParamError;
    SetParamFn fn = (SetParamFn)iface->Lookup(kAudioUnitSetParameterSelect);
    if (!fn) return kAudio_ParamError;
    return fn(inUnit, inID, inScope, inElement, inValue, inBufferOffsetInFrames);
}

OSStatus AudioUnitRender(AudioUnit inUnit,
                         AudioUnitRenderActionFlags *ioActionFlags,
                         const AudioTimeStamp *inTimeStamp,
                         UInt32 inOutputBusNumber,
                         UInt32 inNumberFrames,
                         AudioBufferList *ioData) {
    (void)inUnit; (void)ioActionFlags; (void)inTimeStamp;
    (void)inOutputBusNumber; (void)inNumberFrames; (void)ioData;
    return noErr;
}

AudioComponent AudioComponentInstanceGetComponent(AudioComponentInstance inst) {
    return (AudioComponent)inst;
}

OSStatus AudioComponentCopyName(AudioComponent comp, CFStringRef *outName) {
    (void)comp;
    if (outName) *outName = CFStringCreateWithCString(NULL, "FakeAU", kCFStringEncodingUTF8);
    return noErr;
}

MIDIPacket *MIDIPacketListInit(MIDIPacketList *pktlist) {
    pktlist->numPackets = 0;
    return &pktlist->packet[0];
}

MIDIPacket *MIDIPacketListAdd(MIDIPacketList *pktlist, ByteCount listSize,
                              MIDIPacket *curPacket, MIDITimeStamp time,
                              ByteCount nData, const Byte *data) {
    if (!pktlist || !curPacket || nData > sizeof(curPacket->data)) return NULL;
    (void)listSize;
    curPacket->timeStamp = time;
    curPacket->length = (uint16_t)nData;
    if (nData > 0 && data) memcpy(curPacket->data, data, nData);
    pktlist->numPackets++;
    return curPacket + 1;
}

MIDIEventPacket *MIDIEventListInit(MIDIEventList *evtlist,
                                   MIDIProtocolID protocol) {
    evtlist->protocol = protocol;
    evtlist->numPackets = 0;
    return &evtlist->packet[0];
}

MIDIEventPacket *MIDIEventListAdd(MIDIEventList *evtlist, ByteCount listSize,
                                  MIDIEventPacket *curPacket,
                                  MIDITimeStamp time, ByteCount wordCount,
                                  const UInt32 *words) {
    if (!evtlist || !curPacket || wordCount > 64) return NULL;
    (void)listSize;
    curPacket->timeStamp = time;
    curPacket->wordCount = (UInt32)wordCount;
    if (wordCount > 0 && words) {
        memcpy(curPacket->words, words, wordCount * sizeof(UInt32));
    }
    evtlist->numPackets++;
    return curPacket + 1;
}

const MIDIEventPacket *MIDIEventPacketNext(const MIDIEventPacket *pkt) {
    return pkt + 1;
}

const char *moose_au_view_factory_class_name(void) {
    return "MooseAUCocoaViewProxy_linux_harness";
}
