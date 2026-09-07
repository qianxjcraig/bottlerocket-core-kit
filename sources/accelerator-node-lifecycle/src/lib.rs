//! Transaction state and durable persistence for Bottlerocket accelerator nodes.
//!
//! This crate deliberately contains no Kubernetes or NVIDIA operations. It defines the
//! transaction contract those adapters must follow so a node can resume safely after a process
//! restart or reboot.

mod locked_store;
mod state;
mod store;

pub use locked_store::{LockedJsonFileStore, LockedJsonFileStoreGuard, LockedStoreError};
pub use state::{
    AcceleratorProfile, LifecycleError, LifecycleState, NextAction, TransitionId,
    TransitionOutcome, TransitionPhase, TransitionResult,
};
pub use store::{JsonFileStore, StoreError};
