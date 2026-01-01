pub mod macros;
pub mod net;
pub mod io;
pub mod runtime;

// Re-export key functions for convenient access
pub use runtime::{current_buffer_pool, current_driver, spawn, yield_now};
pub use runtime::LocalExecutor;
pub use runtime::JoinHandle;


#[cfg(test)]
mod tests {
    mod basic;
    mod select_test;
    mod tcp;
    mod udp;
}
