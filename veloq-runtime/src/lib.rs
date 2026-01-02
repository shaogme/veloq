pub mod macros;
pub mod net;
pub mod io;
pub mod runtime;
pub mod config;

// Re-export key functions for convenient access
pub use runtime::{current_buffer_pool, current_driver, spawn, spawn_local, yield_now};
pub use runtime::{LocalExecutor, Runtime}; // Export Runtime for config usage
pub use runtime::{LocalJoinHandle, JoinHandle};


#[cfg(test)]
mod tests {
    mod basic;
    mod select_test;
    mod tcp;
    mod udp;
    mod fs;
}
