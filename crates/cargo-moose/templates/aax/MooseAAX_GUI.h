#pragma once

#include "AAX_CEffectGUI.h"
#include "MooseAAX_Bridge.h"

class MooseAAX_GUI : public AAX_CEffectGUI {
public:
    MooseAAX_GUI();
    ~MooseAAX_GUI() override;

    static AAX_IEffectGUI* AAX_CALLBACK Create();

    void CreateViewContents() override;
    void CreateViewContainer() override;
    void DeleteViewContainer() override;

    AAX_Result GetViewSize(AAX_Point* oViewSize) const override;
    AAX_Result TimerWakeup() override;
    AAX_Result ParameterUpdated(AAX_CParamID paramID) override;

    bool RequestHostResize(uint32_t width, uint32_t height);

private:
    void* GetRustCtx() const;

    bool mEditorExists;
    bool mViewOpen;
};
