//! Loading a recording into a [`model::Profile`]: address spaces, unwinding, symbolization and the
//! call-tree model. [`load::load`] is the entry point; everything else is internal.

mod addrspace;
mod binary;
mod events;
pub mod load;
#[cfg(test)]
mod load_replay;
pub mod model;
mod phys;
mod symbolize;
mod tries;
mod unwind;

#[cfg(test)]
mod oracles;

pub use symbolize::DwarfMode;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
