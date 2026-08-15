//! Core update engine.
//!
//! The core consumes normalized provider models and talks to server
//! controllers only through the [`crate::controllers`] interface. It never
//! knows how CurseForge, AMP, or any other integration works.

pub mod executor;
pub mod ignore;
pub mod overlay;
pub mod ownership;
pub mod planner;
pub mod snapshot;
pub mod staging;
pub mod state;
pub mod updater;
pub mod validation;
