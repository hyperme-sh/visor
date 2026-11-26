//! Map Linux desktop signals to the common companion pack vocabulary.

use hypermesh_visor::companion_packs::State;
pub use hypermesh_visor::companion_packs::{valid_key, ClipSpec, Pack, Signals};

use crate::auth::Pose;
use crate::mascot::Mascot;

pub fn signals<'a>(
    mascot: Mascot,
    paused: bool,
    pose: &'a Pose,
    mood: Option<&'a str>,
    vibe: Option<&'a str>,
) -> Signals<'a> {
    let state = match mascot {
        Mascot::Upgrading => State::Upgrading,
        Mascot::Disconnected => State::Disconnected,
        _ if paused => State::Paused,
        Mascot::Automation => State::Automation,
        Mascot::Sleeping => State::Sleeping,
        Mascot::Idle => State::Idle,
    };
    let (verb, mode) = match pose {
        Pose::Active { verb, mode, .. } => (verb.as_deref(), mode.as_deref()),
        Pose::Idle => (None, None),
    };
    Signals::new(state, verb, mode, mood, vibe)
}
