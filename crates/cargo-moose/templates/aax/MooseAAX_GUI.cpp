#include "MooseAAX_GUI.h"
#include "MooseAAX_Parameters.h"

#include "AAX_GUITypes.h"
#include "AAX_IViewContainer.h"

#ifdef __APPLE__
extern "C" void* objc_autoreleasePoolPush();
extern "C" void objc_autoreleasePoolPop(void*);
#endif

AAX_IEffectGUI* AAX_CALLBACK MooseAAX_GUI::Create() {
    return new MooseAAX_GUI();
}

MooseAAX_GUI::MooseAAX_GUI()
    : AAX_CEffectGUI()
    , mEditorExists(false)
    , mViewOpen(false) {}

MooseAAX_GUI::~MooseAAX_GUI() {
    auto* params = dynamic_cast<MooseAAX_Parameters*>(GetEffectParameters());
    if (params) params->SetActiveGUI(nullptr);
}

void* MooseAAX_GUI::GetRustCtx() const {
    auto* params = dynamic_cast<MooseAAX_Parameters*>(
        const_cast<MooseAAX_GUI*>(this)->GetEffectParameters());
    return params ? params->GetRustCtx() : nullptr;
}

void MooseAAX_GUI::CreateViewContents() {
    const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
    void* ctx = GetRustCtx();
    if (!ctx || !cbs) return;
    mEditorExists = (cbs->editor_has(ctx) != 0);
    auto* params = dynamic_cast<MooseAAX_Parameters*>(GetEffectParameters());
    if (params) params->SetActiveGUI(this);
}

void MooseAAX_GUI::CreateViewContainer() {
    if (!mEditorExists) return;
    const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
    void* ctx = GetRustCtx();
    if (!ctx || !cbs) return;

    void* parentView = GetViewContainerPtr();
    int32_t platform = MOOSE_AAX_VIEW_UNKNOWN;
    switch (GetViewContainerType()) {
        case AAX_eViewContainer_Type_NSView: platform = MOOSE_AAX_VIEW_NSVIEW; break;
        case AAX_eViewContainer_Type_HWND:   platform = MOOSE_AAX_VIEW_HWND; break;
        default:                             platform = MOOSE_AAX_VIEW_UNKNOWN; break;
    }

    mViewOpen = (cbs->editor_open(ctx, parentView, platform) != 0);
}

void MooseAAX_GUI::DeleteViewContainer() {
    if (!mViewOpen) return;
    const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
    void* ctx = GetRustCtx();
    if (ctx && cbs) {
#ifdef __APPLE__
        void* pool = objc_autoreleasePoolPush();
        cbs->editor_close(ctx);
        objc_autoreleasePoolPop(pool);
#else
        cbs->editor_close(ctx);
#endif
    }
    mViewOpen = false;
}

AAX_Result MooseAAX_GUI::GetViewSize(AAX_Point* oViewSize) const {
    const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
    void* ctx = GetRustCtx();
    if (!ctx || !mEditorExists || !cbs || !oViewSize) {
        return AAX_ERROR_NULL_OBJECT;
    }
    uint32_t w = 0, h = 0;
    if (cbs->editor_get_size(ctx, &w, &h)) {
        oViewSize->horz = static_cast<float>(w);
        oViewSize->vert = static_cast<float>(h);
        return AAX_SUCCESS;
    }
    return AAX_ERROR_NULL_OBJECT;
}

AAX_Result MooseAAX_GUI::TimerWakeup() {
    const MooseAaxCallbacks* cbs = MooseAax_Callbacks();
    void* ctx = GetRustCtx();
    if (ctx && cbs) {
        if (mViewOpen && mEditorExists) {
            cbs->editor_idle(ctx);
        }
        cbs->drain_host_queue(ctx);
    }
    return AAX_SUCCESS;
}

AAX_Result MooseAAX_GUI::ParameterUpdated(AAX_CParamID /*paramID*/) {
    return AAX_SUCCESS;
}

bool MooseAAX_GUI::RequestHostResize(uint32_t width, uint32_t height) {
    if (!mViewOpen) return false;
    AAX_IViewContainer* container = GetViewContainer();
    if (!container) return false;
    AAX_Point size;
    size.horz = static_cast<float>(width);
    size.vert = static_cast<float>(height);
    return container->SetViewSize(size) == AAX_SUCCESS;
}
