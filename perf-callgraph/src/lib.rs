//! Loading: address spaces, unwinding, symbolization and the Profile model.
pub mod addrspace;
pub mod binary;
pub mod events;
pub mod load;
#[cfg(test)]
mod load_replay;
pub mod model;
pub mod phys;
pub mod symbolize;
pub mod tries;
pub mod unwind;

#[cfg(test)]
mod oracles;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
