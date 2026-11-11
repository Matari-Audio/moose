// Editor -> host calls made off the host UI thread (a Linux editor's own
// X11 thread) reach IComponentHandler and IPlugFrame only on the UI thread,
// in order, woken through the host IRunLoop's event handler.
#include "../../shim/vst3_shim.cpp"
#include <thread>

static int context_token;
static void require_impl(bool ok, const char* condition, int line) {
    if (!ok) { std::fprintf(stderr, "FAIL line %d: %s\n", line, condition); std::abort(); }
}
#define require(value) require_impl((value), #value, __LINE__)

static pthread_t ui;
static std::string calls;
static void record(char c) { require(pthread_equal(pthread_self(), ui)); calls += c; }

struct Handler { void** vtbl; };
static tresult h_query(void*, const TUID, void**) { return kNoInterface; }
static uint32 h_ref(void*) { return 1; }
static tresult h_begin(void*, uint32) { record('b'); return kResultOk; }
static tresult h_perform(void*, uint32, double v) { record(v == 0.5 ? 'p' : '?'); return kResultOk; }
static tresult h_end(void*, uint32) { record('e'); return kResultOk; }
static tresult h_restart(void*, int32) { return kResultOk; }

struct RunLoop {
    void** vtbl;
    void* handler = nullptr;
    int fd = -1;
    int timers = 0;
};
static uint32 loop_ref(void*) { return 1; }
static tresult loop_add_event(void* self, void* handler, int fd) {
    auto* loop = static_cast<RunLoop*>(self); loop->handler = handler; loop->fd = fd; return kResultOk;
}
static tresult loop_remove_event(void* self, void* handler) {
    auto* loop = static_cast<RunLoop*>(self); require(loop->handler == handler);
    loop->handler = nullptr; loop->fd = -1; return kResultOk;
}
static tresult loop_add_timer(void* self, void*, uint64_t) { ++static_cast<RunLoop*>(self)->timers; return kResultOk; }
static tresult loop_remove_timer(void* self, void*) { --static_cast<RunLoop*>(self)->timers; return kResultOk; }
// What the host's loop does when the fd is readable: call onFDIsSet on its thread.
static void pump(RunLoop& loop) {
    require(loop.handler);
    auto on_fd = (void (*)(void*, int))(*(void***)loop.handler)[3];
    on_fd(loop.handler, loop.fd);
}
static bool readable(int fd) { uint64_t v; return read(fd, &v, sizeof(v)) > 0 && (write(fd, &v, sizeof(v)), true); }

struct Frame { void** vtbl; RunLoop* loop; int resizes = 0; };
static tresult frame_query(void* self, const TUID iid, void** out) {
    auto* frame = static_cast<Frame*>(self);
    if (iid_equal(iid, IRunLoop_iid)) { *out = frame->loop; return kResultOk; }
    *out = nullptr; return kNoInterface;
}
static tresult frame_resize(void* self, void*, ViewRect* rect) {
    record('r'); require(rect->right == 640 && rect->bottom == 480);
    ++static_cast<Frame*>(self)->resizes; return kResultOk;
}

static void off_ui(void (*f)()) { std::thread(f).join(); }

int main() {
    ui = pthread_self();
    Vst3Callbacks callbacks{};
    callbacks.wants_persist_notifications = [](void*) -> bool { return false; };
    callbacks.create = []() -> void* { return &context_token; };
    callbacks.destroy = [](void*) {};
    callbacks.gui_has_editor = [](void*) -> int32_t { return 1; };
    callbacks.gui_close = [](void*) {};
    callbacks.param_presentation_revision = [](void*) -> uint64_t { return 0; };
    g_cb = &callbacks;

    void* handler_vtbl[] = {(void*)h_query, (void*)h_ref, (void*)h_ref,
        (void*)h_begin, (void*)h_perform, (void*)h_end, (void*)h_restart};
    Handler handler{handler_vtbl};
    void* loop_vtbl[] = {nullptr, (void*)loop_ref, (void*)loop_ref, (void*)loop_add_event,
        (void*)loop_remove_event, (void*)loop_add_timer, (void*)loop_remove_timer};
    RunLoop loop{loop_vtbl};
    void* frame_vtbl[] = {(void*)frame_query, (void*)loop_ref, (void*)loop_ref, (void*)frame_resize};
    Frame frame{frame_vtbl, &loop};

    auto* com = create_component();
    require(com);
    static void* ctx; ctx = com->impl.rustContext();
    com->impl.setComponentHandler(&handler);
    auto* view = static_cast<MoosePlugView*>(com->vtbl_controller->createView(&com->vtbl_controller, "editor"));
    require(view->vtbl->setFrame(view, &frame) == kResultOk && loop.handler && loop.timers == 1);

    // A gesture and a resize from the editor's own thread wait for the loop.
    off_ui([] {
        moose_vst3_begin_edit(ctx, 7);
        moose_vst3_perform_edit(ctx, 7, 0.5);
        moose_vst3_end_edit(ctx, 7);
        require(moose_vst3_request_resize(ctx, 640, 480) == 1);
    });
    require(calls.empty() && frame.resizes == 0 && readable(loop.fd));
    pump(loop);
    require(calls == "bper" && frame.resizes == 1);

    // A call on the UI thread goes first through whatever is still queued.
    calls.clear();
    off_ui([] { moose_vst3_begin_edit(ctx, 7); });
    moose_vst3_end_edit(ctx, 7);
    require(calls == "be");

    // Detaching delivers the rest: the host never stays mid-gesture.
    calls.clear();
    off_ui([] { moose_vst3_begin_edit(ctx, 7); moose_vst3_end_edit(ctx, 7); });
    require(view->vtbl->setFrame(view, nullptr) == kResultOk);
    require(calls == "be" && !loop.handler && loop.timers == 0);

    // With no run loop bound, calls go straight through (the old path).
    calls.clear();
    moose_vst3_begin_edit(ctx, 7);
    require(calls == "b");
    view->vtbl->release(view);
    com->vtbl_component->release(com);
    std::puts("PASS: off-UI-thread edits and resizes reach the host on its run loop, in order");
}
