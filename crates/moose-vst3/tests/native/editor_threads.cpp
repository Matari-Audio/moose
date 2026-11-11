// Editor -> host calls made off the host UI thread (a Linux editor's own
// X11 thread) reach IComponentHandler and IPlugFrame only on the UI thread,
// in order, woken through the host IRunLoop's event handler.
#include "../../shim/vst3_shim.cpp"
#include <thread>
#include <fcntl.h>

static int context_token;
static void require_impl(bool ok, const char* condition, int line) {
    if (!ok) { std::fprintf(stderr, "FAIL line %d: %s\n", line, condition); std::abort(); }
}
#define require(value) require_impl((value), #value, __LINE__)

static pthread_t ui;
static std::string calls;
static void record(char c) { require(pthread_equal(pthread_self(), ui)); calls += c; }

struct Handler { void** vtbl; bool replacement = false; };
static bool workerOnBegin = false;
static bool emitOnResize = false;
static bool workerOnResize = false;
static tresult h_query(void*, const TUID, void**) { return kNoInterface; }
static uint32 h_ref(void*) { return 1; }
static tresult h_begin(void* self, uint32) { record(static_cast<Handler*>(self)->replacement ? 'B' : 'b');
    if (workerOnBegin) {
        workerOnBegin = false;
        std::thread([] { moose_vst3_end_edit(&context_token, 7); }).join();
    }
    return kResultOk; }
static bool flood = false;
static tresult h_perform(void*, uint32, double v) {
    record(v == 0.5 ? 'p' : '?');
    // A worker that submits again for every edit the host takes.
    if (flood) std::thread([] { moose_vst3_perform_edit(&context_token, 7, 0.5); }).join();
    return kResultOk;
}
static tresult h_end(void* self, uint32) { record(static_cast<Handler*>(self)->replacement ? 'E' : 'e'); return kResultOk; }
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
static tresult frame_resize(void* self, void* view, ViewRect* rect) {
    record('r'); require(rect->right == 640 && rect->bottom == 480);
    ++static_cast<Frame*>(self)->resizes;
    if (emitOnResize) {
        emitOnResize = false;
        auto* pv = static_cast<MoosePlugView*>(view);
        require(pv->vtbl->onSize(pv, rect) == kResultOk);
    }
    return kResultOk;
}

static void off_ui(void (*f)()) { std::thread(f).join(); }

int main(int argc, char** argv) {
    ui = pthread_self();
    Vst3Callbacks callbacks{};
    callbacks.wants_persist_notifications = [](void*) -> bool { return false; };
    callbacks.create = []() -> void* { return &context_token; };
    callbacks.destroy = [](void*) {};
    callbacks.gui_has_editor = [](void*) -> int32_t { return 1; };
    callbacks.gui_close = [](void*) {};
    callbacks.gui_set_size = [](void* ctx, uint32_t, uint32_t) {
        if (workerOnResize) {
            std::thread([ctx] { moose_vst3_end_edit(ctx, 7); }).join();
        } else {
            moose_vst3_begin_edit(ctx, 8);
            moose_vst3_end_edit(ctx, 8);
        }
    };
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

    // A synchronous resize -> onSize -> editor callback must not jump
    // ahead of the rest of the batch already being delivered.
    if (argc == 1 || std::strcmp(argv[1], "reentrant") == 0) {
        calls.clear();
        emitOnResize = true;
        off_ui([] {
            moose_vst3_begin_edit(ctx, 7);
            moose_vst3_request_resize(ctx, 640, 480);
            moose_vst3_perform_edit(ctx, 7, 0.5);
            moose_vst3_end_edit(ctx, 7);
        });
        pump(loop);
        require(calls == "brpebe");
    }

    // A worker that never stops submitting cannot hold the UI thread: one
    // wake makes a bounded number of passes, then asks for another wake.
    if (argc == 1 || std::strcmp(argv[1], "flood") == 0) {
        calls.clear();
        flood = true;
        off_ui([] { moose_vst3_perform_edit(ctx, 7, 0.5); });
        pump(loop);
        require(calls == std::string(kDrainPasses, 'p') && readable(loop.fd));
        flood = false;
        pump(loop);
        require(calls == std::string(kDrainPasses + 1, 'p'));
    }

    // Every UI operation stays behind the remainder of a saturated drain.
    for (char operation : std::string("bper")) {
        calls.clear();
        flood = true;
        off_ui([] { moose_vst3_perform_edit(ctx, 7, 0.5); });
        switch (operation) {
            case 'b': moose_vst3_begin_edit(ctx, 7); break;
            case 'p': moose_vst3_perform_edit(ctx, 7, 0.5); break;
            case 'e': moose_vst3_end_edit(ctx, 7); break;
            case 'r': require(moose_vst3_request_resize(ctx, 640, 480) == 1); break;
        }
        require(calls == std::string(kDrainPasses, 'p') && readable(loop.fd));
        flood = false;
        pump(loop);
        require(calls == std::string(kDrainPasses + 1, 'p') + operation);
    }

    // Enqueue from a worker while the old handler is draining. Its End
    // must finish on that handler, before replacement or clearing it.
    if (argc == 1 || std::strcmp(argv[1], "handler") == 0) {
        Handler next{handler_vtbl, true};
        for (void* target : {static_cast<void*>(&next), static_cast<void*>(nullptr)}) {
            calls.clear();
            emitOnResize = workerOnResize = true;
            off_ui([] {
                moose_vst3_begin_edit(ctx, 7);
                moose_vst3_request_resize(ctx, 640, 480);
                moose_vst3_perform_edit(ctx, 7, 0.5);
            });
            com->impl.setComponentHandler(target);
            require(calls == "brpe");
            if (target) {
                moose_vst3_begin_edit(ctx, 8);
                moose_vst3_end_edit(ctx, 8);
                require(calls == "brpeBE");
            }
            com->impl.setComponentHandler(&handler);
        }
        workerOnResize = false;
    }

    // Force a worker enqueue inside the detach drain. The run-loop
    // ownership marker must remain live until this batch also finishes.
    if (argc == 1 || std::strcmp(argv[1], "detach") == 0) {
        calls.clear();
        workerOnBegin = true;
        off_ui([] {
            moose_vst3_begin_edit(ctx, 7);
            moose_vst3_perform_edit(ctx, 7, 0.5);
        });
        int oldFd = loop.fd;
        require(view->vtbl->setFrame(view, nullptr) == kResultOk);
        require(calls == "bpe" && !loop.handler && loop.timers == 0);
        require(fcntl(oldFd, F_GETFD) == -1 && errno == EBADF);
        workerOnResize = false;
        require(view->vtbl->setFrame(view, &frame) == kResultOk);
    }

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
