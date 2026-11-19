//! Statefulness axis - whether the scaffolded `moose::Plugin` has a
//! processor struct of its own (`--stateful`) or none (`--pure`,
//! `type Processor = ()`). Drives the descriptor/processor structs, the
//! `type Processor` line, and `activate` in the scaffold's lib.rs.

/// How the scaffold's `impl Plugin` carries DSP state.
#[derive(Clone, Copy, PartialEq)]
pub enum Statefulness {
    /// `type Processor = ()` - nothing to build in `activate`, nothing to
    /// pass to `process` but `&mut ()`. For a params-only effect.
    Pure,
    /// A separate `<Name>Processor` struct holding the DSP state, built in
    /// `activate` and passed to `process` as `&mut Self::Processor`. The
    /// default: most plugins grow DSP state, so the plumbing is pre-wired.
    Stateful,
}

impl Statefulness {
    /// Doc comment emitted immediately above the descriptor struct.
    /// Always ends in a newline so the struct starts its own line.
    #[must_use]
    pub fn descriptor_block(self) -> String {
        match self {
            Self::Pure => PURE_DESCRIPTOR.to_string(),
            Self::Stateful => STATEFUL_DESCRIPTOR.to_string(),
        }
    }

    /// The struct declarations. A pure plugin is a unit descriptor; a
    /// stateful one adds a `<Name>Processor` struct carrying `fields`.
    /// No trailing newline - the template supplies it.
    #[must_use]
    pub fn struct_decl(self, struct_name: &str, fields: &str) -> String {
        match self {
            Self::Pure => format!("pub struct {struct_name};"),
            Self::Stateful => STATEFUL_STRUCT
                .replace("{struct_name}", struct_name)
                .replace("{fields}", fields),
        }
    }

    /// The `type Processor = ...;` right-hand side.
    #[must_use]
    pub fn processor_type(self, struct_name: &str) -> String {
        match self {
            Self::Pure => "()".to_string(),
            Self::Stateful => format!("{struct_name}Processor"),
        }
    }
}

const PURE_DESCRIPTOR: &str = "\
// Stateless descriptor (`type Processor = ()`). When your DSP needs
// per-instance state (filters, voices, phase), scaffold with
// `--stateful` - or add a processor struct, set `type Processor` to it,
// build it in `activate`, and take it as the first `process` argument.
";

const STATEFUL_DESCRIPTOR: &str = "\
// The plugin struct is a stateless descriptor; its DSP state lives in the
// processor below, which `activate` builds when the host activates.
";

const STATEFUL_STRUCT: &str = "\
pub struct {struct_name};

/// The audio processor: per-instance DSP state - filters, delay lines,
/// phase counters, parameter ramps. Size buffers in `activate` (it may
/// allocate); `process` and `reset` run on the audio thread and must not.
pub struct {struct_name}Processor {
{fields}}";
