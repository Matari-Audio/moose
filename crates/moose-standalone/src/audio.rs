//! Shared cpal audio setup + callback. One implementation used by
//! both `windowed` and `headless` runners.
//!
//! Both the input and output cpal streams are owned by dedicated
//! worker threads (cpal `Stream` is `!Send` on macOS, so it can't
//! cross threads). UI / menu / CLI callers manipulate them through
//! `Send + Sync` controllers (`InputController`, `OutputController`)
//! that talk to the workers via `mpsc` channels:
//!
//! - **Toggle / enable** for input (drop the cpal input stream when
//!   off - saves CPU and skips the OS mic permission prompt) and
//!   output (mute - keep the stream open so processing keeps ticking,
//!   just zero-fill the speaker buffer).
//! - **Switch device** for either side. Worker drops the old stream
//!   and opens a new one against the requested device name; on
//!   failure the previous device's name remains in place and the
//!   audio callback keeps running unchanged.

use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use moose_core::buffer::RawBufferScratch;
use moose_core::bus_routing::{BusActivation, BusRouting, bus_layout_fits_routing};
use moose_core::cast::sample_count_usize;
use moose_core::chunked_process::{ChunkedProcess, process_chunked_with_bus_routing};
use moose_core::config::{AudioConfig, ProcessMode};
use moose_core::events::{EVENT_LIST_PREALLOC, Event, EventBody, EventList, OutputEventStatus};
use moose_core::export::PluginExport;
use moose_core::tasks::InstanceContext;
use moose_params::{ParamInfo, Params};

use crate::cli::Options;

/// GAP-1 identity of the standalone runner (it is its own host).
fn standalone_host() -> moose_core::info::HostIdentity {
    moose_core::info::HostIdentity::new(moose_core::info::HostFormat::Standalone)
        .with_name("moose-standalone")
        .with_version(env!("CARGO_PKG_VERSION"))
}
use crate::transport::Transport;
use crate::vlog;

type BoxErr = Box<dyn std::error::Error>;

/// Per-stream audio-callback scratch for the channel-pointer arrays
/// fed into [`RawBufferScratch::build`]. cpal's stream closure must
/// be `Send + 'static`, but `Vec<*const f32>` / `Vec<*mut f32>` are
/// `!Send` because raw pointers don't carry thread-safety. The cpal
/// closure runs on a single dedicated audio thread per stream and the
/// pointer values are written + consumed within one callback (they
/// always alias `input_bufs` / `channel_bufs` on the same stack
/// frame), so the closure capture is sound.
struct CallbackPtrScratch {
    inputs: Vec<*const f32>,
    outputs: Vec<*mut f32>,
}
// SAFETY: see [`CallbackPtrScratch`] doc.
unsafe impl Send for CallbackPtrScratch {}

impl CallbackPtrScratch {
    /// Point the arrays at this block's lanes. Allocation-free within the
    /// capacities reserved at stream setup.
    fn fill(&mut self, inputs: &[Vec<f32>], outputs: &mut [Vec<f32>]) {
        self.inputs.clear();
        self.outputs.clear();
        self.inputs.extend(inputs.iter().map(Vec::as_ptr));
        self.outputs.extend(outputs.iter_mut().map(Vec::as_mut_ptr));
    }
}

/// Widest interleaved mic-input frame the ring carries. The input
/// callback normalizes the capture device's frames to the ring width
/// (the output stream's channel count); this bounds the fixed-size
/// frame so the ring is a lock-free, alloc-free `ArrayQueue`. A wider
/// output folds its extra channels onto these.
const MAX_INPUT_RING_CHANNELS: usize = 16;

/// One interleaved mic-input frame, fixed-width so the ring never
/// allocates. Only the first `width` lanes are meaningful (the ring
/// width; see [`normalize_input_frame`]).
#[derive(Clone, Copy)]
struct InputFrame {
    samples: [f32; MAX_INPUT_RING_CHANNELS],
}

/// Lock-free hand-off from the input (capture) audio thread to the
/// output (render) audio thread. Bounded and drop-oldest on overflow -
/// see [`build_and_play_input_stream`]. Frame-granular so an overflow
/// drop never splits an interleaved frame (which would shift channel
/// alignment).
type InputRing = ArrayQueue<InputFrame>;

/// Normalize one native capture frame (`src`, `src.len()` interleaved
/// channels) to a ring frame of `width` channels. A mono source
/// broadcasts to every lane (so a mono mic feeds both plugin inputs); a
/// wider source is truncated, a narrower one zero-fills past its
/// channels. Pure arithmetic - safe on the audio thread.
fn normalize_input_frame(src: &[f32], width: usize) -> InputFrame {
    let mut frame = InputFrame {
        samples: [0.0; MAX_INPUT_RING_CHANNELS],
    };
    let w = width.min(MAX_INPUT_RING_CHANNELS);
    if src.len() == 1 {
        for lane in &mut frame.samples[..w] {
            *lane = src[0];
        }
    } else {
        for (lane, &s) in frame.samples[..w].iter_mut().zip(src.iter()) {
            *lane = s;
        }
    }
    frame
}

/// A queued MIDI event the UI thread hands off to the audio callback.
pub struct MidiEvent {
    pub body: EventBody,
    /// Plugin MIDI input port this event targets. Device input stamps
    /// the port its `--midi-input` slot maps to; the QWERTY keyboard
    /// uses `0`.
    pub port: u8,
}

/// Shared audio-thread resources handed back from `start_audio`.
///
/// The output stream is owned by a worker thread, not by this
/// struct, so `AudioHandles` is fully `Send`. The worker exits
/// when the controller channel closes (all `OutputController`
/// clones dropped).
pub struct AudioHandles<P: PluginExport> {
    /// Event queue the caller pushes MIDI into; drained by the audio
    /// callback each block.
    pub pending: Arc<ArrayQueue<MidiEvent>>,
    /// Editor-initiated custom-state (`load_state`) blobs. The UI thread
    /// pushes here instead of locking the plugin, and the audio callback
    /// drains and applies one at the block top under its per-block lock -
    /// so a slow `load_state` never stalls the callback's `try_lock` (the
    /// VST3 `StateLoadQueue` pattern; capacity 1, newest wins). Custom
    /// state only; params restore atomically through `set_param`.
    pub pending_state: Arc<ArrayQueue<Vec<u8>>>,
    /// Plugin instance shared between caller and audio callback.
    pub plugin: Arc<Mutex<P>>,
    /// Audio config (sample rate, channels) resolved from the device.
    pub sample_rate: f64,
    pub channels: usize,
    pub is_effect: bool,
    /// `Send + Sync` handle for toggling mic input and switching
    /// the input device. Backed by a worker thread that owns the
    /// (`!Send`) cpal input stream.
    pub input: InputController,
    /// `Send + Sync` handle for switching the output device.
    /// Backed by a worker thread that owns the (`!Send`) cpal
    /// output stream.
    pub output: OutputController,
    /// Shared transport state; UI thread toggles play/stop, audio
    /// thread advances position each block.
    pub transport: Transport,
    /// Live-mode `--input-file` source (gated on the `playback`
    /// feature). Exposed so the runner can poll
    /// `playback.is_eof()` to drive clean shutdown when paired
    /// with `--output-file`.
    #[cfg(feature = "playback")]
    pub playback: Option<Arc<crate::playback::PlaybackSource>>,
    /// Live-mode `--sidechain-file` source (gated on the `playback`
    /// feature). Feeds the sidechain input bus independently of
    /// `playback`, so the main and sidechain buses run separate files.
    #[cfg(feature = "playback")]
    pub sidechain_playback: Option<Arc<crate::playback::PlaybackSource>>,
    /// Live-mode `--output-file` capture sink. The runner calls
    /// `take_capture().finalize()` on its way out so the WAV
    /// header gets the correct sample count.
    #[cfg(feature = "playback")]
    pub capture: Option<crate::playback::CaptureSink>,
    /// Unregisters these controllers from [`reopen_audio`] on drop.
    _live: LiveAudio,
}

// ---------------------------------------------------------------------------
// Audio API, open results, sessions and live reconfiguration
// ---------------------------------------------------------------------------

/// Highest 1-based device channel a [`ChannelRoute`] may name. Bounds the
/// packed encoding (so it can't overflow) far above any real interface.
pub const MAX_ROUTE_CHANNEL: usize = 4096;
/// Highest `--sample-rate` accepted.
const MAX_SAMPLE_RATE: u32 = 1_536_000;
/// Highest `--buffer` accepted (matches the largest advertised device
/// maximum the preparation contract trusts).
const MAX_BUFFER_FRAMES: u32 = 32_768;

const WORKER_GONE: &str = "the standalone audio worker has stopped";

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Resolve an audio API (cpal host) by case-insensitive name, e.g. `alsa`,
/// `jack`, `coreaudio`, `wasapi`, `asio`. `None` = the platform default.
///
/// # Errors
///
/// Names the compiled-in APIs when `name` matches none (JACK needs the
/// `jack` feature, ASIO cpal's `asio` feature).
fn parse_audio_api(name: Option<&str>) -> Result<Option<cpal::HostId>, String> {
    let Some(name) = name else {
        return Ok(None);
    };
    let hosts = cpal::available_hosts();
    hosts
        .iter()
        .copied()
        .find(|id| id.name().eq_ignore_ascii_case(name.trim()))
        .map(Some)
        .ok_or_else(|| {
            let names: Vec<_> = hosts.iter().map(cpal::HostId::name).collect();
            format!(
                "unknown audio API '{name}'; available: {}",
                names.join(", ")
            )
        })
}

/// Open the cpal host for `api`. An unavailable API (e.g. a stopped JACK
/// server) is an error, so a switch onto it fails and is rolled back.
/// The documented name of an audio API. cpal names JACK `"Jack"`; embedders
/// match the conventional `"JACK"`.
fn api_name(id: cpal::HostId) -> &'static str {
    let name = id.name();
    if name.eq_ignore_ascii_case("jack") {
        "JACK"
    } else {
        name
    }
}

fn open_host(api: Option<cpal::HostId>) -> Result<cpal::Host, String> {
    match api {
        None => Ok(cpal::default_host()),
        Some(id) => cpal::host_from_id(id)
            .map_err(|e| format!("audio API {} is unavailable: {e}", id.name())),
    }
}

/// The configuration a standalone audio open committed, reported through
/// [`on_audio_opened`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AudioOpened {
    /// Audio API name (`"ALSA"`, `"JACK"`, `"CoreAudio"`, ...).
    pub api: &'static str,
    /// Output device name, when the backend reports one.
    pub output_device: Option<String>,
    /// Stream sample rate in Hz.
    pub sample_rate: u32,
    /// Fixed period in frames; `None` = the device default.
    pub buffer_size: Option<u32>,
    /// Device output stream width.
    pub output_channels: u16,
    /// Capture stream width; `None` = input closed.
    pub input_channels: Option<u16>,
    /// The plugin's active bus-layout index (into `bus_layouts()`).
    pub bus_layout: usize,
}

type AudioOpenedFn = dyn Fn(Result<AudioOpened, String>) + Send + Sync;

static AUDIO_OPENED: Mutex<Option<Arc<AudioOpenedFn>>> = Mutex::new(None);

/// Register `f` to receive exactly one terminal result per audio request:
/// the launch (`start_audio`, including startup failures), every controller
/// or menu change, and every [`reopen_audio`]. `Ok` carries the complete
/// configuration that was committed. `Err` means nothing from the request
/// was applied: the previous configuration was restored, and the message
/// says if restoring failed too. An explicitly requested input that can't
/// open fails its request. Called on the audio worker thread; a panic in
/// `f` is caught there. Replaces any earlier callback.
pub fn on_audio_opened(f: impl Fn(Result<AudioOpened, String>) + Send + Sync + 'static) {
    *lock(&AUDIO_OPENED) = Some(Arc::new(f));
}

fn notify_audio_opened(result: Result<AudioOpened, String>) {
    let f = lock(&AUDIO_OPENED).clone();
    if let Some(f) = f {
        // A panicking observer must not unwind (and kill) the audio worker.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(result))).is_err() {
            eprintln!("moose-standalone: the on_audio_opened callback panicked");
        }
    }
}

/// Audio arguments validated before anything is applied. Audio API and
/// device names always apply (`None` = the default); every other `None`
/// keeps the running value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct AudioPatch {
    api: Option<cpal::HostId>,
    output_device: Option<String>,
    input_device: Option<String>,
    sample_rate: Option<u32>,
    buffer_size: Option<u32>,
    layout: Option<usize>,
    output_route: Option<ChannelRoute>,
    input_route: Option<ChannelRoute>,
    output_enabled: Option<bool>,
    input_enabled: Option<bool>,
}

impl AudioPatch {
    fn from_options(opts: &Options) -> Result<Self, String> {
        let route = |spec: Option<&str>, flag: &str| {
            spec.map(|s| {
                ChannelRoute::parse(s).ok_or_else(|| {
                    format!(
                        "{flag}: invalid '{s}' (expected 'direct', a channel 1..={MAX_ROUTE_CHANNEL} \
                         like '3', or a pair like '3-4')"
                    )
                })
            })
            .transpose()
        };
        let bounded = |value: Option<u32>, flag: &str, max: u32| match value {
            Some(n) if n == 0 || n > max => Err(format!("{flag}: {n} is outside 1..={max}")),
            value => Ok(value),
        };
        Ok(Self {
            api: parse_audio_api(opts.audio_api.as_deref())?,
            output_device: opts.output_device.clone(),
            input_device: opts.input_device.clone(),
            sample_rate: bounded(opts.sample_rate, "--sample-rate", MAX_SAMPLE_RATE)?,
            buffer_size: bounded(opts.buffer_size, "--buffer", MAX_BUFFER_FRAMES)?,
            layout: opts.bus_layout,
            output_route: route(opts.output_channels.as_deref(), "--output-channels")?,
            input_route: route(opts.input_channels.as_deref(), "--input-channels")?,
            output_enabled: opts.output_enabled,
            input_enabled: opts.input_enabled,
        })
    }
}

/// One complete stream configuration: what the audio worker opens.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct AudioRequest {
    api: Option<cpal::HostId>,
    output_device: Option<String>,
    input_device: Option<String>,
    sample_rate: Option<u32>,
    buffer_size: Option<u32>,
    layout: usize,
    input_enabled: bool,
}

impl AudioRequest {
    fn patched(&self, p: &AudioPatch) -> Self {
        Self {
            api: p.api,
            output_device: p.output_device.clone(),
            input_device: p.input_device.clone(),
            sample_rate: p.sample_rate.or(self.sample_rate),
            buffer_size: p.buffer_size.or(self.buffer_size),
            layout: p.layout.unwrap_or(self.layout),
            input_enabled: p.input_enabled.unwrap_or(self.input_enabled),
        }
    }

    /// Whether `other` runs on the same output stream (so only the capture
    /// side needs reopening).
    fn same_output(&self, other: &Self) -> bool {
        (
            self.api,
            &self.output_device,
            self.sample_rate,
            self.buffer_size,
            self.layout,
        ) == (
            other.api,
            &other.output_device,
            other.sample_rate,
            other.buffer_size,
            other.layout,
        )
    }
}

/// Settings that need no stream reopen. Committed only together with a
/// successful open; `None` keeps the running value.
#[derive(Clone, Copy, Debug, Default)]
struct Extras {
    output_route: Option<ChannelRoute>,
    input_route: Option<ChannelRoute>,
    output_enabled: Option<bool>,
}

impl Extras {
    fn from_patch(p: &AudioPatch) -> Self {
        Self {
            output_route: p.output_route,
            input_route: p.input_route,
            output_enabled: p.output_enabled,
        }
    }

    /// Launch values: unspecified routes are `Direct`, output is on.
    fn launch(p: &AudioPatch) -> Self {
        Self {
            output_route: Some(p.output_route.unwrap_or(ChannelRoute::Direct)),
            input_route: Some(p.input_route.unwrap_or(ChannelRoute::Direct)),
            output_enabled: Some(p.output_enabled.unwrap_or(true)),
        }
    }

    fn snapshot<P: PluginExport>(res: &OutputResources<P>) -> Self {
        Self {
            output_route: Some(ChannelRoute::decode(
                res.output_channel_route.load(Ordering::Relaxed),
            )),
            input_route: Some(ChannelRoute::decode(
                res.input_channel_route.load(Ordering::Relaxed),
            )),
            output_enabled: Some(res.output_enabled.load(Ordering::Relaxed)),
        }
    }

    fn store<P: PluginExport>(self, res: &OutputResources<P>) {
        if let Some(route) = self.output_route {
            res.output_channel_route
                .store(route.encode(), Ordering::Relaxed);
        }
        if let Some(route) = self.input_route {
            res.input_channel_route
                .store(route.encode(), Ordering::Relaxed);
        }
        if let Some(on) = self.output_enabled {
            res.output_enabled.store(on, Ordering::Relaxed);
        }
    }
}

/// A request to the audio worker, the single owner of both streams.
enum EngineCmd {
    /// [`reopen_audio`]: apply these arguments and reopen even if unchanged.
    Reconfigure(AudioPatch),
    OutputDevice(Option<String>),
    InputDevice(Option<String>),
    InputEnabled(bool),
    Layout(usize),
}

/// `Send + Sync` handle to the audio worker; clones share it.
#[derive(Clone)]
struct Engine {
    tx: mpsc::Sender<EngineCmd>,
    /// The plugin's bus-layout check, so non-generic callers validate a
    /// layout index before submitting.
    check_layout: fn(usize) -> Result<(), String>,
    /// The committed audio API, for device listings.
    api: Arc<Mutex<Option<cpal::HostId>>>,
}

impl Engine {
    fn send(&self, cmd: EngineCmd) -> Result<(), String> {
        self.tx.send(cmd).map_err(|_| WORKER_GONE.to_string())
    }
}

/// The process's running standalone audio. One at a time: a second
/// [`start_audio`] fails instead of replacing the first.
struct Session {
    id: u64,
    engine: Option<Engine>,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// Reservation of [`SESSION`]; releases only its own session on drop.
struct LiveAudio {
    id: u64,
}

impl LiveAudio {
    fn reserve() -> Result<Self, String> {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut slot = lock(&SESSION);
        if slot.is_some() {
            return Err("standalone audio is already running in this process".into());
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        *slot = Some(Session { id, engine: None });
        Ok(Self { id })
    }

    fn register(&self, engine: Engine) {
        if let Some(session) = lock(&SESSION).as_mut().filter(|s| s.id == self.id) {
            session.engine = Some(engine);
        }
    }
}

impl Drop for LiveAudio {
    fn drop(&mut self) {
        let mut slot = lock(&SESSION);
        if slot.as_ref().is_some_and(|s| s.id == self.id) {
            *slot = None;
        }
    }
}

fn session_engine() -> Option<Engine> {
    lock(&SESSION).as_ref().and_then(|s| s.engine.clone())
}

/// Re-apply audio arguments to the running standalone: `--audio-api`,
/// `--output`, `--input`, `--sample-rate`, `--buffer`, `--bus-layout`,
/// `--output-enabled`, `--input-enabled`, `--output-channels`,
/// `--input-channels` (env fallbacks apply as at launch). Audio API and
/// devices always apply (absent = the default); other absent flags keep
/// the running value. `args` starts with the program name, like
/// `std::env::args_os()`.
///
/// Everything is validated here, then submitted as one request to the
/// audio worker, which reopens the streams (keeping the plugin instance
/// and its state) and commits the whole configuration or none of it. The
/// outcome arrives through [`on_audio_opened`].
///
/// # Errors
///
/// Returns an error when `args` don't parse, name an unknown audio API,
/// channel route or bus layout, give an out-of-range sample rate or
/// buffer, or no standalone audio is running.
pub fn reopen_audio<I>(args: I) -> Result<(), String>
where
    I: IntoIterator,
    I::Item: Into<std::ffi::OsString>,
{
    let opts = crate::cli::parse_from(args)?;
    let patch = AudioPatch::from_options(&opts)?;
    let engine = session_engine().ok_or("no standalone audio is running")?;
    if let Some(layout) = patch.layout {
        (engine.check_layout)(layout)?;
    }
    engine.send(EngineCmd::Reconfigure(patch))
}

/// Mic-ring frame width: the wider of the device output stream and the
/// plugin's input bus (so a 16-in / 2-out plugin captures 16 channels,
/// e.g. 16 JACK ports), capped at [`MAX_INPUT_RING_CHANNELS`].
fn input_ring_width(channels: usize, num_in: usize) -> usize {
    channels.max(num_in).min(MAX_INPUT_RING_CHANNELS)
}

// ---------------------------------------------------------------------------
// Channel routing
// ---------------------------------------------------------------------------

/// How the plugin's channels map onto the audio device's channels.
///
/// Stored encoded in an `AtomicUsize` on each controller (so the menu
/// can update it lock-free) and decoded in the audio callback. The
/// default, `Direct`, is the historical 1:1 mapping and is what every
/// non-multichannel-aware caller gets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChannelRoute {
    /// Plugin channel N ↔ device channel N, across every channel.
    /// Preserves multichannel plugins; for a stereo plugin on a
    /// 4-out interface this is the same as `Stereo { base: 0 }`.
    Direct,
    /// Plugin channels 0 and 1 ↔ device channels `base` and `base+1`.
    /// Other device outputs are silenced; other device inputs ignored.
    Stereo { base: usize },
    /// A single device channel `base`. On output the plugin's first
    /// two channels fold down (sum) into it; on input it feeds both
    /// plugin input channels.
    Mono { base: usize },
}

impl ChannelRoute {
    /// Pack into the `usize` the controllers store. `Direct` is 0 so a
    /// freshly-zeroed atomic decodes to the default mapping. A `base` past
    /// [`MAX_ROUTE_CHANNEL`] (only constructible directly; [`Self::parse`]
    /// rejects it) clamps to the last channel instead of overflowing.
    #[must_use]
    pub fn encode(self) -> usize {
        let clamp = |base: usize| base.min(MAX_ROUTE_CHANNEL - 1);
        match self {
            ChannelRoute::Direct => 0,
            ChannelRoute::Stereo { base } => 1 + clamp(base) * 2,
            ChannelRoute::Mono { base } => 2 + clamp(base) * 2,
        }
    }

    /// Parse a CLI / env spec into a route. Channel numbers are
    /// 1-based (matching the menu labels): `direct` / `all` →
    /// [`Self::Direct`], `N` → [`Self::Mono`] on channel N, `N-M` →
    /// [`Self::Stereo`] pair starting at N (requires `M == N + 1`).
    /// Channels are limited to `1..=`[`MAX_ROUTE_CHANNEL`]. Returns `None`
    /// for anything malformed or out of range.
    #[must_use]
    pub fn parse(spec: &str) -> Option<Self> {
        let s = spec.trim().to_ascii_lowercase();
        if s == "direct" || s == "all" {
            return Some(ChannelRoute::Direct);
        }
        let channel = |c: &str| {
            c.trim()
                .parse::<usize>()
                .ok()
                .filter(|c| (1..=MAX_ROUTE_CHANNEL).contains(c))
        };
        if let Some((a, b)) = s.split_once('-') {
            let (a, b) = (channel(a)?, channel(b)?);
            return (b == a + 1).then(|| ChannelRoute::Stereo { base: a - 1 });
        }
        channel(&s).map(|c| ChannelRoute::Mono { base: c - 1 })
    }

    /// Inverse of [`Self::encode`].
    #[must_use]
    pub fn decode(v: usize) -> Self {
        if v == 0 {
            return ChannelRoute::Direct;
        }
        let k = v - 1;
        if k.is_multiple_of(2) {
            ChannelRoute::Stereo { base: k / 2 }
        } else {
            ChannelRoute::Mono { base: (k - 1) / 2 }
        }
    }
}

// ---------------------------------------------------------------------------
// InputController
// ---------------------------------------------------------------------------

/// `Send + Sync` handle for managing mic input from the UI thread.
///
/// Cloneable; multiple holders can request toggles or device switches.
/// The capture `cpal::Stream` (`!Send` on macOS) lives on the audio
/// worker spawned by `start_audio`, together with the output stream.
#[derive(Clone)]
pub struct InputController {
    /// Audio callback reads this every block to decide whether to drain
    /// the input ring. The worker sets it once a capture stream is live.
    pub enabled: Arc<AtomicBool>,
    /// True if an input device was found at launch. When false, toggling
    /// on fails its request.
    pub has_device: bool,
    engine: Engine,
    /// Resolved device name after each open, for the menu checkmark.
    current_name: Arc<Mutex<Option<String>>>,
    /// How device input channels map onto the plugin's input bus, encoded
    /// per [`ChannelRoute`]. Shared with the output callback.
    channel_route: Arc<AtomicUsize>,
}

impl InputController {
    /// Open or close the capture stream. The worker applies it
    /// asynchronously and reports through [`on_audio_opened`].
    ///
    /// # Errors
    ///
    /// Returns an error when the audio worker has stopped.
    pub fn set_enabled(&self, on: bool) -> Result<(), String> {
        self.engine.send(EngineCmd::InputEnabled(on))
    }

    /// Read the current state. Source of truth for the audio
    /// callback's zero-fill decision.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Switch the input device by name (`None` = the system default). An
    /// open capture stream reopens on the new device; a closed one picks
    /// it up on the next enable.
    ///
    /// # Errors
    ///
    /// Returns an error when the audio worker has stopped.
    pub fn set_device(&self, name: Option<String>) -> Result<(), String> {
        self.engine.send(EngineCmd::InputDevice(name))
    }

    /// Currently-resolved input device name, if any.
    #[must_use]
    pub fn current_name(&self) -> Option<String> {
        lock(&self.current_name).clone()
    }

    /// Choose how device input channels feed the plugin's input bus.
    /// Takes effect on the next audio block.
    pub fn set_channel_route(&self, route: ChannelRoute) {
        self.channel_route.store(route.encode(), Ordering::Relaxed);
    }

    /// The current input channel routing.
    #[must_use]
    pub fn channel_route(&self) -> ChannelRoute {
        ChannelRoute::decode(self.channel_route.load(Ordering::Relaxed))
    }
}

// ---------------------------------------------------------------------------
// OutputController
// ---------------------------------------------------------------------------

/// `Send + Sync` handle for managing the output device from the
/// UI thread. Cloneable; clones share the worker.
#[derive(Clone)]
pub struct OutputController {
    /// Audio callback reads this every block to decide whether to
    /// zero-fill the output buffer (mute).
    pub enabled: Arc<AtomicBool>,
    engine: Engine,
    current_name: Arc<Mutex<Option<String>>>,
    /// How the plugin's output bus maps onto device output channels,
    /// encoded per [`ChannelRoute`]. Shared with the output callback.
    channel_route: Arc<AtomicUsize>,
    /// The plugin's active bus-layout index (into `P::bus_layouts()`),
    /// committed by the worker after a successful switch.
    layout: Arc<AtomicUsize>,
}

impl OutputController {
    /// Mute / unmute the output. The cpal stream stays open either
    /// way - disabling just makes the audio callback zero-fill its
    /// buffer, so the plugin keeps processing (transport ticks,
    /// MIDI is consumed) while the speakers are silent.
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// Read the current mute state. Source of truth for the audio
    /// callback's zero-fill decision.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Switch the output device by name (`None` = the system default). A
    /// failed switch restores the previous device; the outcome arrives
    /// through [`on_audio_opened`].
    ///
    /// # Errors
    ///
    /// Returns an error when the audio worker has stopped.
    pub fn set_device(&self, name: Option<String>) -> Result<(), String> {
        self.engine.send(EngineCmd::OutputDevice(name))
    }

    /// Switch the plugin to the declared bus layout at `index`. The plugin
    /// is re-prepared at that layout's dimensions; the device stream stays
    /// at a hardware-supported width and the plugin output is mapped onto
    /// it. Capture reopens at the new layout's input width.
    ///
    /// # Errors
    ///
    /// Returns an error when `index` names no usable layout or the audio
    /// worker has stopped.
    pub fn set_layout(&self, index: usize) -> Result<(), String> {
        (self.engine.check_layout)(index)?;
        self.engine.send(EngineCmd::Layout(index))
    }

    /// The plugin's active bus-layout index (into `P::bus_layouts()`).
    #[must_use]
    pub fn current_index(&self) -> usize {
        self.layout.load(Ordering::Relaxed)
    }

    /// Currently-resolved output device name, or `None` if not
    /// resolvable.
    #[must_use]
    pub fn current_name(&self) -> Option<String> {
        lock(&self.current_name).clone()
    }

    /// Choose how the plugin's output bus maps onto device output
    /// channels. Takes effect on the next audio block.
    pub fn set_channel_route(&self, route: ChannelRoute) {
        self.channel_route.store(route.encode(), Ordering::Relaxed);
    }

    /// The current output channel routing.
    #[must_use]
    pub fn channel_route(&self) -> ChannelRoute {
        ChannelRoute::decode(self.channel_route.load(Ordering::Relaxed))
    }
}

// ---------------------------------------------------------------------------
// Device enumeration helpers (used by `--list-devices` + menus).
// ---------------------------------------------------------------------------

/// Print the devices of `api` (`None` = the platform default). Used by
/// `--list-devices`, which honors `--audio-api`.
///
/// # Errors
///
/// Returns an error when `api` names no compiled-in audio API or it can't
/// be opened.
pub fn list_devices(api: Option<&str>) -> Result<(), String> {
    let host = open_host(parse_audio_api(api)?)?;
    println!("Audio devices ({})", api_name(host.id()));
    for (label, output) in [("Output:", true), ("Input:", false)] {
        println!("{label}");
        let (default, names) = enumerate_devices(&host, output);
        for name in names {
            let marker = if default.as_deref() == Some(name.as_str()) {
                " (default)"
            } else {
                ""
            };
            println!("  {name}{marker}");
        }
    }
    Ok(())
}

/// Snapshot of the running session's output devices
/// (`(default_name, all_names)`).
#[must_use]
pub fn list_output_devices() -> (Option<String>, Vec<String>) {
    enumerate_devices(&session_host(), true)
}

/// Snapshot of the running session's input devices
/// (`(default_name, all_names)`).
#[must_use]
pub fn list_input_devices() -> (Option<String>, Vec<String>) {
    enumerate_devices(&session_host(), false)
}

/// The running session's committed audio API (the default without one).
fn session_host() -> cpal::Host {
    session_engine()
        .and_then(|engine| *lock(&engine.api))
        .and_then(|id| cpal::host_from_id(id).ok())
        .unwrap_or_else(cpal::default_host)
}

fn device_name(device: &cpal::Device) -> Option<String> {
    device.description().map(|d| d.name().to_string()).ok()
}

fn enumerate_devices(host: &cpal::Host, output: bool) -> (Option<String>, Vec<String>) {
    let (default, devices) = if output {
        (host.default_output_device(), host.output_devices().ok())
    } else {
        (host.default_input_device(), host.input_devices().ok())
    };
    let names = devices
        .map(|it| it.filter_map(|d| device_name(&d)).collect())
        .unwrap_or_default();
    (default.and_then(|d| device_name(&d)), names)
}

/// Background-refreshed cache of cpal device names.
///
/// `CoreAudio` enumeration is slow - hundreds of milliseconds with a
/// few devices connected - and the macOS menu used to run it
/// synchronously on the main thread every time a device submenu
/// opened, which made the submenu visibly lag. The menu now reads
/// cached names (instant) and fires an off-thread refresh so the
/// *next* open reflects hot-plugged or removed devices. Cloneable;
/// all clones share one cache.
#[derive(Clone)]
pub struct DeviceCache {
    inner: Arc<Mutex<DeviceNames>>,
    refreshing: Arc<AtomicBool>,
}

impl Default for DeviceCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct DeviceNames {
    outputs: Vec<String>,
    inputs: Vec<String>,
    /// False until the first enumeration lands. Distinguishes "not
    /// warmed yet" from "warmed and genuinely empty" so a machine with
    /// no inputs doesn't re-enumerate on every read.
    warmed: bool,
}

impl DeviceCache {
    /// Create the cache and warm it on a background thread.
    #[must_use]
    pub fn new() -> Self {
        let cache = Self {
            inner: Arc::new(Mutex::new(DeviceNames::default())),
            refreshing: Arc::new(AtomicBool::new(false)),
        };
        cache.refresh_async();
        cache
    }

    /// Cached output device names. Blocks once on a cold read (a menu
    /// opened before the warm-up finished) so the list is never
    /// spuriously empty.
    #[must_use]
    pub fn outputs(&self) -> Vec<String> {
        self.ensure_warm();
        self.inner
            .lock()
            .map(|g| g.outputs.clone())
            .unwrap_or_default()
    }

    /// Cached input device names. Same cold-read guarantee as
    /// [`Self::outputs`].
    #[must_use]
    pub fn inputs(&self) -> Vec<String> {
        self.ensure_warm();
        self.inner
            .lock()
            .map(|g| g.inputs.clone())
            .unwrap_or_default()
    }

    fn ensure_warm(&self) {
        // Check (and release the lock) before the slow enumeration so
        // we never hold the mutex across a CoreAudio round-trip.
        match self.inner.lock() {
            Ok(g) if g.warmed => return,
            Ok(_) => {}
            Err(_) => return,
        }
        let (_, outputs) = list_output_devices();
        let (_, inputs) = list_input_devices();
        if let Ok(mut names) = self.inner.lock() {
            names.outputs = outputs;
            names.inputs = inputs;
            names.warmed = true;
        }
    }

    /// Re-enumerate both device lists on a background thread, storing
    /// the result for the next read. No-op while a refresh is already
    /// in flight, so rapid menu reopens don't pile up threads.
    pub fn refresh_async(&self) {
        if self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let refreshing = Arc::clone(&self.refreshing);
        std::thread::spawn(move || {
            let (_, outputs) = list_output_devices();
            let (_, inputs) = list_input_devices();
            if let Ok(mut names) = inner.lock() {
                names.outputs = outputs;
                names.inputs = inputs;
                names.warmed = true;
            }
            refreshing.store(false, Ordering::Release);
        });
    }
}

// ---------------------------------------------------------------------------
// start_audio
// ---------------------------------------------------------------------------

/// Validate the launch arguments, instantiate the plugin, and start the
/// audio worker that owns both cpal streams. Returns the handles the
/// caller needs to push MIDI and reach the worker. The outcome, including
/// a startup failure, is also reported through [`on_audio_opened`].
///
/// # Errors
///
/// Returns an error if standalone audio is already running in this
/// process, an argument is invalid (audio API, bus layout, channel route,
/// sample rate, buffer), a device can't be found or opened at the requested
/// configuration, a requested input can't open, or the plugin refuses
/// preparation.
pub fn start_audio<P: PluginExport>(opts: &Options) -> Result<AudioHandles<P>, BoxErr> {
    match start_session::<P>(opts) {
        Ok((handles, opened)) => {
            notify_audio_opened(Ok(opened));
            Ok(handles)
        }
        Err(e) => {
            notify_audio_opened(Err(e.clone()));
            Err(e.into())
        }
    }
}

#[allow(clippy::too_many_lines)]
fn start_session<P: PluginExport>(
    opts: &Options,
) -> Result<(AudioHandles<P>, AudioOpened), String> {
    // Reserve first, so a concurrent second runner fails before it
    // touches devices or the registration of the running one.
    let live = LiveAudio::reserve()?;
    let patch = AudioPatch::from_options(opts)?;
    let request = AudioRequest::default().patched(&patch);
    check_layout::<P>(request.layout)?;

    // Resolve the launch geometry here so the plugin is prepared (and its
    // `--state` / `--preset` restored) before audio runs. The worker
    // resolves again on its own thread (cpal streams are `!Send`).
    let host = open_host(request.api)?;
    let launch = resolve_output::<P>(&host, &request)?;
    let sample_rate = f64::from(launch.config.sample_rate);
    let channels = usize::from(launch.config.channels);
    let (num_in, _, num_main_in) = layout_at_index::<P>(request.layout);
    // Keep the public field name for compatibility; this means audio-input capable.
    let is_effect = num_in > 0;

    // Capacity 256: covers a generous MIDI burst within a single
    // audio callback period. ArrayQueue is lock-free MPMC - the MIDI
    // input thread pushes, the audio thread drains, neither blocks.
    // On overflow the producer drops the oldest event (see midi.rs).
    let pending: Arc<ArrayQueue<MidiEvent>> = Arc::new(ArrayQueue::new(256));
    // Capacity 1, newest-wins: only the most recent editor state-load matters
    // if several arrive before the audio thread drains one.
    let pending_state: Arc<ArrayQueue<Vec<u8>>> = Arc::new(ArrayQueue::new(1));
    let (max_frames, frame_bound) = launch.config.buffer_size_geometry(&launch.supported);
    let plugin = Arc::new(Mutex::new({
        let mut p = P::create_for(InstanceContext::new(standalone_host()));
        p.init();
        p.prepare(
            &moose_core::plugin::PreparationConfig::new(
                AudioConfig::new(sample_rate, max_frames),
                frame_bound,
            )
            .with_host(standalone_host()),
        )
        .map_err(|error| format!("plugin preparation refused: {error:?}"))?;
        // Apply `--state <path>` BEFORE snapping smoothers so the
        // first audio block sees the restored values, not defaults
        // ramping toward them.
        if let Some(path) = opts.state_path.as_deref() {
            crate::state::load_into(&mut p, path);
        }
        // `--preset` layers on top of `--state` (both pre-snap so
        // the first block sees restored values), resolved through
        // the same store `--list-presets` uses.
        if let Some(sel) = opts.preset.as_deref() {
            crate::presets::apply_on_launch::<P>(opts.presets_dir.as_deref(), &mut p, sel);
        }
        p.params().snap_smoothers();
        p
    }));
    let prepared = Prepared {
        sample_rate: launch.config.sample_rate,
        max_frames,
        layout: request.layout,
    };

    let input_device = is_effect
        .then(|| resolve_device(&host, request.input_device.as_deref(), false).ok())
        .flatten();
    let has_input_device = input_device.is_some();
    if is_effect && !has_input_device {
        eprintln!("Note: no input device found - input-enable will fail.");
    }

    let transport = Transport::new(opts.bpm.unwrap_or(120.0), sample_rate);

    // Decode `--input-file` (if set) once at startup against the
    // resolved device sample-rate / channel-count. Hard error on
    // unreadable / unparseable file - we fail noisily here rather
    // than letting the audio worker silently emit zeros.
    #[cfg(feature = "playback")]
    let playback = match &opts.input_file {
        // `num_main_in > 0`: a layout with no main input bus has nowhere
        // to route the file, and decoding to a zero width divides by zero
        // in `PlaybackSource::from_wav`.
        Some(file) if is_effect && num_main_in > 0 => {
            // Decode to the plugin's MAIN-bus width, not the device
            // stream width: the callback mixes this into
            // `input_bufs[..num_main_in]`.
            let src = crate::playback::PlaybackSource::from_wav(file, sample_rate, num_main_in)?;
            vlog!(
                "Playback: {} → input bus (one-shot, sums with mic when enabled)",
                file.display()
            );
            Some(Arc::new(src))
        }
        Some(_) => {
            eprintln!("--input-file ignored: plugin has no main input bus");
            None
        }
        None => None,
    };
    #[cfg(not(feature = "playback"))]
    let _ = num_main_in;

    // Decode `--sidechain-file` against the sidechain bus width. A
    // plugin with no sidechain bus (or a non-effect) has nowhere to
    // route it, so warn and ignore rather than fail.
    #[cfg(feature = "playback")]
    let sidechain_playback = {
        let sc_width = num_in.saturating_sub(num_main_in);
        match &opts.sidechain_file {
            Some(file) if is_effect && sc_width > 0 => {
                let src = crate::playback::PlaybackSource::from_wav(file, sample_rate, sc_width)?;
                vlog!("Sidechain: {} → sidechain bus (one-shot)", file.display());
                Some(Arc::new(src))
            }
            Some(_) => {
                eprintln!("--sidechain-file ignored: plugin has no sidechain input bus");
                None
            }
            None => None,
        }
    };

    // `--output-file` capture sink. Created here so any filesystem error
    // propagates back to the runner before audio starts.
    #[cfg(feature = "playback")]
    let capture = match &opts.output_file {
        Some(file) => {
            let sink = crate::playback::CaptureSink::create(file, sample_rate, channels)?;
            vlog!(
                "Capture: {} ({} Hz, {} ch, f32) - pre-mute output",
                file.display(),
                sample_rate,
                channels,
            );
            Some(sink)
        }
        None => None,
    };

    // Decoded files and the capture sink are fixed to the launch format.
    #[cfg(feature = "playback")]
    let fixed = FixedFormat {
        sample_rate: (playback.is_some() || sidechain_playback.is_some() || capture.is_some())
            .then_some(launch.config.sample_rate),
        channels: capture.is_some().then_some(launch.config.channels),
    };
    #[cfg(not(feature = "playback"))]
    let fixed = FixedFormat::default();

    // Bound the ring at ~100 ms of capture frames. The producer drops the
    // oldest frame on overflow, so a slow render (or a paused output)
    // sheds the stalest audio instead of growing.
    let ring_frames = (sample_count_usize(sample_rate) / 10).max(1);
    let res = OutputResources {
        plugin: Arc::clone(&plugin),
        pending: Arc::clone(&pending),
        pending_state: Arc::clone(&pending_state),
        input_ring: Arc::new(ArrayQueue::new(ring_frames)),
        input_enabled: Arc::new(AtomicBool::new(false)),
        output_enabled: Arc::new(AtomicBool::new(false)),
        transport: transport.clone(),
        input_name: Arc::new(Mutex::new(input_device.as_ref().and_then(device_name))),
        output_name: Arc::new(Mutex::new(None)),
        input_channel_route: Arc::new(AtomicUsize::new(0)),
        output_channel_route: Arc::new(AtomicUsize::new(0)),
        layout: Arc::new(AtomicUsize::new(request.layout)),
        api: Arc::new(Mutex::new(request.api)),
        #[cfg(feature = "playback")]
        playback: playback.clone(),
        #[cfg(feature = "playback")]
        sidechain_playback: sidechain_playback.clone(),
        #[cfg(feature = "playback")]
        capture: capture.as_ref().map(super::playback::CaptureSink::pusher),
    };

    let (tx, rx) = mpsc::channel::<EngineCmd>();
    let engine = Engine {
        tx,
        check_layout: check_layout::<P>,
        api: Arc::clone(&res.api),
    };
    let input = InputController {
        enabled: Arc::clone(&res.input_enabled),
        has_device: has_input_device,
        engine: engine.clone(),
        current_name: Arc::clone(&res.input_name),
        channel_route: Arc::clone(&res.input_channel_route),
    };
    let output = OutputController {
        enabled: Arc::clone(&res.output_enabled),
        engine: engine.clone(),
        current_name: Arc::clone(&res.output_name),
        channel_route: Arc::clone(&res.output_channel_route),
        layout: Arc::clone(&res.layout),
    };

    let (reply_tx, reply_rx) = mpsc::sync_channel::<Result<AudioOpened, String>>(1);
    let launch_extras = Extras::launch(&patch);
    std::thread::Builder::new()
        .name("moose-standalone-audio".into())
        .spawn(move || {
            let mut worker = Worker::<P> {
                res,
                is_effect,
                fixed,
                request: request.clone(),
                prepared,
                live: None,
            };
            let first = worker.reconfigure(&request, launch_extras, true);
            let opened = first.is_ok();
            let _ = reply_tx.send(first);
            if opened {
                worker.run(&rx);
            }
        })
        .map_err(|e| format!("could not spawn the audio worker: {e}"))?;

    // The worker reports the launch open synchronously, so a failure
    // reaches `start_audio`'s caller.
    let opened = reply_rx
        .recv()
        .map_err(|_| "the audio worker exited before reporting".to_string())??;
    live.register(engine);

    if !output.is_enabled() {
        vlog!(
            "Output: muted at launch - toggle from the Plugin menu or \
             pass --output-enabled on"
        );
    }
    if is_effect {
        vlog!(
            "Input:  {} ({})",
            input.current_name().as_deref().unwrap_or("(default)"),
            if input.is_enabled() {
                "enabled"
            } else if cfg!(target_os = "macos") {
                "disabled - press Cmd+I in the window or pass --input-enabled on"
            } else {
                "disabled - press Ctrl+I in the window or pass --input-enabled on"
            }
        );
    }

    Ok((
        AudioHandles {
            _live: live,
            pending,
            pending_state,
            plugin,
            sample_rate,
            channels,
            is_effect,
            input,
            output,
            transport,
            #[cfg(feature = "playback")]
            playback,
            #[cfg(feature = "playback")]
            sidechain_playback,
            #[cfg(feature = "playback")]
            capture,
        },
        opened,
    ))
}

// ---------------------------------------------------------------------------
// Audio worker: the single owner of both streams
// ---------------------------------------------------------------------------

/// State shared between the worker, the controllers and the output
/// callback. New streams clone the inner Arcs so `process()` keeps seeing
/// the same plugin / pending / transport state across reopens.
struct OutputResources<P: PluginExport> {
    plugin: Arc<Mutex<P>>,
    pending: Arc<ArrayQueue<MidiEvent>>,
    /// Editor-initiated `load_state` blobs, drained at the block top (see
    /// [`AudioHandles::pending_state`]).
    pending_state: Arc<ArrayQueue<Vec<u8>>>,
    input_ring: Arc<InputRing>,
    /// True while a capture stream is live (set by the worker).
    input_enabled: Arc<AtomicBool>,
    /// Drives the audio callback's mute decision.
    output_enabled: Arc<AtomicBool>,
    transport: Transport,
    input_name: Arc<Mutex<Option<String>>>,
    output_name: Arc<Mutex<Option<String>>>,
    /// Input channel routing (encoded [`ChannelRoute`]).
    input_channel_route: Arc<AtomicUsize>,
    /// Output channel routing (encoded [`ChannelRoute`]).
    output_channel_route: Arc<AtomicUsize>,
    /// The committed bus-layout index.
    layout: Arc<AtomicUsize>,
    /// The committed audio API.
    api: Arc<Mutex<Option<cpal::HostId>>>,
    /// Optional `.wav` playback source (gated on the `playback`
    /// feature). When present, summed into the input bus alongside
    /// the mic ring - see the matrix in `cli.rs::HELP`.
    #[cfg(feature = "playback")]
    playback: Option<Arc<crate::playback::PlaybackSource>>,
    /// Optional `.wav` sidechain source (gated on `playback`).
    #[cfg(feature = "playback")]
    sidechain_playback: Option<Arc<crate::playback::PlaybackSource>>,
    /// Optional `--output-file` capture pusher, cloned into each stream.
    #[cfg(feature = "playback")]
    capture: Option<crate::playback::CapturePusher>,
}

/// Formats fixed at launch by decoded files / the capture sink.
#[derive(Clone, Copy, Default)]
struct FixedFormat {
    sample_rate: Option<u32>,
    channels: Option<u16>,
}

/// What the plugin was last prepared for.
#[derive(Clone, Copy)]
struct Prepared {
    sample_rate: u32,
    max_frames: usize,
    layout: usize,
}

/// The open streams of the committed configuration.
struct Live {
    host: cpal::Host,
    output: cpal::Stream,
    input: Option<cpal::Stream>,
    /// Mic-ring width of this output geometry; capture opens at it.
    ring_width: usize,
    opened: AudioOpened,
}

struct Worker<P: PluginExport> {
    res: OutputResources<P>,
    is_effect: bool,
    fixed: FixedFormat,
    /// The committed request.
    request: AudioRequest,
    prepared: Prepared,
    live: Option<Live>,
}

/// Device and stream geometry a request resolves to.
struct ResolvedOutput {
    device: cpal::Device,
    supported: cpal::SupportedStreamConfig,
    config: cpal::StreamConfig,
}

/// The per-stream geometry the output callback runs at.
#[derive(Clone, Copy)]
struct StreamSpec {
    /// Device interleave stride (= `config.channels`).
    channels: usize,
    /// The plugin bus `(num_in, num_out)`; mapped onto the device channels
    /// by the routes.
    num_in: usize,
    num_out: usize,
    /// Main-input width; channels `[num_main_in, num_in)` are the sidechain.
    num_main_in: usize,
    layout: usize,
    sample_rate: f64,
    /// The negotiated frame bound the callback scratch is preallocated to.
    frame_bound: usize,
    is_effect: bool,
}

impl<P: PluginExport> Worker<P> {
    fn run(mut self, rx: &mpsc::Receiver<EngineCmd>) {
        while let Ok(cmd) = rx.recv() {
            let mut next = self.request.clone();
            let mut extras = Extras::default();
            let mut force = false;
            match cmd {
                EngineCmd::Reconfigure(patch) => {
                    next = next.patched(&patch);
                    extras = Extras::from_patch(&patch);
                    force = true;
                }
                EngineCmd::OutputDevice(name) => next.output_device = name,
                EngineCmd::InputDevice(name) => next.input_device = name,
                EngineCmd::InputEnabled(on) => next.input_enabled = on,
                EngineCmd::Layout(index) => next.layout = index,
            }
            let result =
                check_layout::<P>(next.layout).and_then(|()| self.apply(&next, extras, force));
            notify_audio_opened(result);
        }
    }

    /// Commit `next` completely, or restore the previous configuration.
    fn apply(
        &mut self,
        next: &AudioRequest,
        extras: Extras,
        force: bool,
    ) -> Result<AudioOpened, String> {
        let prev = self.request.clone();
        let before = Extras::snapshot(&self.res);
        let reopen_output = force || !prev.same_output(next);
        let e = match self.reconfigure(next, extras, reopen_output) {
            Ok(opened) => return Ok(opened),
            Err(e) => e,
        };
        before.store(&self.res);
        let restored = self
            .reconfigure(&prev, Extras::default(), false)
            .or_else(|e2| {
                // Keep the output playing even if the old input is gone.
                let output_only = AudioRequest {
                    input_enabled: false,
                    ..prev.clone()
                };
                if prev.input_enabled {
                    self.reconfigure(&output_only, Extras::default(), false)
                        .map_err(|_| e2)
                } else {
                    Err(e2)
                }
            });
        Err(match restored {
            Ok(_) => format!("{e} (the previous configuration was restored)"),
            Err(e2) => format!("{e}; restoring the previous configuration failed too: {e2}"),
        })
    }

    /// Close what `req` replaces, open it, and commit on success.
    fn reconfigure(
        &mut self,
        req: &AudioRequest,
        extras: Extras,
        reopen_output: bool,
    ) -> Result<AudioOpened, String> {
        // Release the streams first: some backends refuse a second
        // exclusive stream on one device.
        self.res.input_enabled.store(false, Ordering::Relaxed);
        if let Some(live) = &mut self.live {
            live.input = None;
        }
        if reopen_output {
            self.live = None;
        }
        // Frames captured at the old width must not reach the new layout.
        while self.res.input_ring.pop().is_some() {}
        let (mut live, reopened) = match self.live.take() {
            Some(live) => (live, false),
            None => (self.open_output(req)?, true),
        };
        let result = self.open_input_and_commit(&mut live, req, extras);
        // A kept output stays playing; a new, uncommitted one is dropped.
        if result.is_ok() || !reopened {
            self.live = Some(live);
        }
        if result.is_ok() {
            self.request = req.clone();
        }
        result
    }

    fn open_output(&mut self, req: &AudioRequest) -> Result<Live, String> {
        let host = open_host(req.api)?;
        let out = resolve_output::<P>(&host, req)?;
        let rate = out.config.sample_rate;
        if let Some(fixed) = self.fixed.sample_rate
            && fixed != rate
        {
            return Err(format!(
                "sample rate {rate} Hz differs from the {fixed} Hz the playback / \
                 capture files were opened at"
            ));
        }
        if let Some(fixed) = self.fixed.channels
            && fixed != out.config.channels
        {
            return Err(format!(
                "{} output channels differ from the {fixed} the --output-file capture \
                 was opened with",
                out.config.channels
            ));
        }
        let (frame_bound, provenance) = out.config.buffer_size_geometry(&out.supported);
        self.prepare(rate, frame_bound, provenance, req.layout)?;
        let (num_in, num_out, num_main_in) = layout_at_index::<P>(req.layout);
        let spec = StreamSpec {
            channels: usize::from(out.config.channels),
            num_in,
            num_out,
            num_main_in,
            layout: req.layout,
            sample_rate: f64::from(rate),
            frame_bound,
            is_effect: self.is_effect,
        };
        let output = build_output_stream::<P>(
            &out.device,
            &out.config,
            out.supported.sample_format(),
            spec,
            &self.res,
        )?;
        Ok(Live {
            opened: AudioOpened {
                api: api_name(host.id()),
                output_device: device_name(&out.device),
                sample_rate: rate,
                buffer_size: req.buffer_size,
                output_channels: out.config.channels,
                input_channels: None,
                bus_layout: req.layout,
            },
            host,
            output,
            input: None,
            ring_width: input_ring_width(spec.channels, num_in),
        })
    }

    /// Renew the plugin's preparation when the device geometry needs it.
    fn prepare(
        &mut self,
        sample_rate: u32,
        frame_bound: usize,
        provenance: moose_core::plugin::PreparationFrameBound,
        layout: usize,
    ) -> Result<(), String> {
        let prev = self.prepared;
        let bound_grew = frame_bound > prev.max_frames;
        // A layout or rate change re-prepares per-channel DSP state; every
        // format wrapper couples an arrangement change to a reset.
        let force = layout != prev.layout || sample_rate != prev.sample_rate;
        let mut p = lock(&self.res.plugin);
        // Workers renew on EACH actual device open when the plugin requires
        // fallible preparation, including equal-B changes in provenance.
        // Legacy plugins keep the old reset timing, so the launch
        // state/preset load is not erased by a gratuitous second reset.
        if renew_device_preparation(p.requires_fallible_preparation(), bound_grew, force) {
            p.prepare(
                &moose_core::plugin::PreparationConfig::new(
                    AudioConfig::new(f64::from(sample_rate), frame_bound),
                    provenance,
                )
                .with_host(standalone_host()),
            )
            .map_err(|error| format!("plugin device preparation refused: {error:?}"))?;
            self.prepared = Prepared {
                sample_rate,
                max_frames: frame_bound,
                layout,
            };
        }
        Ok(())
    }

    fn open_input_and_commit(
        &self,
        live: &mut Live,
        req: &AudioRequest,
        extras: Extras,
    ) -> Result<AudioOpened, String> {
        // Capture opens at the mic-ring width of the target geometry.
        let input = if self.is_effect && req.input_enabled {
            let device = resolve_device(&live.host, req.input_device.as_deref(), false)
                .map_err(|e| format!("input: {e}"))?;
            let (stream, width) = build_input_stream(
                &device,
                live.ring_width,
                live.opened.sample_rate,
                Arc::clone(&self.res.input_ring),
            )
            .map_err(|e| format!("input: {e}"))?;
            Some((stream, width, device_name(&device)))
        } else {
            None
        };

        // Commit: everything the request carries lands together.
        extras.store(&self.res);
        self.res.layout.store(req.layout, Ordering::Relaxed);
        self.res
            .transport
            .set_sample_rate(f64::from(live.opened.sample_rate));
        *lock(&self.res.api) = req.api;
        live.output
            .play()
            .map_err(|e| format!("could not start the output stream: {e}"))?;
        lock(&self.res.output_name).clone_from(&live.opened.output_device);
        let mut opened = live.opened.clone();
        match input {
            Some((stream, width, name)) => {
                stream
                    .play()
                    .map_err(|e| format!("input: could not start the capture stream: {e}"))?;
                live.input = Some(stream);
                self.res.input_enabled.store(true, Ordering::Relaxed);
                opened.input_channels = Some(width);
                *lock(&self.res.input_name) = name;
            }
            None => {
                if req.input_device.is_some() {
                    lock(&self.res.input_name).clone_from(&req.input_device);
                }
            }
        }
        live.opened.input_channels = opened.input_channels;
        vlog!(
            "Output: {} ({}) @ {} Hz, {} ch; input: {}",
            opened.output_device.as_deref().unwrap_or("(unnamed)"),
            opened.api,
            opened.sample_rate,
            opened.output_channels,
            opened
                .input_channels
                .map_or_else(|| "off".to_string(), |ch| format!("{ch} ch")),
        );
        Ok(opened)
    }
}

/// Size `bufs` to `n` zeroed lanes of `num_frames`. Allocation-free once
/// [`preallocated_lanes`] reserved the frame bound.
fn shape_lanes(bufs: &mut Vec<Vec<f32>>, n: usize, num_frames: usize) {
    bufs.resize_with(n, Vec::new);
    for buf in bufs.iter_mut() {
        buf.clear();
        buf.resize(num_frames, 0.0);
    }
}

fn preallocated_lanes(n: usize, frames: usize) -> Vec<Vec<f32>> {
    (0..n).map(|_| Vec::with_capacity(frames)).collect()
}

fn build_output_stream<P: PluginExport>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    spec: StreamSpec,
    res: &OutputResources<P>,
) -> Result<cpal::Stream, String> {
    if sample_format != cpal::SampleFormat::F32 {
        return Err(format!(
            "audio output format {sample_format:?} is not supported \
             (moose standalone handles f32 only)"
        ));
    }
    let plugin_a = Arc::clone(&res.plugin);
    let pending_a = Arc::clone(&res.pending);
    let pending_state_a = Arc::clone(&res.pending_state);
    let ring_a = Arc::clone(&res.input_ring);
    let enabled_a = Arc::clone(&res.input_enabled);
    let out_enabled_a = Arc::clone(&res.output_enabled);
    let in_route_a = Arc::clone(&res.input_channel_route);
    let out_route_a = Arc::clone(&res.output_channel_route);
    let transport_a = res.transport.clone();
    #[cfg(feature = "playback")]
    let playback_a = res.playback.clone();
    #[cfg(feature = "playback")]
    let sidechain_playback_a = res.sidechain_playback.clone();
    #[cfg(feature = "playback")]
    let capture_a = res.capture.clone();

    // Per-stream audio-callback scratch, preallocated here (not on the
    // audio thread) for the negotiated frame bound and both bus widths,
    // so the first callback doesn't allocate. Owned by the move-closure;
    // cpal calls it on a single dedicated audio thread per stream.
    let input_width = if spec.is_effect { spec.num_in } else { 0 };
    let mut channel_bufs = preallocated_lanes(spec.num_out, spec.frame_bound);
    let mut input_bufs = preallocated_lanes(input_width, spec.frame_bound);
    let mut ptr_scratch = CallbackPtrScratch {
        inputs: Vec::with_capacity(input_width),
        outputs: Vec::with_capacity(spec.num_out),
    };
    let mut event_list = EventList::with_capacity(EVENT_LIST_PREALLOC);
    let mut output_events = EventList::with_capacity(EVENT_LIST_PREALLOC);
    let mut sub_event_scratch = EventList::with_capacity(EVENT_LIST_PREALLOC);
    // Cached for `chunked_process::process_chunked`: the chunker can't call
    // `plugin.params()` itself because it already holds `&mut plugin`.
    let (param_infos, params_arc) = {
        let p = lock(&plugin_a);
        (p.params().param_infos(), p.params_arc())
    };
    let automation = P::info().automation;
    // Pre-grow the widening / alias-copy scratch to the stream's frame
    // bound (the same number `prepare` hands the plugin) so an f64
    // plugin's first callback doesn't allocate.
    let mut scratch: RawBufferScratch<<P as moose_core::plugin::PluginRuntime>::Sample> =
        RawBufferScratch::default();
    scratch.ensure_capacity(spec.num_in, spec.num_out, spec.frame_bound);
    let bus_routing = bus_routing_at_index::<P>(spec.layout);

    device
        .build_output_stream(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                audio_callback::<P>(
                    data,
                    spec.channels,
                    spec.num_in,
                    spec.num_main_in,
                    spec.num_out,
                    spec.sample_rate,
                    spec.is_effect,
                    bus_routing,
                    &plugin_a,
                    &pending_a,
                    &pending_state_a,
                    &ring_a,
                    &enabled_a,
                    &out_enabled_a,
                    &in_route_a,
                    &out_route_a,
                    &transport_a,
                    &mut channel_bufs,
                    &mut input_bufs,
                    &mut event_list,
                    &mut output_events,
                    &mut sub_event_scratch,
                    &param_infos,
                    &params_arc,
                    automation,
                    &mut ptr_scratch,
                    &mut scratch,
                    #[cfg(feature = "playback")]
                    playback_a.as_ref(),
                    #[cfg(feature = "playback")]
                    sidechain_playback_a.as_ref(),
                    #[cfg(feature = "playback")]
                    capture_a.as_ref(),
                );
            },
            |err| eprintln!("Audio error: {err}"),
            None,
        )
        .map_err(|e| format!("could not build the output stream: {e}"))
}

/// Resolve an input `StreamConfig` for `device`. Prefers exactly `channels`
/// at the render rate; otherwise the narrowest advertised count above it at
/// that rate (JACK advertises 1, 2, 4, 6, 8, 16, ... ports, so a 15-input
/// layout opens 16 ports and the extra lane is dropped by
/// [`normalize_input_frame`]). Many capture devices can't open either shape (a mono USB mic on a stereo
/// render stream, a 44.1 kHz interface against a 48 kHz output). On any
/// mismatch, fall back to the device's own default config so the mic
/// still opens. A fallback whose rate differs from the render rate is
/// not sample-rate-converted, so the ring drifts within its drop-oldest
/// cap - opening at all beats a permanently-unusable mic.
fn resolve_input_config(
    device: &cpal::Device,
    channels: usize,
    sample_rate: u32,
) -> cpal::StreamConfig {
    let ideal = cpal::StreamConfig {
        channels: u16::try_from(channels).unwrap_or(u16::MAX),
        sample_rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let counts: Vec<u16> = device
        .supported_input_configs()
        .map(|r| {
            r.filter(|c| c.min_sample_rate() <= sample_rate && c.max_sample_rate() >= sample_rate)
                .map(|c| c.channels())
                .collect()
        })
        .unwrap_or_default();
    if let Some(channels) = capture_channels(&counts, ideal.channels) {
        return cpal::StreamConfig { channels, ..ideal };
    }
    match device.default_input_config() {
        Ok(def) => {
            eprintln!(
                "mic: device can't open {} ch @ {} Hz; using its default {} ch @ {} Hz",
                ideal.channels,
                ideal.sample_rate,
                def.channels(),
                def.sample_rate(),
            );
            def.config()
        }
        Err(_) => ideal,
    }
}

/// The capture width to open for a `want`-lane ring: exactly `want` when
/// advertised, else the narrowest advertised count above it.
fn capture_channels(advertised: &[u16], want: u16) -> Option<u16> {
    advertised.iter().copied().filter(|&c| c >= want).min()
}

/// Build (not start) a capture stream on `device` that hands frames to
/// `ring`, normalized to `width` lanes (the mic-ring width of the output
/// geometry it was opened for; a geometry change reopens it). Returns the
/// stream and its device channel count.
fn build_input_stream(
    device: &cpal::Device,
    width: usize,
    sample_rate: u32,
    ring: Arc<InputRing>,
) -> Result<(cpal::Stream, u16), String> {
    let input_config = resolve_input_config(device, width, sample_rate);
    let in_ch = usize::from(input_config.channels);
    let stream = device
        .build_input_stream(
            &input_config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if in_ch == 0 {
                    return;
                }
                // `force_push` drops the oldest frame when the ring is
                // full - a slow / paused render sheds the stalest audio.
                // No lock, no allocation.
                for frame in data.chunks_exact(in_ch) {
                    ring.force_push(normalize_input_frame(frame, width));
                }
            },
            |err| eprintln!("Input error: {err}"),
            None,
        )
        .map_err(|e| format!("could not build the input stream: {e}"))?;
    Ok((stream, input_config.channels))
}

/// The named device (case-insensitive substring), or the default.
fn resolve_device(
    host: &cpal::Host,
    name: Option<&str>,
    output: bool,
) -> Result<cpal::Device, String> {
    let kind = if output { "output" } else { "input" };
    match name {
        Some(name) => find_device(host, name, output).ok_or_else(|| {
            format!(
                "no {kind} device matching '{name}' on {}. \
                 Run with --list-devices to see available devices.",
                host.id().name()
            )
        }),
        None => if output {
            host.default_output_device()
        } else {
            host.default_input_device()
        }
        .ok_or_else(|| format!("no default {kind} device on {}", host.id().name())),
    }
}

fn find_device(host: &cpal::Host, name: &str, output: bool) -> Option<cpal::Device> {
    let devices = if output {
        host.output_devices().ok()?
    } else {
        host.input_devices().ok()?
    };
    let name = name.to_lowercase();
    devices
        .into_iter()
        .find(|d| device_name(d).is_some_and(|n| n.to_lowercase().contains(&name)))
}

fn resolve_output<P: PluginExport>(
    host: &cpal::Host,
    req: &AudioRequest,
) -> Result<ResolvedOutput, String> {
    let device = resolve_device(host, req.output_device.as_deref(), true)?;
    let supported = device
        .default_output_config()
        .map_err(|e| format!("could not query the output device's default config: {e}"))?;
    let (_, num_out, _) = layout_at_index::<P>(req.layout);
    let config = resolve_config(
        &device,
        &supported,
        req.sample_rate,
        req.buffer_size,
        u16::try_from(num_out).ok().filter(|&c| c > 0),
    )?;
    Ok(ResolvedOutput {
        device,
        supported,
        config,
    })
}

/// Validate a bus-layout index against the plugin's declared layouts.
fn check_layout<P: PluginExport>(index: usize) -> Result<(), String> {
    let layouts = P::bus_layouts();
    if layouts.is_empty() && index == 0 {
        return Ok(());
    }
    let layout = layouts.get(index).ok_or_else(|| {
        format!(
            "bus layout {index} is not declared (the plugin declares {})",
            layouts.len()
        )
    })?;
    if bus_layout_fits_routing(layout) {
        Ok(())
    } else {
        Err(format!(
            "bus layout {index} exceeds BusRouting's limit of 32 buses per \
             direction and 65,535 channels per bus"
        ))
    }
}

/// `(total_in, total_out, main_in)` for the declared layout at `idx`. The
/// layout index is the unambiguous identity (two layouts can share channel
/// totals but split main/sidechain differently), so both the initial
/// selection and a runtime `SetLayout` switch resolve widths from here.
fn layout_at_index<P: PluginExport>(idx: usize) -> (usize, usize, usize) {
    P::bus_layouts().get(idx).map_or((0, 0, 0), |l| {
        (
            l.total_input_channels() as usize,
            l.total_output_channels() as usize,
            l.inputs
                .first()
                .map_or(0, |b| b.channels.channel_count() as usize),
        )
    })
}

fn bus_routing_at_index<P: PluginExport>(idx: usize) -> BusRouting {
    let mut routing = BusRouting::new();
    let Some(layout) = P::bus_layouts().into_iter().nth(idx) else {
        return routing;
    };
    for bus in layout.inputs {
        let state = if bus.enabled {
            BusActivation::Active
        } else {
            BusActivation::Inactive
        };
        let channels = if bus.enabled {
            bus.channels.channel_count()
        } else {
            0
        };
        assert!(routing.push_input(channels, state));
    }
    for bus in layout.outputs {
        let state = if bus.enabled {
            BusActivation::Active
        } else {
            BusActivation::Inactive
        };
        let channels = if bus.enabled {
            bus.channels.channel_count()
        } else {
            0
        };
        assert!(routing.push_output(channels, state));
    }
    routing
}

/// Whether the output device advertises a config with exactly `ch`
/// channels. Used to reject a `--bus-layout` wider than the hardware.
fn device_supports_output_channels(device: &cpal::Device, ch: u16) -> bool {
    device
        .supported_output_configs()
        .is_ok_and(|mut r| r.any(|c| c.channels() == ch))
}

/// Build the output `StreamConfig` for a request. A layout width the
/// device doesn't offer keeps the device default width (the plugin output
/// is mapped onto it). An explicitly requested sample rate the device
/// doesn't support is an error, so the request fails rather than silently
/// running at another rate.
fn resolve_config(
    device: &cpal::Device,
    default: &cpal::SupportedStreamConfig,
    sample_rate: Option<u32>,
    buffer_size: Option<u32>,
    requested_channels: Option<u16>,
) -> Result<cpal::StreamConfig, String> {
    let mut channels = default.channels();
    if let Some(req) = requested_channels
        && req != channels
    {
        if device_supports_output_channels(device, req) {
            channels = req;
        } else {
            eprintln!(
                "bus-layout: device doesn't offer {req} output channels; \
                 keeping the {channels}-channel device stream and mapping \
                 the plugin's output onto it"
            );
        }
    }
    let sample_rate = match sample_rate {
        None => default.sample_rate(),
        Some(sr) => {
            let supported = device.supported_output_configs().is_ok_and(|mut ranges| {
                ranges.any(|r| r.min_sample_rate() <= sr && r.max_sample_rate() >= sr)
            });
            if !supported {
                return Err(format!("the output device does not support {sr} Hz"));
            }
            sr
        }
    };
    Ok(cpal::StreamConfig {
        channels,
        sample_rate,
        buffer_size: buffer_size.map_or(cpal::BufferSize::Default, cpal::BufferSize::Fixed),
    })
}

fn renew_device_preparation(required: bool, bound_grew: bool, force_reset: bool) -> bool {
    required || bound_grew || force_reset
}

trait BufferSizeMax {
    fn buffer_size_geometry(
        &self,
        supported: &cpal::SupportedStreamConfig,
    ) -> (usize, moose_core::plugin::PreparationFrameBound);
}
fn preparation_buffer_geometry(
    requested: cpal::BufferSize,
    supported: &cpal::SupportedBufferSize,
) -> (usize, moose_core::plugin::PreparationFrameBound) {
    use moose_core::plugin::PreparationFrameBound as Bound;
    match requested {
        cpal::BufferSize::Fixed(n) => (n as usize, Bound::RequestedFixedDevice),
        cpal::BufferSize::Default => match supported {
            cpal::SupportedBufferSize::Range {
                max: max @ 1..=32_768,
                ..
            } => (*max as usize, Bound::AdvertisedDeviceMaximum),
            // Defensive scratch fallback is explicitly NOT a device promise,
            // including unusable Range maxima (WASAPI 0/u32::MAX cases).
            _ => (8192, Bound::UnprovenDeviceFallback),
        },
    }
}
impl BufferSizeMax for cpal::StreamConfig {
    fn buffer_size_geometry(
        &self,
        supported: &cpal::SupportedStreamConfig,
    ) -> (usize, moose_core::plugin::PreparationFrameBound) {
        preparation_buffer_geometry(self.buffer_size, supported.buffer_size())
    }
}

#[cfg(test)]
mod preparation_geometry172_tests {
    use super::*;
    #[test]
    fn legacy_initial_state_is_not_reset_again_but_workers_renew_equal_b_admission() {
        let mut restored_state = 71;
        let mut resets = 1;
        if renew_device_preparation(false, false, false) {
            restored_state = 0;
            resets += 1;
        }
        assert_eq!((restored_state, resets), (71, 1));
        for (old, new) in [
            (
                cpal::SupportedBufferSize::Range { min: 1, max: 8192 },
                cpal::SupportedBufferSize::Unknown,
            ),
            (
                cpal::SupportedBufferSize::Range { min: 1, max: 8192 },
                cpal::SupportedBufferSize::Range { min: 0, max: 0 },
            ),
        ] {
            let (old_b, _) = preparation_buffer_geometry(cpal::BufferSize::Default, &old);
            let (new_b, provenance) = preparation_buffer_geometry(cpal::BufferSize::Default, &new);
            assert_eq!(old_b, new_b);
            assert_eq!(
                provenance,
                moose_core::plugin::PreparationFrameBound::UnprovenDeviceFallback
            );
            assert!(
                renew_device_preparation(true, new_b > old_b, false),
                "worker cannot reuse an equal numeric prior proof"
            );
        }
        assert!(renew_device_preparation(false, true, false));
        assert!(renew_device_preparation(false, false, true));
    }
    #[test]
    fn fallback_is_never_labeled_as_a_proven_device_bound() {
        use moose_core::plugin::PreparationFrameBound as Bound;
        for supported in [
            cpal::SupportedBufferSize::Unknown,
            cpal::SupportedBufferSize::Range { min: 0, max: 0 },
            cpal::SupportedBufferSize::Range {
                min: 0,
                max: u32::MAX,
            },
            cpal::SupportedBufferSize::Range {
                min: 1,
                max: 32_769,
            },
        ] {
            assert_eq!(
                preparation_buffer_geometry(cpal::BufferSize::Default, &supported),
                (8192, Bound::UnprovenDeviceFallback)
            );
        }
        for max in [1, 8192, 32_768] {
            assert_eq!(
                preparation_buffer_geometry(
                    cpal::BufferSize::Default,
                    &cpal::SupportedBufferSize::Range { min: 1, max }
                ),
                (max as usize, Bound::AdvertisedDeviceMaximum)
            );
        }
        assert_eq!(
            preparation_buffer_geometry(
                cpal::BufferSize::Fixed(71),
                &cpal::SupportedBufferSize::Unknown
            ),
            (71, Bound::RequestedFixedDevice)
        );
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn audio_callback<P: PluginExport>(
    data: &mut [f32],
    // Device interleave stride (the cpal stream's channel count). Kept
    // separate from the plugin's bus dimensions so a plugin bus that
    // doesn't match the device (a mono plugin on a stereo-only output, a
    // mono-in/stereo-out effect) maps onto the device channels via the
    // route rather than forcing a device stream at the plugin's width,
    // which the hardware may not offer.
    channels: usize,
    // The active bus layout's exact (input, output) channel counts. Input
    // scratch is sized to `num_in`, output to `num_out`, so an asymmetric
    // layout (1 -> 2, 2 -> 1) runs at its real dimensions instead of a
    // single collapsed width.
    num_in: usize,
    // Channels [0, num_main_in) are the main input bus; [num_main_in,
    // num_in) are the sidechain. The mic and `--input-file` feed the main
    // bus; `--sidechain-file` feeds the sidechain when the plugin has one.
    num_main_in: usize,
    num_out: usize,
    sample_rate: f64,
    is_effect: bool,
    bus_routing: BusRouting,
    plugin: &Arc<Mutex<P>>,
    pending: &Arc<ArrayQueue<MidiEvent>>,
    pending_state: &Arc<ArrayQueue<Vec<u8>>>,
    input_ring: &Arc<InputRing>,
    input_enabled: &Arc<AtomicBool>,
    output_enabled: &Arc<AtomicBool>,
    input_channel_route: &Arc<AtomicUsize>,
    output_channel_route: &Arc<AtomicUsize>,
    transport: &Transport,
    channel_bufs: &mut Vec<Vec<f32>>,
    input_bufs: &mut Vec<Vec<f32>>,
    event_list: &mut EventList,
    output_events: &mut EventList,
    sub_event_scratch: &mut EventList,
    param_infos: &[ParamInfo],
    params_arc: &Arc<P::Params>,
    automation: moose_core::Automation,
    ptr_scratch: &mut CallbackPtrScratch,
    scratch: &mut RawBufferScratch<<P as moose_core::plugin::PluginRuntime>::Sample>,
    #[cfg(feature = "playback")] playback: Option<&Arc<crate::playback::PlaybackSource>>,
    #[cfg(feature = "playback")] sidechain_playback: Option<&Arc<crate::playback::PlaybackSource>>,
    #[cfg(feature = "playback")] capture: Option<&crate::playback::CapturePusher>,
) {
    // `num_main_in` only routes the sidechain when the `playback` feature
    // supplies a file source to feed it.
    #[cfg(not(feature = "playback"))]
    let _ = num_main_in;
    let num_frames = data.len() / channels;

    let Ok(mut plugin) = plugin.try_lock() else {
        data.fill(0.0);
        return;
    };

    // Apply an editor-initiated state load before this block's work, under
    // the `&mut plugin` we now hold. The UI thread only pushed the bytes to
    // the lock-free `pending_state`, so it never blocked us or stalled this
    // callback's `try_lock`. `load_state` legitimately allocates (a sampler
    // decoding, say); accepted here at the block top, the same handoff the
    // VST3 wrapper uses for its queued state.
    if let Some(bytes) = pending_state.pop()
        && let Err(e) = plugin.load_state(&bytes)
    {
        // Debug-only breadcrumb: this runs on the audio thread (the editor's
        // load pops off the handoff queue here), and `eprintln!` locks stderr
        // and allocates, so it must never fire in a release build. Matches
        // `moose_core::state::apply_state`, which defers the same log.
        #[cfg(debug_assertions)]
        eprintln!("moose-standalone: load_state failed: {e}");
        #[cfg(not(debug_assertions))]
        let _ = e;
    }

    // Drain queued MIDI only after the lock is held. A lost try_lock
    // (an editor state read in flight) returns early above, and events
    // drained before it would be wiped by the next callback's
    // `event_list.clear()` - dropped notes. Left in `pending` they
    // arrive one block late at offset 0 instead.
    event_list.clear();
    output_events.clear();
    output_events.clear_overflow();
    while let Some(ev) = pending.pop() {
        event_list.push(Event {
            sample_offset: 0,
            port: ev.port,
            body: ev.body,
        });
    }

    // Re-shape the plugin's output scratch to (num_out, num_frames) and
    // zero it. Preallocated per stream for the frame bound, so this never
    // allocates on the audio thread.
    shape_lanes(channel_bufs, num_out, num_frames);

    // Effect-only input plumbing: mic + file are independent input
    // sources that *sum* into the plugin's input bus (`input_bufs`, sized
    // `num_in`), separate from the output the plugin writes (`channel_bufs`,
    // sized `num_out`). Instruments skip the whole block and just clear
    // `input_bufs`.
    if is_effect {
        // Input scratch: (num_in, num_frames), zeroed, allocation-free
        // within the preallocated frame bound.
        shape_lanes(input_bufs, num_in, num_frames);
        // (1) Mic ring → input_bufs (per-block sum). Pop up to one
        // block of frames off the lock-free ring (each already
        // normalized to the ring width `w`).
        if input_enabled.load(Ordering::Relaxed) {
            // Map device input channels onto the plugin's input bus per
            // the selected route. `Direct` is the 1:1 default (clamped to
            // `num_in`, so a narrower plugin bus drops the extra device
            // channels); `Stereo` pulls a chosen device pair into plugin in
            // 0/1; `Mono` feeds one device channel into both plugin inputs.
            // Bounds checks keep an out-of-range base (e.g. after a device
            // swap to fewer channels) from indexing past the frame.
            let route = ChannelRoute::decode(input_channel_route.load(Ordering::Relaxed));
            let n_in = input_bufs.len();
            let w = input_ring_width(channels, num_in);
            let frames = input_ring.len().min(num_frames);
            // `i` is the frame index: each iteration pops one ring frame
            // and writes it into every `input_bufs` channel at `[i]`, so a
            // range loop (not an iterator) is the clear form.
            #[allow(clippy::needless_range_loop)]
            for i in 0..frames {
                let Some(popped) = input_ring.pop() else {
                    break;
                };
                let frame = &popped.samples[..w];
                match route {
                    ChannelRoute::Direct => {
                        for ch in 0..w.min(n_in) {
                            input_bufs[ch][i] += frame[ch];
                        }
                    }
                    ChannelRoute::Stereo { base } => {
                        if base < w && n_in > 0 {
                            input_bufs[0][i] += frame[base];
                        }
                        if n_in > 1 && base + 1 < w {
                            input_bufs[1][i] += frame[base + 1];
                        }
                    }
                    ChannelRoute::Mono { base } => {
                        if base < w && n_in > 0 {
                            let s = frame[base];
                            input_bufs[0][i] += s;
                            if n_in > 1 {
                                input_bufs[1][i] += s;
                            }
                        }
                    }
                }
            }
        }

        // (2) Playback files → their buses. The main file sums onto the
        // main bus (alongside the mic); the sidechain file feeds the
        // sidechain bus, so the two are independent sources you can drive
        // and test separately.
        #[cfg(feature = "playback")]
        {
            if let Some(src) = playback {
                src.mix_into(&mut input_bufs[..num_main_in], num_frames);
            }
            if let Some(src) = sidechain_playback
                && num_in > num_main_in
            {
                src.mix_into(&mut input_bufs[num_main_in..num_in], num_frames);
            }
        }
    } else {
        input_bufs.clear();
    }
    // Raw-pointer arrays mirroring `input_bufs` / `channel_bufs`, fed through
    // the shared `RawBufferScratch::build` helper (one unsafe path for every host).
    ptr_scratch.fill(input_bufs, channel_bufs);
    let num_frames_u32 = u32::try_from(num_frames).unwrap_or(u32::MAX);
    let num_in_u32 = u32::try_from(ptr_scratch.inputs.len()).unwrap_or(u32::MAX);
    let num_out_u32 = u32::try_from(ptr_scratch.outputs.len()).unwrap_or(u32::MAX);
    // SAFETY: the `*const f32` / `*mut f32` entries above all point
    // into `input_bufs` / `channel_bufs`, both alive for the rest of
    // this call (they're owned by the cpal closure and not mutated
    // until the next block); each `Vec<f32>` was sized to
    // `num_frames` above, satisfying `build`'s readability /
    // writability requirements.
    let mut audio_buffer = unsafe {
        scratch.build(
            ptr_scratch.inputs.as_ptr(),
            ptr_scratch.outputs.as_mut_ptr(),
            num_in_u32,
            num_out_u32,
            num_frames_u32,
            P::supports_in_place(),
        )
    };

    let transport_info = transport.tick_audio(num_frames);
    let mut transport_snap = transport_info;
    let chunk_args = ChunkedProcess {
        events: event_list,
        sub_event_scratch,
        transport: &mut transport_snap,
        sample_rate,
        // The standalone is a live host; offline render goes through
        // `offline.rs` + the driver, not this realtime path.
        process_mode: ProcessMode::Realtime,
        output_events,
        params_fn: None,
        meters_fn: None,
        latency_change_request: None,
        param_infos,
        automation,
    };
    process_chunked_with_bus_routing(
        &mut *plugin,
        params_arc.as_ref() as &dyn Params,
        &mut audio_buffer,
        chunk_args,
        bus_routing,
    );
    let _ = audio_buffer;
    let output_status = output_events.overflow().map_or_else(
        || {
            if output_events.is_empty() {
                OutputEventStatus::Success
            } else {
                OutputEventStatus::Unsupported
            }
        },
        OutputEventStatus::BufferFull,
    );
    output_events.set_output_status(output_status);
    // Narrow rendered f64 output back to host f32 when the plugin's
    // `Sample = f64`. No-op for `f32` plugins.
    // SAFETY: `ptr_scratch.outputs` lives through this function;
    // `num_out_u32` / `num_frames_u32` match the build above.
    unsafe {
        scratch.finish_widening(
            ptr_scratch.outputs.as_mut_ptr(),
            num_out_u32,
            num_frames_u32,
        );
    }

    let route = ChannelRoute::decode(output_channel_route.load(Ordering::Relaxed));
    write_output_to_device(
        data,
        channels,
        num_frames,
        channel_bufs,
        output_enabled.load(Ordering::Relaxed),
        route,
        #[cfg(feature = "playback")]
        capture,
    );
}

/// Map the plugin's rendered output onto the interleaved device buffer
/// `data`. `channel_bufs` holds one `Vec<f32>` per plugin output channel
/// (each `num_frames` long); `channels` is the device interleave stride.
///
/// A layout with no output channels - a MIDI-only instrument *or* an
/// input-only analyzer / sink - leaves `channel_bufs` empty. There is then
/// nothing to route, and every path below indexes `channel_bufs[..]`
/// (`Direct` via `n_buf - 1`, `Stereo` / `Mono` via `[0]` / `[1]`), so the
/// empty case is silenced and returned first. Keeping that guard here, next
/// to the indexing it protects, is deliberate: cpal drives this from an
/// `extern "C"` callback on the OS audio thread, where a `0 - 1` underflow
/// panic (or an empty-`Vec` index) would abort the host process.
///
/// `output_enabled == false` is device mute: the plugin already ran, so the
/// `--output-file` capture copy is still taken (mute is speaker silence, not
/// a bounce cut), then the device buffer is zeroed.
fn write_output_to_device(
    data: &mut [f32],
    channels: usize,
    num_frames: usize,
    channel_bufs: &[Vec<f32>],
    output_enabled: bool,
    route: ChannelRoute,
    #[cfg(feature = "playback")] capture: Option<&crate::playback::CapturePusher>,
) {
    let n_buf = channel_bufs.len();
    if n_buf == 0 {
        data.fill(0.0);
        return;
    }

    // `--output-file` capture: hand a copy of the post-process, pre-mute
    // output to the writer thread. The capture path transfers Vec ownership
    // to the writer thread (channel-bounded `mpsc::sync_channel`), so the
    // per-block alloc here can't be amortized without a free-list pool. Left
    // as-is - `--output-file` is the offline render/capture path (often
    // paired with `--no-playback`), not the real-time playback hot path.
    #[cfg(feature = "playback")]
    if let Some(pusher) = capture {
        let mut interleaved = vec![0.0_f32; num_frames * channels];
        for frame in 0..num_frames {
            for ch in 0..channels {
                let ch_idx = ch.min(n_buf - 1);
                interleaved[frame * channels + ch] = channel_bufs[ch_idx][frame];
            }
        }
        pusher.submit(interleaved);
    }

    // Output mute: keep the plugin running (transport, MIDI, meters all still
    // tick) but zero-fill the device buffer so the speakers stay silent.
    // Cheaper and more responsive than tearing down the cpal stream.
    if !output_enabled {
        data.fill(0.0);
        return;
    }

    // Map the plugin's output bus onto device output channels per the
    // selected route. `Direct` is the 1:1 default; `Stereo` drives a chosen
    // device pair (other channels silent); `Mono` folds the plugin's first
    // two channels down into one device channel. The non-Direct modes clear
    // the buffer first since they only write the selected channels.
    match route {
        ChannelRoute::Direct => {
            for frame in 0..num_frames {
                for ch in 0..channels {
                    let ch_idx = ch.min(n_buf - 1);
                    data[frame * channels + ch] = channel_bufs[ch_idx][frame];
                }
            }
        }
        ChannelRoute::Stereo { base } => {
            data.fill(0.0);
            for frame in 0..num_frames {
                if base < channels {
                    data[frame * channels + base] = channel_bufs[0][frame];
                }
                if n_buf > 1 && base + 1 < channels {
                    data[frame * channels + base + 1] = channel_bufs[1][frame];
                }
            }
        }
        ChannelRoute::Mono { base } => {
            data.fill(0.0);
            if base < channels {
                for frame in 0..num_frames {
                    let mut v = channel_bufs[0][frame];
                    if n_buf > 1 {
                        v += channel_bufs[1][frame];
                    }
                    data[frame * channels + base] = v;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ChannelRoute, write_output_to_device};

    /// Call `write_output_to_device` the same way `audio_callback` does,
    /// threading the `playback`-gated `capture` arg through so the test
    /// compiles in both feature configs.
    fn route(
        data: &mut [f32],
        channels: usize,
        num_frames: usize,
        channel_bufs: &[Vec<f32>],
        output_enabled: bool,
        route: ChannelRoute,
    ) {
        write_output_to_device(
            data,
            channels,
            num_frames,
            channel_bufs,
            output_enabled,
            route,
            #[cfg(feature = "playback")]
            None,
        );
    }

    #[test]
    fn zero_output_layout_silences_without_underflow() {
        // A MIDI-only instrument or an input-only analyzer / sink declares no
        // output buses, so `channel_bufs` is empty while the device stream
        // still asks for `channels` frames. Routing must not compute
        // `0usize - 1` or index an empty buffer (either aborts the cpal audio
        // thread); it silences the device and returns.
        let channels = 2;
        let num_frames = 4;
        let channel_bufs: Vec<Vec<f32>> = Vec::new();
        for &r in &[
            ChannelRoute::Direct,
            ChannelRoute::Stereo { base: 0 },
            ChannelRoute::Mono { base: 0 },
        ] {
            let mut data = vec![7.0_f32; num_frames * channels];
            route(&mut data, channels, num_frames, &channel_bufs, true, r);
            assert!(
                data.iter().all(|&s| s == 0.0),
                "zero-output layout must silence the device buffer ({r:?})"
            );
        }
    }

    #[test]
    fn direct_route_maps_and_clamps_channels() {
        let channels = 2;
        let num_frames = 2;
        // Mono plugin output on a stereo device: `ch.min(n_buf - 1)` folds
        // both device channels onto the single plugin channel.
        let mono = vec![vec![0.25_f32, 0.5]];
        let mut data = vec![0.0_f32; num_frames * channels];
        route(
            &mut data,
            channels,
            num_frames,
            &mono,
            true,
            ChannelRoute::Direct,
        );
        assert_eq!(data, vec![0.25, 0.25, 0.5, 0.5]);

        // Stereo plugin output maps 1:1.
        let stereo = vec![vec![0.1_f32, 0.2], vec![0.3, 0.4]];
        let mut data = vec![0.0_f32; num_frames * channels];
        route(
            &mut data,
            channels,
            num_frames,
            &stereo,
            true,
            ChannelRoute::Direct,
        );
        assert_eq!(data, vec![0.1, 0.3, 0.2, 0.4]);
    }

    #[test]
    fn muted_output_zeroes_device_buffer() {
        let channels = 2;
        let num_frames = 2;
        let stereo = vec![vec![0.1_f32, 0.2], vec![0.3, 0.4]];
        let mut data = vec![9.0_f32; num_frames * channels];
        route(
            &mut data,
            channels,
            num_frames,
            &stereo,
            false,
            ChannelRoute::Direct,
        );
        assert!(data.iter().all(|&s| s == 0.0), "mute silences the device");
    }

    #[test]
    fn encode_decode_roundtrips() {
        let cases = [
            ChannelRoute::Direct,
            ChannelRoute::Stereo { base: 0 },
            ChannelRoute::Stereo { base: 2 },
            ChannelRoute::Mono { base: 0 },
            ChannelRoute::Mono { base: 3 },
        ];
        for route in cases {
            assert_eq!(ChannelRoute::decode(route.encode()), route);
        }
    }

    #[test]
    fn direct_encodes_to_zero() {
        // A freshly-zeroed atomic must decode to the default mapping.
        assert_eq!(ChannelRoute::Direct.encode(), 0);
        assert_eq!(ChannelRoute::decode(0), ChannelRoute::Direct);
    }

    #[test]
    fn parse_specs() {
        assert_eq!(ChannelRoute::parse("direct"), Some(ChannelRoute::Direct));
        assert_eq!(ChannelRoute::parse(" ALL "), Some(ChannelRoute::Direct));
        // 1-based in the spec, 0-based base internally.
        assert_eq!(
            ChannelRoute::parse("1"),
            Some(ChannelRoute::Mono { base: 0 })
        );
        assert_eq!(
            ChannelRoute::parse("3"),
            Some(ChannelRoute::Mono { base: 2 })
        );
        assert_eq!(
            ChannelRoute::parse("3-4"),
            Some(ChannelRoute::Stereo { base: 2 })
        );
        // Non-adjacent / reversed / zero / garbage are rejected.
        assert_eq!(ChannelRoute::parse("3-5"), None);
        assert_eq!(ChannelRoute::parse("4-3"), None);
        assert_eq!(ChannelRoute::parse("0"), None);
        assert_eq!(ChannelRoute::parse("x"), None);
    }
}

#[cfg(test)]
mod host_control_tests {
    use super::*;

    #[test]
    fn audio_api_names_are_validated() {
        let err = parse_audio_api(Some("no-such-api")).unwrap_err();
        assert!(err.contains("available:"), "{err}");
        let default = cpal::default_host().id();
        assert_eq!(
            parse_audio_api(Some(&default.name().to_uppercase())),
            Ok(Some(default)),
            "case-insensitive match"
        );
        assert_eq!(parse_audio_api(None), Ok(None));
    }

    fn patch(args: &[&str]) -> Result<AudioPatch, String> {
        AudioPatch::from_options(&crate::cli::parse_from(
            std::iter::once("app").chain(args.iter().copied()),
        )?)
    }

    #[test]
    fn requests_are_validated_before_anything_applies() {
        for (args, flag) in [
            (&["--output-channels", "3-7"][..], "--output-channels"),
            (&["--input-channels", "4097"], "--input-channels"),
            (&["--sample-rate", "0"], "--sample-rate"),
            (&["--buffer", "0"], "--buffer"),
            (&["--buffer", "65536"], "--buffer"),
            (&["--audio-api", "no-such-api"], "no-such-api"),
        ] {
            let err = patch(args).unwrap_err();
            assert!(err.contains(flag), "{args:?}: {err}");
        }
        let ok = patch(&[
            "--sample-rate",
            "96000",
            "--buffer",
            "64",
            "--bus-layout",
            "1",
        ])
        .unwrap();
        let next = AudioRequest::default().patched(&ok);
        assert_eq!(
            (next.sample_rate, next.buffer_size, next.layout),
            (Some(96_000), Some(64), 1)
        );
        // Omitted rate / buffer / layout / input keep the running values;
        // the API and devices always apply (absent = default).
        let running = AudioRequest {
            output_device: Some("old".into()),
            input_enabled: true,
            ..next.clone()
        };
        let kept = running.patched(&AudioPatch::default());
        assert_eq!(kept.output_device, None);
        assert_eq!(
            (
                kept.sample_rate,
                kept.buffer_size,
                kept.layout,
                kept.input_enabled
            ),
            (Some(96_000), Some(64), 1, true)
        );
    }

    #[test]
    fn live_layout_switch_reopens_capture_at_the_target_width() {
        // A switch from a stereo to a 16-in layout changes the output
        // geometry, so the worker reopens output and capture, and capture
        // opens at the target ring width: all 16 ports, not the old 2.
        let stereo = AudioRequest::default();
        let wide = AudioRequest {
            layout: 1,
            ..stereo.clone()
        };
        assert!(!stereo.same_output(&wide));
        assert_eq!(input_ring_width(2, 2), 2);
        let width = input_ring_width(2, 16);
        assert_eq!(width, 16);
        let frame: Vec<f32> = (0..16u8).map(f32::from).collect();
        assert_eq!(normalize_input_frame(&frame, width).samples[15], 15.0);
        // An input-only change keeps the output stream.
        let mic = AudioRequest {
            input_enabled: true,
            input_device: Some("mic".into()),
            ..stereo.clone()
        };
        assert!(stereo.same_output(&mic));
    }

    #[test]
    fn sessions_are_exclusive_and_release_only_their_own() {
        // One test: the session slot is process-global.
        assert_eq!(
            reopen_audio(["app"]).unwrap_err(),
            "no standalone audio is running"
        );
        assert!(
            reopen_audio(["app", "--bogus"])
                .unwrap_err()
                .contains("unknown arguments")
        );
        let first = LiveAudio::reserve().expect("first session");
        assert!(LiveAudio::reserve().is_err(), "second session is refused");
        // A stale guard from another session never clears this one.
        drop(LiveAudio {
            id: first.id + 1000,
        });
        assert!(LiveAudio::reserve().is_err());
        let (tx, rx) = mpsc::channel();
        let engine = Engine {
            tx,
            check_layout: |i| {
                if i == 0 {
                    Ok(())
                } else {
                    Err("no layout".into())
                }
            },
            api: Arc::new(Mutex::new(None)),
        };
        first.register(engine.clone());
        assert_eq!(
            reopen_audio(["app", "--bus-layout", "3"]).unwrap_err(),
            "no layout"
        );
        reopen_audio(["app", "--output-channels", "3-4"]).expect("submitted");
        assert!(matches!(
            rx.try_recv(),
            Ok(EngineCmd::Reconfigure(AudioPatch {
                output_route: Some(ChannelRoute::Stereo { base: 2 }),
                ..
            }))
        ));
        // A stopped worker reaches callers as an error.
        drop(rx);
        assert_eq!(reopen_audio(["app"]).unwrap_err(), WORKER_GONE.to_string());
        assert_eq!(
            engine.send(EngineCmd::InputEnabled(true)).unwrap_err(),
            WORKER_GONE
        );
        drop(first);
        drop(LiveAudio::reserve().expect("released"));
    }

    #[test]
    fn audio_opened_callback_receives_results_and_panics_are_contained() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        on_audio_opened({
            let seen = Arc::clone(&seen);
            move |r| seen.lock().unwrap().push(r)
        });
        let opened = AudioOpened {
            api: "JACK",
            output_device: None,
            sample_rate: 48_000,
            buffer_size: Some(128),
            output_channels: 2,
            input_channels: Some(16),
            bus_layout: 1,
        };
        notify_audio_opened(Ok(opened.clone()));
        notify_audio_opened(Err("busy".into()));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Ok(opened), Err("busy".to_string())]
        );
        on_audio_opened(|_| panic!("observer bug"));
        notify_audio_opened(Err("contained".into()));
        *lock(&AUDIO_OPENED) = None;
    }

    #[test]
    fn capture_opens_enough_ports_for_the_target_layout() {
        // cpal 0.17.1's JACK host advertises these port counts.
        let jack = [1, 2, 4, 6, 8, 16, 24, 32, 48, 64];
        assert_eq!(capture_channels(&jack, 2), Some(2));
        assert_eq!(capture_channels(&jack, 16), Some(16), "2 -> 16 switch");
        assert_eq!(capture_channels(&jack, 15), Some(16), "16 -> 15 switch");
        assert_eq!(capture_channels(&[1, 2], 16), None, "falls back to default");
        // The extra 16th port is dropped at the 15-lane ring width.
        let frame: Vec<f32> = (0..16u8).map(f32::from).collect();
        let lanes = normalize_input_frame(&frame, 15).samples;
        assert_eq!(lanes[14], 14.0);
        assert_eq!(lanes[15], 0.0);
    }

    #[test]
    fn api_names_match_the_documented_spelling() {
        for id in cpal::ALL_HOSTS {
            let name = api_name(*id);
            if id.name().eq_ignore_ascii_case("jack") {
                assert_eq!(name, "JACK");
            } else {
                assert_eq!(name, id.name());
            }
        }
        #[cfg(all(feature = "jack", target_os = "linux"))]
        assert!(cpal::ALL_HOSTS.iter().any(|id| api_name(*id) == "JACK"));
    }

    #[test]
    fn ring_width_covers_wide_plugin_inputs() {
        assert_eq!(input_ring_width(2, 16), 16);
        assert_eq!(input_ring_width(8, 2), 8);
        assert_eq!(input_ring_width(2, 64), MAX_INPUT_RING_CHANNELS);
    }

    #[test]
    fn routes_reject_unrepresentable_channels() {
        assert_eq!(ChannelRoute::parse("4097"), None);
        assert_eq!(ChannelRoute::parse("4096-4097"), None);
        assert_eq!(ChannelRoute::parse(&usize::MAX.to_string()), None);
        assert_eq!(
            ChannelRoute::parse(&format!("{}-{}", usize::MAX, usize::MAX)),
            None
        );
        let top = ChannelRoute::parse("4096").unwrap();
        assert_eq!(ChannelRoute::decode(top.encode()), top);
        // Out-of-range bases built directly clamp instead of overflowing.
        assert_eq!(
            ChannelRoute::decode(ChannelRoute::Mono { base: usize::MAX }.encode()),
            ChannelRoute::Mono {
                base: MAX_ROUTE_CHANNEL - 1
            }
        );
    }

    #[test]
    fn callback_scratch_is_preallocated_for_the_bound() {
        let bound = 512;
        let mut out = preallocated_lanes(16, bound);
        let mut inp = preallocated_lanes(16, bound);
        let mut ptrs = CallbackPtrScratch {
            inputs: Vec::with_capacity(16),
            outputs: Vec::with_capacity(16),
        };
        let ((), allocations) = moose_core::rt::audit(|| {
            let _section = moose_core::rt::RtSection::enter();
            for frames in [bound, 64, bound] {
                shape_lanes(&mut out, 16, frames);
                shape_lanes(&mut inp, 16, frames);
                ptrs.fill(&inp, &mut out);
            }
        });
        assert_eq!(allocations, 0, "first blocks must not allocate");
        assert!(out.iter().all(|b| b.len() == bound));
    }

    #[test]
    fn standalone_identity() {
        let host = standalone_host();
        assert_eq!(host.format, moose_core::info::HostFormat::Standalone);
        assert_eq!(host.name.as_deref(), Some("moose-standalone"));
    }
}

/// `rt-paranoid` test builds count allocations made inside audio sections.
#[cfg(all(test, feature = "rt-paranoid"))]
#[global_allocator]
static RT_CHECK_ALLOC: moose_core::rt::RtCheckAlloc = moose_core::rt::RtCheckAlloc::new();
