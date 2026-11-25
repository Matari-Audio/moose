//! Widget ids bound to moose parameters: a `Ui` gesture becomes the host's
//! begin/perform/end, and whatever the host did to a parameter is the value
//! the next tree reads.
use std::sync::Arc;

use moose_core::editor::{ParamModulation, PluginContext};
use moose_core::main_thread::{MenuTarget, ParamIndicationState};
use moose_params::{ParamFlags, ParamInfo, ParamRange, Params};
use mui::input::{Button, Input, PointerInput};
use mui::layout::Id;
use mui::prelude::Point;
use mui::scene::{El, IntoEl};
use mui::{Edit, Ui};

/// The widget id [`Bridge::bind`] gives parameter `param`: `param/<id>`.
/// Derived, never typed, so the gesture a widget reports is always the one
/// its parameter listens for.
pub fn widget_id(param: impl Into<u32>) -> Id {
    Id::of("param").entity(u64::from(param.into()))
}

/// The editor's side of the plugin: its parameter store, the host's gesture
/// channel while the window is open, and the gestures that channel owes an
/// end to.
pub struct Bridge<P: ?Sized = dyn Params> {
    params: Arc<P>,
    context: Option<PluginContext<P>>,
    infos: Box<[ParamInfo]>,
    meters: Box<[u32]>,
    /// Open gestures: the id, the value the control holds, and the value
    /// last sent. The control reads its own value back, not the store, so a
    /// host echo cannot jitter a drag, and an incremental drag across a
    /// discrete parameter accumulates between the steps it sends.
    open: Vec<(u32, f64, f64)>,
    held: Vec<u32>,
    /// The ids `bind` saw this build: an open gesture not among them lost
    /// its widget, and nothing else would ever end it.
    bound: Vec<u32>,
    /// Widget-id to parameter-id pairs collected during the current build.
    building_widgets: Vec<(Id, u32)>,
    /// Widget-id to parameter-id pairs from the last finished build, used by
    /// `MuiEditor::param_at` (`IParameterFinder`) to map hit surfaces to params.
    bound_widgets: Vec<(Id, u32)>,
    /// Explicit widget-id to context-menu-target overrides registered during
    /// the current build via [`Self::context_menu_target`].
    building_context_targets: Vec<(Id, MenuTarget)>,
    /// Every parameter (value, mono mod, poly min, poly max) and meter as last
    /// seen, bit for bit: `changed` compares against it without hashing or
    /// allocating.
    seen: Box<[u64]>,
    /// Last observed [`PluginContext::param_indication_revision`].
    seen_indication_rev: u64,
    /// Parameter hovered during the build currently in progress.
    building_hovered: Option<u32>,
    /// Last hovered parameter reported to [`PluginContext::param_hovered`].
    last_hovered: Option<u32>,
    /// Context menu target triggered by a secondary click during this frame.
    building_context_menu: Option<(MenuTarget, i32, i32)>,
    /// Whether the secondary pointer button was held on the previous frame.
    prev_secondary_down: bool,
    /// The pointer of the input the build in progress answers.
    pub(crate) pointer: PointerInput,
    /// Scene position where the current secondary button press started.
    secondary_press_pos: Option<Point>,
    /// Resolved menu target under the pointer when the secondary press started.
    secondary_press_target: Option<MenuTarget>,
    /// Current scene-to-logical-point zoom factor used to map scene positions
    /// into host logical editor coordinates for context menus.
    zoom: f64,
    #[cfg(target_os = "linux")]
    native_requests: std::sync::Weak<crate::window::Requests>,
}

/// Number of `u64` snapshot slots per parameter in `Bridge::seen`:
/// normalized value, mono modulation, poly min, poly max.
const PARAM_SEEN_STRIDE: usize = 4;
/// Maximum scene-unit travel for a background secondary click to still open
/// the host's global context menu (avoids triggering on secondary drags).
const CONTEXT_MENU_CLICK_SLOP: f64 = 6.0;

impl<P: Params + ?Sized> Bridge<P> {
    /// Reads the parameter table once; no host until [`Bridge::attach`].
    pub fn new(params: Arc<P>) -> Self {
        let infos = params.param_infos().into_boxed_slice();
        let meters = params.meter_ids().into_boxed_slice();
        let seen =
            vec![u64::MAX; infos.len() * PARAM_SEEN_STRIDE + meters.len()].into_boxed_slice();
        Self {
            params,
            context: None,
            infos,
            meters,
            open: Vec::new(),
            held: Vec::new(),
            bound: Vec::new(),
            building_widgets: Vec::new(),
            bound_widgets: Vec::new(),
            building_context_targets: Vec::new(),
            seen,
            seen_indication_rev: u64::MAX,
            building_hovered: None,
            last_hovered: None,
            building_context_menu: None,
            prev_secondary_down: false,
            pointer: PointerInput::default(),
            secondary_press_pos: None,
            secondary_press_target: None,
            zoom: 1.0,
            #[cfg(target_os = "linux")]
            native_requests: std::sync::Weak::new(),
        }
    }

    /// The typed store, for everything the bridge does not wrap.
    #[must_use]
    pub fn params(&self) -> &Arc<P> {
        &self.params
    }

    /// The host channel, while the editor is open.
    #[must_use]
    pub fn context(&self) -> Option<&PluginContext<P>> {
        self.context.as_ref()
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn set_native_requests(
        &mut self,
        requests: std::sync::Weak<crate::window::Requests>,
    ) {
        self.native_requests = requests;
    }

    /// Ask the host (or native Wayland window on Linux) to resize the editor
    /// to `(width, height)` logical points.
    #[must_use]
    pub fn request_resize(&self, width: u32, height: u32) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(requests) = self.native_requests.upgrade()
            && requests.wayland_open()
        {
            requests.resize(width, height);
            return true;
        }
        self.context
            .as_ref()
            .is_some_and(|c| c.request_resize(width, height))
    }

    /// Start talking to a host. Replacing a context ends the gestures the
    /// old one was owed.
    pub fn attach(&mut self, context: PluginContext<P>) {
        self.close();
        self.context = Some(context);
        self.seen.fill(u64::MAX);
        self.seen_indication_rev = u64::MAX;
    }

    /// End every open gesture and let the host go. Safe to call twice.
    pub fn close(&mut self) {
        if let Some(context) = self.context.take() {
            for (id, ..) in self.open.drain(..) {
                context.end_edit(id);
            }
            if self.last_hovered.take().is_some() {
                context.param_hovered(None);
            }
            context.set_editor_waker(None);
        }
        self.open.clear();
        self.held.clear();
        self.building_context_targets.clear();
        self.building_hovered = None;
        self.last_hovered = None;
        self.building_context_menu = None;
        self.prev_secondary_down = false;
        self.secondary_press_pos = None;
        self.secondary_press_target = None;
        #[cfg(target_os = "linux")]
        {
            self.native_requests = std::sync::Weak::new();
        }
    }

    /// Drop the host without calling host C APIs: for `Drop`, when the host
    /// may have torn its side down already. Still clears the wrapper-internal
    /// Rust editor waker so dropped editor state is never retained or signalled.
    pub fn detach(&mut self) {
        if let Some(context) = self.context.take() {
            context.set_editor_waker(None);
        }
        self.open.clear();
        self.held.clear();
        self.building_context_targets.clear();
        self.building_hovered = None;
        self.last_hovered = None;
        self.building_context_menu = None;
        self.prev_secondary_down = false;
        self.secondary_press_pos = None;
        self.secondary_press_target = None;
        #[cfg(target_os = "linux")]
        {
            self.native_requests = std::sync::Weak::new();
        }
    }

    /// End every open gesture but stay attached: a state load replaced the
    /// values they were editing.
    pub fn end_all(&mut self) {
        if let Some(context) = &self.context {
            for (id, ..) in self.open.drain(..) {
                context.end_edit(id);
            }
        }
        self.held.clear();
    }

    /// Look up the parameter id bound to widget surface key `widget` in the
    /// most recently completed build.
    #[must_use]
    pub fn param_for_widget(&self, widget: &str) -> Option<u32> {
        self.bound_widgets
            .iter()
            .rev()
            .find(|(w, _)| w.as_str() == widget)
            .map(|&(_, id)| id)
    }

    fn info(&self, id: u32) -> Option<&ParamInfo> {
        self.infos.iter().find(|info| info.id == id)
    }

    /// The normalized value the tree should show: the gesture's own while
    /// one is open, the store's otherwise. `0` for an unknown id.
    pub fn value(&self, id: impl Into<u32>) -> f64 {
        let id = id.into();
        self.open
            .iter()
            .find(|(open, ..)| *open == id)
            .map(|&(_, shown, _)| shown)
            .or_else(|| self.params.get_normalized(id))
            .unwrap_or(0.0)
    }

    /// The value as the plugin formats it, unit included.
    pub fn text(&self, id: impl Into<u32>) -> String {
        let id = id.into();
        // ponytail: one String per call; cache per id when a label-heavy
        // editor shows it in a profile.
        self.info(id)
            .and_then(|info| {
                self.params
                    .format_value(id, info.range.denormalize(self.value(id)))
            })
            .unwrap_or_default()
    }

    /// A meter the audio thread published, `0` while closed.
    pub fn meter(&self, id: impl Into<u32>) -> f32 {
        self.context.as_ref().map_or(0.0, |c| c.get_meter(id))
    }

    /// Current host indication state (automation state/colour and mapped
    /// controller metadata) for parameter `id` (`clap.param-indication/4`).
    #[must_use]
    pub fn indication(&self, id: impl Into<u32>) -> ParamIndicationState {
        let id = id.into();
        self.context
            .as_ref()
            .map_or_else(ParamIndicationState::default, |c| c.param_indication(id))
    }

    /// Live monophonic modulation offset of `id` in plain units (`0.0` when
    /// unmodulated or unknown). Reads the parameter store directly, falling
    /// back to the attached host context if reported there.
    #[must_use]
    pub fn modulation(&self, id: impl Into<u32>) -> f64 {
        let id = id.into();
        let store_mod = self.params.get_modulation(id);
        if store_mod != 0.0 {
            return store_mod;
        }
        self.context
            .as_ref()
            .map_or(0.0, |c| c.param_modulation(id).mono)
    }

    /// Combined live modulation state (mono offset plus optional polyphonic
    /// min/max envelope, in plain units) for parameter `id`.
    #[must_use]
    pub fn modulation_state(&self, id: impl Into<u32>) -> ParamModulation {
        let id = id.into();
        let store_mod = self.params.get_modulation(id);
        let host_mod = self.context.as_ref().map(|c| c.param_modulation(id));
        ParamModulation {
            mono: if store_mod == 0.0 {
                host_mod.map_or(0.0, |m| m.mono)
            } else {
                store_mod
            },
            poly_range: host_mod.and_then(|m| m.poly_range),
        }
    }

    /// Effective normalized value (`0.0..=1.0`) of `id` with its live mono
    /// modulation offset applied. Equal to [`Self::value`] when unmodulated.
    #[must_use]
    pub fn modulated_value(&self, id: impl Into<u32>) -> f64 {
        let id = id.into();
        let base = self.value(id);
        let offset = self.modulation(id);
        if offset == 0.0 || !offset.is_finite() {
            return base;
        }
        let Some(info) = self.info(id) else {
            return base;
        };
        let plain = info.range.denormalize(base) + offset;
        info.range.normalize(plain).clamp(0.0, 1.0)
    }

    /// Signed normalized modulation delta (`modulated_value - value`) for
    /// rendering modulation rings or arcs around a control.
    #[must_use]
    pub fn modulation_normalized(&self, id: impl Into<u32>) -> f64 {
        let id = id.into();
        self.modulated_value(id) - self.value(id)
    }

    /// The pointer of the input the build in progress answers: newer than
    /// [`Ui::pointer`], the one the last frame laid out, so a drag can
    /// follow it in this frame rather than the next.
    pub fn pointer(&self) -> PointerInput {
        self.pointer
    }

    /// Set the scene-to-logical zoom factor used to translate pointer
    /// coordinates into host logical editor points for context menus.
    pub fn set_zoom(&mut self, zoom: f64) {
        if zoom.is_finite() && zoom > 0.0 {
            self.zoom = zoom;
        }
    }

    /// Register an explicit host context-menu `target` for `widget` during this
    /// build. Secondary-click resolution runs synchronously in
    /// [`Self::finish_frame`] on the mouse-up frame so custom background or
    /// non-parameter surfaces fire deterministically in 0 Hz idle mode with no
    /// double-dispatch.
    pub fn context_menu_target(&mut self, widget: &Id, target: MenuTarget) {
        self.building_context_targets.push((widget.clone(), target));
    }

    /// Whether any parameter, modulation offset, host indication, or meter
    /// moved since the last call. The window polls this on wake to decide
    /// whether a frame has anything to draw.
    pub fn changed(&mut self) -> bool {
        let mut changed = false;
        let indication_rev = self
            .context
            .as_ref()
            .map_or(0, PluginContext::param_indication_revision);
        if self.seen_indication_rev != indication_rev {
            self.seen_indication_rev = indication_rev;
            changed = true;
        }
        let mut idx = 0;
        for info in &self.infos {
            let val_bits = self
                .params
                .get_normalized(info.id)
                .map_or(u64::MAX - 1, f64::to_bits);
            let mod_state = self.modulation_state(info.id);
            let mono_bits = mod_state.mono.to_bits();
            let (poly_min_bits, poly_max_bits) = match mod_state.poly_range {
                Some((lo, hi)) => (lo.to_bits(), hi.to_bits()),
                None => (u64::MAX - 2, u64::MAX - 2),
            };
            for now in [val_bits, mono_bits, poly_min_bits, poly_max_bits] {
                changed |= self.seen[idx] != now;
                self.seen[idx] = now;
                idx += 1;
            }
        }
        for &id in &self.meters {
            let now = self
                .context
                .as_ref()
                .map_or(0, |c| u64::from(c.get_meter(id).to_bits()));
            changed |= self.seen[idx] != now;
            self.seen[idx] = now;
            idx += 1;
        }
        changed
    }

    /// Build the widget bound to parameter `param`: `control` gets the
    /// widget id ([`widget_id`]) and the normalized value to draw and edit,
    /// and whatever it leaves there goes to the host inside the gesture
    /// `Ui` reported for that id. It returns anything that becomes an `El`:
    /// a widget's whole `Response`, or a tree of its own.
    ///
    /// ```ignore
    /// let gain = bridge.bind(ui, P::Gain, |ui, id, v| knob(ui, id, "Gain", v, 0.0..=1.0));
    /// ```
    ///
    /// A drag is `Begin` .. values .. `End`. A change with no gesture open --
    /// a key step, an accessibility action -- is wrapped in its own
    /// begin/set/end, and the atomic `Begin`/`End` pair `Ui` reports for it
    /// a frame later is dropped. Read-only and unknown parameters draw but
    /// never reach the host. A gesture whose parameter no `bind` of a build
    /// names -- its control dropped out of the tree -- ends with that build.
    ///
    /// One parameter bound twice (a knob and a value field) needs two widget
    /// ids: give the second one its own with [`Bridge::bind_as`].
    pub fn bind<R: IntoEl>(
        &mut self,
        ui: &mut Ui,
        param: impl Into<u32>,
        control: impl FnOnce(&mut Ui, Id, &mut f64) -> R,
    ) -> El {
        let id = param.into();
        self.bind_as(ui, id, widget_id(id), control)
    }

    /// [`Bridge::bind`] under the widget id `widget` instead of
    /// [`widget_id`]`(param)`, for a second control on the same parameter:
    /// two widgets under one id are a duplicate key, and the tree stops
    /// resolving. Gestures still bracket by parameter, whichever control
    /// made them.
    ///
    /// ```ignore
    /// let knob = bridge.bind(ui, P::Gain, |ui, id, v| knob(ui, id, "Gain", v, 0.0..=1.0));
    /// let field = widget_id(P::Gain).field("value");
    /// let entry = bridge.bind_as(ui, P::Gain, field, |ui, id, v| number(ui, id, v));
    /// ```
    pub fn bind_as<R: IntoEl>(
        &mut self,
        ui: &mut Ui,
        param: impl Into<u32>,
        widget: Id,
        control: impl FnOnce(&mut Ui, Id, &mut f64) -> R,
    ) -> El {
        let id = param.into();
        self.bound.push(id);
        let (known, range) = match self.info(id) {
            Some(info) => (
                true,
                (!info.flags.contains(ParamFlags::READONLY)).then_some(info.range),
            ),
            None => (false, None),
        };
        if known {
            self.building_widgets.push((widget.clone(), id));
            if ui.get(&widget).hovered {
                self.building_hovered = Some(id);
            }
        }
        let before = self.value(id);
        let mut value = before;
        let Some(range) = range else {
            return control(ui, widget, &mut value).into_el();
        };
        // At most a cancel's End, an End and a Begin reach one id per frame.
        let mut edges = [None; 4];
        for (slot, edit) in edges.iter_mut().zip(ui.edits_for(&widget)) {
            *slot = Some(edit);
        }
        let atomic = edges == [Some(Edit::Begin), Some(Edit::End), None, None] && !self.is_open(id);
        let last = |e| edges.iter().rposition(|x| *x == Some(e));
        // An End after the last Begin closes the gesture after this frame's
        // value, so the value lands inside it.
        let trailing = match (last(Edit::End), last(Edit::Begin)) {
            (Some(end), Some(begin)) if end > begin => Some(end),
            (Some(end), None) => Some(end),
            _ => None,
        };
        if !atomic {
            for (i, edge) in edges.iter().enumerate() {
                match edge {
                    Some(Edit::Begin) => self.begin(id),
                    Some(Edit::End) if Some(i) != trailing => self.end(id),
                    _ => {}
                }
            }
        }
        let el = control(ui, widget, &mut value).into_el();
        if value.to_bits() != before.to_bits() && value.is_finite() {
            if self.is_open(id) {
                self.set(id, range, value);
            } else if quantize(range, value).to_bits() != quantize(range, before).to_bits() {
                // ponytail: a key step smaller than a discrete parameter's
                // step is dropped here; widgets step by a hundredth of the
                // range, so stepping a 1..8 switch needs its own step size.
                self.begin(id);
                self.set(id, range, value);
                self.end(id);
            }
        }
        if !atomic && trailing.is_some() {
            self.end(id);
        }
        el
    }

    /// [`Bridge::bind`] for a switch: `control` edits a `bool`, sent as
    /// normalized 0 or 1.
    ///
    /// ```ignore
    /// let bypass = bridge.bind_bool(ui, P::Bypass, |ui, id, on| toggle(ui, id, "Bypass", on));
    /// ```
    pub fn bind_bool<R: IntoEl>(
        &mut self,
        ui: &mut Ui,
        param: impl Into<u32>,
        control: impl FnOnce(&mut Ui, Id, &mut bool) -> R,
    ) -> El {
        self.bind(ui, param, |ui, id, v| {
            let mut on = *v >= 0.5;
            let el = control(ui, id, &mut on).into_el();
            *v = f64::from(u8::from(on));
            el
        })
    }

    /// Keep a wheel or other pointer-free gesture open while it is rolling.
    /// Releasing a hold does not end a gesture opened by a pointer control.
    pub fn hold(&mut self, param: impl Into<u32>, rolling: bool) {
        let id = param.into();
        if rolling {
            if !self.is_open(id)
                && self
                    .info(id)
                    .is_some_and(|info| !info.flags.contains(ParamFlags::READONLY))
            {
                self.begin(id);
                if self.is_open(id) {
                    self.held.push(id);
                }
            }
        } else if self.held.contains(&id) {
            self.end(id);
        }
    }

    /// Send a canvas or plot drag in the parameter's plain units.
    /// Call on each build during the drag; an unbound build ends it.
    pub fn drag_gesture(&mut self, param: impl Into<u32>, plain: f64) {
        let id = param.into();
        self.bound.push(id);
        let Some(range) = self
            .info(id)
            .filter(|info| !info.flags.contains(ParamFlags::READONLY))
            .map(|info| info.range)
        else {
            return;
        };
        if plain.is_finite() {
            self.begin(id);
            self.set(id, range, range.normalize(plain));
        }
    }

    /// Resolve which [`MenuTarget`] is topmost at scene position `pos`,
    /// preferring explicit [`Self::context_menu_target`] registrations and
    /// bound parameter controls over [`MenuTarget::Global`].
    fn resolve_context_target(&self, ui: &Ui, pos: Point) -> MenuTarget {
        let Some(scene) = ui.scene() else {
            return MenuTarget::Global;
        };
        for surface in scene.surfaces().rev() {
            if surface.disabled {
                continue;
            }
            if let Some(clip) = surface.clip
                && (pos.x < clip.x0 || pos.x > clip.x1 || pos.y < clip.y0 || pos.y > clip.y1)
            {
                continue;
            }
            let local = surface.local(pos);
            if local.x < 0.0
                || local.y < 0.0
                || local.x > surface.frame.size.width
                || local.y > surface.frame.size.height
            {
                continue;
            }
            let mut key = Some(surface.key.as_str());
            while let Some(k) = key {
                if let Some(&(_, target)) = self
                    .building_context_targets
                    .iter()
                    .rev()
                    .find(|(w, _)| w.as_str() == k)
                {
                    return target;
                }
                if let Some(&(_, id)) = self
                    .building_widgets
                    .iter()
                    .rev()
                    .find(|(w, _)| w.as_str() == k)
                {
                    return MenuTarget::Param(id);
                }
                key = scene.surface(k).and_then(|n| n.parent.as_deref());
            }
        }
        MenuTarget::Global
    }

    /// After a build with full frame input: detects secondary-button clicks
    /// synchronously on the release frame across bound parameter controls,
    /// explicit [`Self::context_menu_target`] surfaces, and background space
    /// (`MenuTarget::Global`), then runs [`Self::end_unbound`] to close
    /// orphaned gestures and flush hover / context-menu notifications.
    pub fn finish_frame(&mut self, ui: &Ui, input: &Input) {
        let secondary_down = input.pointer.buttons.contains(Button::Secondary);
        let pos = input.pointer.pos.or_else(|| ui.pointer().pos);
        if secondary_down && !self.prev_secondary_down {
            self.secondary_press_pos = pos;
            self.secondary_press_target = pos.map(|p| self.resolve_context_target(ui, p));
        } else if !secondary_down && self.prev_secondary_down {
            let press_pos = self.secondary_press_pos.take();
            let press_target = self.secondary_press_target.take();
            if self.building_context_menu.is_none()
                && let Some(pos) = pos
            {
                let within_slop = press_pos.is_none_or(|start| {
                    let dx = pos.x - start.x;
                    let dy = pos.y - start.y;
                    (dx * dx + dy * dy) <= CONTEXT_MENU_CLICK_SLOP * CONTEXT_MENU_CLICK_SLOP
                });
                let release_target = self.resolve_context_target(ui, pos);
                if within_slop && press_target.is_none_or(|t| t == release_target) {
                    let (x, y) = logical_pointer(Some(pos), self.zoom);
                    self.building_context_menu = Some((release_target, x, y));
                }
            }
        }
        self.prev_secondary_down = secondary_down;
        self.end_unbound();
    }

    /// After a build: end the gestures no `bind` in it named, and flush any
    /// parameter hover transitions or right-click context menu requests.
    /// `MuiEditor` calls [`Self::finish_frame`]; a host that drives the bridge
    /// itself calls this (or `finish_frame`) after every build.
    pub fn end_unbound(&mut self) {
        while let Some(&(id, ..)) = self.open.iter().find(|(id, ..)| !self.bound.contains(id)) {
            self.end(id);
        }
        self.bound.clear();
        std::mem::swap(&mut self.bound_widgets, &mut self.building_widgets);
        self.building_widgets.clear();
        self.building_context_targets.clear();

        let next_hovered = self.building_hovered.take();
        if next_hovered != self.last_hovered {
            self.last_hovered = next_hovered;
            if let Some(context) = &self.context {
                context.param_hovered(next_hovered);
            }
        }
        if let Some((target, x, y)) = self.building_context_menu.take()
            && let Some(context) = &self.context
        {
            let _ = context.context_menu(target, x, y);
        }
    }

    fn is_open(&self, id: u32) -> bool {
        self.open.iter().any(|(open, ..)| *open == id)
    }

    fn begin(&mut self, id: u32) {
        let Some(context) = &self.context else {
            return;
        };
        if !self.is_open(id) {
            let value = self.params.get_normalized(id).unwrap_or(0.0);
            context.begin_edit(id);
            self.open.push((id, value, value));
        }
    }

    fn set(&mut self, id: u32, range: ParamRange, value: f64) {
        let (Some(context), Some((_, shown, sent))) = (
            &self.context,
            self.open.iter_mut().find(|(open, ..)| *open == id),
        ) else {
            return;
        };
        *shown = value.clamp(0.0, 1.0);
        let value = quantize(range, *shown);
        if sent.to_bits() != value.to_bits() {
            *sent = value;
            context.set_param(id, value);
        }
    }

    fn end(&mut self, id: u32) {
        self.held.retain(|held| *held != id);
        if let (Some(context), Some(i)) = (
            &self.context,
            self.open.iter().position(|(open, ..)| *open == id),
        ) {
            self.open.swap_remove(i);
            context.end_edit(id);
        }
    }
}

/// Moose's declared range quantizes: a discrete parameter snaps.
fn quantize(range: ParamRange, normalized: f64) -> f64 {
    range.normalize(range.denormalize(normalized.clamp(0.0, 1.0)))
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "editor coordinates are bounded logical window points"
)]
fn logical_pointer(pos: Option<Point>, zoom: f64) -> (i32, i32) {
    let Some(pos) = pos else {
        return (0, 0);
    };
    let z = if zoom.is_finite() && zoom > 0.0 {
        zoom
    } else {
        1.0
    };
    ((pos.x * z).round() as i32, (pos.y * z).round() as i32)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use moose_core::editor::{EditorBridge, EditorFeedback, HostContext, PluginContext};
    use moose_core::events::TransportInfo;
    use moose_core::main_thread::{
        AutomationState, HostColor, HostMapping, MenuTarget, ParamIndication,
    };
    use moose_params::{DynamicParams, Params, SlotDef, SlotRange};
    use mui::input::{Button, Buttons, Input, PointerInput};
    use mui::prelude::*;

    use super::*;

    struct MockHost {
        params: Arc<DynamicParams<2>>,
        feedback: Arc<EditorFeedback>,
        hovered: Mutex<Vec<Option<u32>>>,
        menus: Mutex<Vec<(MenuTarget, i32, i32)>>,
    }

    impl MockHost {
        fn new(params: Arc<DynamicParams<2>>) -> Arc<Self> {
            let ids = params.param_infos().into_iter().map(|i| i.id);
            Arc::new(Self {
                params,
                feedback: Arc::new(EditorFeedback::new(ids)),
                hovered: Mutex::new(Vec::new()),
                menus: Mutex::new(Vec::new()),
            })
        }
    }

    impl EditorBridge for MockHost {
        fn begin_edit(&self, _id: u32) {}
        fn set_param(&self, id: u32, normalized: f64) {
            self.params.set_normalized(id, normalized);
        }
        fn end_edit(&self, _id: u32) {}
        fn request_resize(&self, _w: u32, _h: u32) -> bool {
            true
        }
        fn get_param(&self, id: u32) -> f64 {
            self.params.get_normalized(id).unwrap_or(0.0)
        }
        fn get_param_plain(&self, id: u32) -> f64 {
            self.params.get_plain(id).unwrap_or(0.0)
        }
        fn format_param(&self, _id: u32) -> String {
            String::new()
        }
        fn get_meter(&self, _id: u32) -> f32 {
            0.0
        }
        fn get_state(&self) -> Vec<u8> {
            Vec::new()
        }
        fn set_state(&self, _data: Vec<u8>) {}
        fn transport(&self) -> Option<TransportInfo> {
            None
        }
    }

    impl HostContext for MockHost {
        fn param_hovered(&self, id: Option<u32>) {
            self.hovered.lock().unwrap().push(id);
        }

        fn context_menu(&self, target: MenuTarget, x: i32, y: i32) -> bool {
            self.menus.lock().unwrap().push((target, x, y));
            true
        }

        fn param_indication(&self, id: u32) -> ParamIndicationState {
            self.feedback.indication(id)
        }

        fn param_indication_revision(&self) -> u64 {
            self.feedback.indication_revision()
        }

        fn param_modulation(&self, id: u32) -> ParamModulation {
            self.feedback.modulation(id, self.params.get_modulation(id))
        }

        fn set_editor_waker(&self, waker: Option<Arc<dyn Fn() + Send + Sync>>) {
            self.feedback.set_waker(waker);
        }
    }

    fn make_params() -> Arc<DynamicParams<2>> {
        let params = Arc::new(DynamicParams::<2>::new());
        let s0 = params.slot(0).unwrap();
        let s1 = params.slot(1).unwrap();
        params.edit(|e| {
            e.bind(
                s0,
                SlotDef::new(
                    "Gain",
                    SlotRange::Linear {
                        min: 0.0,
                        max: 10.0,
                    },
                )
                .with_default(5.0),
            );
            e.bind(
                s1,
                SlotDef::new("Mix", SlotRange::Linear { min: 0.0, max: 1.0 }).with_default(0.25),
            );
        });
        params
    }

    fn pointer_at(x: f64, y: f64, secondary: bool) -> Input {
        let mut buttons = Buttons::default();
        if secondary {
            buttons = buttons.set(Button::Secondary, true);
        }
        Input::from(PointerInput {
            pos: Some(Point::new(x, y)),
            buttons,
            ..PointerInput::default()
        })
    }

    #[test]
    fn indication_and_modulation_propagate_and_trigger_changed() {
        let params = make_params();
        let host = MockHost::new(Arc::clone(&params));
        let mut bridge = Bridge::new(Arc::clone(&params));
        let ctx = PluginContext::from_host(
            Arc::clone(&host) as Arc<dyn HostContext>,
            Arc::clone(&params),
        );
        bridge.attach(ctx);

        // First `changed()` seeds the snapshot.
        assert!(bridge.changed());
        assert!(!bridge.changed(), "settled snapshot reports unchanged");

        // Host automation indication + mapping propagation.
        let red = HostColor {
            r: 255,
            g: 32,
            b: 32,
            a: 255,
        };
        assert!(host.feedback.set_indication(
            0,
            &ParamIndication::Automation {
                state: AutomationState::Recording,
                color: Some(red),
            }
        ));
        assert!(bridge.changed(), "indication change marks bridge dirty");
        assert!(!bridge.changed());

        assert!(host.feedback.set_indication(
            0,
            &ParamIndication::Mapping(Some(HostMapping {
                color: Some(red),
                label: "Macro 1".into(),
                description: "Host Macro".into(),
            }))
        ));
        let ind = bridge.indication(0u32);
        assert_eq!(ind.automation, AutomationState::Recording);
        assert_eq!(ind.automation_color, Some(red));
        assert_eq!(
            ind.mapping.as_ref().map(|m| m.label.as_str()),
            Some("Macro 1")
        );
        assert!(bridge.changed());
        assert!(!bridge.changed());

        // Mono modulation offset (+2.5 on 0..10 range => +0.25 normalized).
        params.set_modulation(0, 2.5);
        assert!((bridge.modulation(0u32) - 2.5).abs() < 1e-9);
        assert!((bridge.value(0u32) - 0.5).abs() < 1e-9);
        assert!((bridge.modulated_value(0u32) - 0.75).abs() < 1e-9);
        assert!((bridge.modulation_normalized(0u32) - 0.25).abs() < 1e-9);
        assert!(bridge.changed(), "mono modulation marks bridge dirty");
        assert!(!bridge.changed());

        // Polyphonic modulation range summary.
        host.feedback.record_poly_mod(0, -1.0);
        host.feedback.record_poly_mod(0, 3.0);
        let mod_state = bridge.modulation_state(0u32);
        assert_eq!(mod_state.poly_range, Some((-1.0, 3.0)));
        assert!(bridge.changed(), "poly modulation marks bridge dirty");
        assert!(!bridge.changed());
    }

    #[test]
    fn coalesced_host_waker_fires_once_until_rearmed() {
        let params = make_params();
        let host = MockHost::new(Arc::clone(&params));
        let wakes = Arc::new(AtomicUsize::new(0));
        let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        host.feedback.set_waker(Some(Arc::new({
            let wakes = Arc::clone(&wakes);
            let armed = Arc::clone(&armed);
            move || {
                if !armed.swap(true, Ordering::AcqRel) {
                    wakes.fetch_add(1, Ordering::Relaxed);
                }
            }
        })));

        host.feedback.set_indication(
            0,
            &ParamIndication::Automation {
                state: AutomationState::Playing,
                color: None,
            },
        );
        host.feedback.set_indication(
            1,
            &ParamIndication::Automation {
                state: AutomationState::Recording,
                color: None,
            },
        );
        assert_eq!(
            wakes.load(Ordering::Relaxed),
            1,
            "burst coalesces to one wake"
        );

        // Simulating a frame tick re-arms the coalescer.
        armed.store(false, Ordering::Release);
        host.feedback.wake_editor();
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn hover_transitions_and_right_click_context_menus_dispatch_cleanly() {
        let params = make_params();
        let host = MockHost::new(Arc::clone(&params));
        let mut bridge = Bridge::new(Arc::clone(&params));
        let ctx = PluginContext::from_host(
            Arc::clone(&host) as Arc<dyn HostContext>,
            Arc::clone(&params),
        );
        bridge.attach(ctx);

        let mut ui = Ui::default();
        let run_frame = |ui: &mut Ui, bridge: &mut Bridge<DynamicParams<2>>, input: Input| {
            let root = col![
                bridge.bind(ui, 0u32, |_ui, id, _v| block(40.0, 40.0)
                    .fill(Role::Raised)
                    .id(id)),
                block(40.0, 40.0).fill(Role::Surface).id("bg"),
            ]
            .w(100.0)
            .h(100.0);
            bridge.finish_frame(ui, &input);
            ui.frame(root, Some(Size::new(100.0, 100.0)), input, 0.016)
                .unwrap();
        };

        // Frame 1: seed geometry with pointer outside.
        run_frame(&mut ui, &mut bridge, pointer_at(90.0, 90.0, false));
        assert!(host.hovered.lock().unwrap().is_empty());

        // Frame 2 + 3: hover param 0 (centered at x=30..70, y=0..40 inside 100x100 col).
        run_frame(&mut ui, &mut bridge, pointer_at(50.0, 15.0, false));
        run_frame(&mut ui, &mut bridge, pointer_at(50.0, 15.0, false));
        assert_eq!(*host.hovered.lock().unwrap(), vec![Some(0)]);

        // Frame 4: staying on param 0 does NOT emit duplicate hover events.
        run_frame(&mut ui, &mut bridge, pointer_at(52.0, 18.0, false));
        assert_eq!(*host.hovered.lock().unwrap(), vec![Some(0)]);

        // Right-click on param 0: fires synchronously on the release frame
        // (critical for 0 Hz idle mode where no follow-up frame is pumped).
        run_frame(&mut ui, &mut bridge, pointer_at(52.0, 18.0, true));
        run_frame(&mut ui, &mut bridge, pointer_at(52.0, 18.0, false));
        assert_eq!(
            *host.menus.lock().unwrap(),
            vec![(MenuTarget::Param(0), 52, 18)]
        );

        // Move off param 0 to background => hover transitions to None.
        run_frame(&mut ui, &mut bridge, pointer_at(80.0, 80.0, false));
        run_frame(&mut ui, &mut bridge, pointer_at(80.0, 80.0, false));
        assert_eq!(*host.hovered.lock().unwrap(), vec![Some(0), None]);

        // Right-click on background => MenuTarget::Global.
        run_frame(&mut ui, &mut bridge, pointer_at(80.0, 80.0, true));
        run_frame(&mut ui, &mut bridge, pointer_at(80.0, 80.0, false));
        assert_eq!(
            *host.menus.lock().unwrap(),
            vec![(MenuTarget::Param(0), 52, 18), (MenuTarget::Global, 80, 80)]
        );
    }

    #[test]
    fn context_menu_target_fires_once_on_release_frame_and_detach_clears_waker() {
        let params = make_params();
        let host = MockHost::new(Arc::clone(&params));
        let mut bridge = Bridge::new(Arc::clone(&params));
        let ctx = PluginContext::from_host(
            Arc::clone(&host) as Arc<dyn HostContext>,
            Arc::clone(&params),
        );
        ctx.set_editor_waker(Some(Arc::new(|| {})));
        assert!(host.feedback.has_waker());
        bridge.attach(ctx);

        let mut ui = Ui::default();
        let bg_id = Id::of("custom_bg");
        let run_frame = |ui: &mut Ui, bridge: &mut Bridge<DynamicParams<2>>, input: Input| {
            bridge.context_menu_target(&bg_id, MenuTarget::Param(1));
            let root = col![
                bridge.bind(ui, 0u32, |_ui, id, _v| block(40.0, 40.0)
                    .fill(Role::Raised)
                    .id(id)),
                block(40.0, 40.0).fill(Role::Surface).id(bg_id.clone()),
            ]
            .w(100.0)
            .h(100.0);
            bridge.finish_frame(ui, &input);
            ui.frame(root, Some(Size::new(100.0, 100.0)), input, 0.016)
                .unwrap();
        };

        // Frame 1: seed geometry.
        run_frame(&mut ui, &mut bridge, pointer_at(90.0, 90.0, false));

        // Right-click on custom_bg (centered at x=30..70, y=40..80):
        // must fire ONCE on the release frame with MenuTarget::Param(1),
        // never double-firing with MenuTarget::Global.
        run_frame(&mut ui, &mut bridge, pointer_at(50.0, 55.0, true));
        run_frame(&mut ui, &mut bridge, pointer_at(50.0, 55.0, false));
        assert_eq!(
            *host.menus.lock().unwrap(),
            vec![(MenuTarget::Param(1), 50, 55)]
        );

        // Detach must still clear the wrapper-internal Rust waker.
        bridge.detach();
        assert!(!host.feedback.has_waker());
    }
}
