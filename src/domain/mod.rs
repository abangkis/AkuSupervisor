//! Pure lifecycle types and invariants.
//!
//! Roadmap Phase 1 adds the service state machine, actor policy, operator hold,
//! configuration fingerprint, and canonical errors here.

mod control;
mod lifecycle;

pub use control::{
    Actor, AuthorizationError, ControlPolicy, LifecycleAction, OperatorHold, Reason,
};
pub use lifecycle::{DesiredState, LifecycleState, TransitionError};
