//! 状态管理 — State, WorkflowState, StateKey, StateLens。

pub mod state;
pub mod state_lens;
pub mod statekey;
pub mod workflow_state;

pub use state::*;
pub use statekey::*;
// NOTE: workflow_state::StateMutation trait is NOT re-exported here
// to avoid ambiguity with state::StateMutation enum.
// Access it via crate::state::workflow_state::StateMutation.
pub use state_lens::*;
pub use workflow_state::{LastWriteWins, MergeStrategy, WorkflowError, WorkflowState};
