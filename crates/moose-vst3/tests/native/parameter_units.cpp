// Exercise the real shim's registration, ParameterInfo and IUnitInfo methods.
// Override the include path to run the same assertions against a main control.
#ifndef MOOSE_VST3_SHIM_PATH
#define MOOSE_VST3_SHIM_PATH "../../shim/vst3_shim.cpp"
#endif
#include MOOSE_VST3_SHIM_PATH
#include <cassert>
#include <string>
#include <vector>

int main() {
    std::vector<std::string> groups;
    groups.reserve(100);
    for (int index = 0; index < 96; ++index)
        groups.push_back("Oscillator " + std::to_string(index + 1) + "/Unison");
    const std::string long_prefix(180, 'x');
    groups.push_back(long_prefix + "A/Leaf");
    groups.push_back(long_prefix + "B/Leaf");
    groups.push_back("");
    std::vector<Vst3ParamDescriptor> params(5004);
    for (size_t index = 0; index < params.size(); ++index) {
        auto &param = params[index];
        param.id = 0x01000000 + static_cast<uint32_t>(index);
        param.name = "Voices";
        param.short_name = "Voices";
        param.units = "";
        param.min = 1;
        param.max = 64;
        param.step_count = 63;
        param.default_normalized = 0;
        param.flags = 1;
        param.group = groups[index < 5001 ? index % 96 : 96 + (index - 5001)].c_str();
    }
    moose_vst3_register(nullptr, nullptr, params.data(), static_cast<uint32_t>(params.size()));
    assert(ui_getUnitCount(nullptr) == 197); // root + 98 parents/children
    MooseComponent component;
    for (int32 index : {0, 4095, 4096, 5000}) {
        ParameterInfo info{};
        assert(component.getParameterInfo(index, &info) == kResultOk);
        assert(info.id == params[index].id);
        assert(info.stepCount == 63);
        assert(info.unitId != 0);
        UnitInfo child{}, parent{};
        assert(ui_getUnitInfo(nullptr, info.unitId, &child) == kResultOk);
        assert(ui_getUnitInfo(nullptr, child.parentUnitId, &parent) == kResultOk);
        assert(parent.parentUnitId == 0);
        assert(child.programListId == -1);
    }
    ParameterInfo a{}, b{}, root{};
    assert(component.getParameterInfo(5001, &a) == kResultOk);
    assert(component.getParameterInfo(5002, &b) == kResultOk);
    assert(a.unitId != b.unitId); // full paths remain distinct beyond 128 bytes
    assert(component.getParameterInfo(5003, &root) == kResultOk);
    assert(root.unitId == 0);
    assert(component.getParameterInfo(5004, &root) == kInvalidArgument);
    assert(ui_getUnitInfo(nullptr, 197, nullptr) == kInvalidArgument);
    std::puts("VST3 5004 descriptors / 197 static hierarchical units passed");
}
