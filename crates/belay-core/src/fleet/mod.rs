//! Fleets: belay sessions on one machine that know about each other (F1) and run as a
//! team the user describes (F3b). See the guide's Fleets section.

mod awareness;
pub mod model;
pub mod scope;
pub mod spec;

pub use awareness::*;
