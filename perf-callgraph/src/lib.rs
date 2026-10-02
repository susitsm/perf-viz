//! Loading a recording into a [`model::Profile`]: address spaces, unwinding, symbolization and the
//! call-tree model. [`load::load`] is the entry point; everything else is internal.

pub mod index;
pub mod load;
pub mod model;
mod symbolizing;
mod unwinding;

#[cfg(test)]
mod oracles;

pub use symbolizing::DwarfMode;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
