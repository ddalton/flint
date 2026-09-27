//! tlc-rs: an explicit-state model checker for a subset of TLA+.

pub mod ast;
pub mod check;
pub mod cli;
pub mod closure;
pub mod codegen;
pub mod compile;
pub mod eval;
pub mod lexer;
pub mod liveness;
pub mod parser;
pub mod store;
pub mod symkey;
pub mod value;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
