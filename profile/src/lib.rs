//! Loading: address spaces, unwinding, symbolization and the Profile model.
pub mod addrspace;
pub mod binary;
pub mod cli;
pub mod model;
pub mod load;
pub mod load_replay;
pub mod load_two_pass;
pub mod meta;
pub mod phys;
pub mod symbolize;
pub mod unwind;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
