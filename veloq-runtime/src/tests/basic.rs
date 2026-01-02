//! Basic runtime tests for spawn and spawn_local functionality.

use crate::runtime::executor::{LocalExecutor, Runtime};
use crate::{spawn, spawn_local};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

// ============ LocalExecutor Tests (Single Threaded) ============

/// Test that spawn_local works correctly in a basic LocalExecutor.
/// This verifies that tasks are executed on the same thread.
#[test]
fn test_spawn_local_basic() {
    let exec = LocalExecutor::new();
    let result = Rc::new(RefCell::new(0));
    let result_clone = result.clone();

    exec.block_on(async move {
        let handle = spawn_local(async move {
            *result_clone.borrow_mut() = 42;
            "done"
        });

        assert_eq!(handle.await, "done");
    });

    assert_eq!(*result.borrow(), 42);
}

/// Test that spawn_local supports !Send futures (like Rc).
#[test]
fn test_spawn_local_not_send() {
    let exec = LocalExecutor::new();
    // Rc is !Send
    let data = Rc::new(vec![1, 2, 3]);
    let data_clone = data.clone();

    exec.block_on(async move {
        // This would fail to compile with spawn()
        let handle = spawn_local(async move {
            assert_eq!(data_clone.len(), 3);
            data_clone[0] + data_clone[1] + data_clone[2]
        });

        assert_eq!(handle.await, 6);
    });
}

/// Test nested spawn_local calls.
#[test]
fn test_nested_spawn_local() {
    let exec = LocalExecutor::new();
    let counter = Rc::new(RefCell::new(0));
    let c1 = counter.clone();

    exec.block_on(async move {
        let h1 = spawn_local(async move {
            *c1.borrow_mut() += 1;
            let c2 = c1.clone();
            
            let h2 = spawn_local(async move {
                *c2.borrow_mut() += 10;
            });
            h2.await;
        });
        h1.await;
    });

    assert_eq!(*counter.borrow(), 11);
}

// ============ Runtime Tests (Multi-Threaded) ============

/// Test global spawn works from within the Runtime (injecting into workers).
#[test]
fn test_runtime_global_spawn() {
    let mut runtime = Runtime::new(crate::config::Config::default());
    let (tx, rx) = std::sync::mpsc::channel();

    // Spawn 1 worker that stays alive
    runtime.spawn_worker(move || async move {
        // Keep alive for a bit to allow receiving tasks
        let mut i = 0;
        while i < 10 {
            crate::runtime::context::yield_now().await;
            std::thread::sleep(std::time::Duration::from_millis(10));
            i += 1;
        }
    });

    // Spawn a task globally from the main thread
    let _handle = runtime.spawn(async move {
        42
    });

    // We can't await the handle directly in a blocking test without blocking on the runtime,
    // but the runtime handle allows us to await it if we were in an async context.
    // Here we'll just block_on_all which runs the workers.
    // Ideally, we want to know if the task ran. 
    
    // Let's spawn a task that sends a signal.
    runtime.spawn(async move {
        tx.send(true).unwrap();
    });

    runtime.block_on_all();
    assert!(rx.recv().unwrap());
}

/// Test global spawn works from INSIDE a worker.
#[test]
fn test_spawn_from_worker() {
    let mut runtime = Runtime::new(crate::config::Config::default());
    let (tx, rx) = std::sync::mpsc::channel();

    runtime.spawn_worker(move || async move {
        // We are inside a worker, so we should have a Spawner in context.
        let handle = spawn(async move {
            "hello from global"
        });
        
        // Wait for it
        let res = handle.await;
        tx.send(res).unwrap();
    });

    runtime.block_on_all();
    assert_eq!(rx.recv().unwrap(), "hello from global");
}

/// Test using both spawn_local and spawn in a worker.
#[test]
fn test_mixed_spawn_in_worker() {
    let mut runtime = Runtime::new(crate::config::Config::default());
    let (tx, rx) = std::sync::mpsc::channel();

    runtime.spawn_worker(move || async move {
        // 1. spawn_local (!Send)
        let rc_val = Rc::new(5);
        let rc_clone = rc_val.clone();
        let local_handle = spawn_local(async move {
            *rc_clone * 2
        });

        // 2. spawn (Send)
        let global_handle = spawn(async move {
            20
        });

        let v1 = local_handle.await;
        let v2 = global_handle.await;

        tx.send(v1 + v2).unwrap();
    });

    runtime.block_on_all();
    assert_eq!(rx.recv().unwrap(), 10 + 20);
}

/// Test that tasks can float between workers (basic check).
/// If we have 2 workers, and one spawns many tasks, they *should* ideally be distributed,
/// but since the current implementation is likely a simple injector or FIFO, 
/// we just verify they all run, and that we can use multiple workers.
#[test]
fn test_multi_worker_throughput() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mut runtime = Runtime::new(crate::config::Config::default());
    let counter = Arc::new(AtomicUsize::new(0));
    
    // Spawn 2 workers that process tasks until done
    let c_worker = counter.clone();
    for _ in 0..2 {
        let c = c_worker.clone();
        runtime.spawn_worker(move || async move {
             let start = std::time::Instant::now();
             // Run until we see 50 tasks done or timeout
             while c.load(Ordering::SeqCst) < 50 {
                 if start.elapsed() > std::time::Duration::from_secs(5) {
                     break;
                 }
                 crate::runtime::context::yield_now().await;
             }
        });
    }

    // Spawn 50 global tasks
    for _ in 0..50 {
        let c = counter.clone();
        runtime.spawn(async move {
            c.fetch_add(1, Ordering::SeqCst);
        });
    }

    runtime.block_on_all();
    
    let final_count = counter.load(Ordering::SeqCst);
    assert_eq!(final_count, 50);
    println!("Processed {} tasks", final_count);
}
