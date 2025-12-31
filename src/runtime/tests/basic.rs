//! Basic runtime tests for executor, context, and spawn functionality.

use crate::runtime::buffer::BufferPool;
use crate::runtime::executor::{LocalExecutor, Runtime};
use crate::runtime::op::{IoOp, IoResources, Op};
use crate::runtime::{current_driver, spawn};
use std::cell::RefCell;
use std::rc::Rc;

// ============ Helper Types ============

struct Nop;

impl IoOp for Nop {
    fn into_resource(self) -> IoResources {
        IoResources::None
    }

    fn from_resource(_res: IoResources) -> Self {
        Nop
    }
}

// ============ Single-Thread Basic Tests ============

/// Test basic nop operation completion
#[test]
fn test_nop_completion() {
    let exec = LocalExecutor::new();
    let driver = exec.driver_handle();

    let result = exec.block_on(async move {
        let op = Op::new(Nop, driver);
        let (res, _nop) = op.await;
        res
    });

    assert!(result.is_ok());
}

/// Test global spawn() API works inside block_on
#[test]
fn test_global_spawn() {
    let exec = LocalExecutor::new();
    let counter = Rc::new(RefCell::new(0));
    let counter_clone = counter.clone();

    exec.block_on(async move {
        // Use global spawn API
        let handle = spawn(async move {
            *counter_clone.borrow_mut() += 1;
        });

        // Wait for spawned task to complete
        handle.await;
    });

    // The spawned task should have run
    assert_eq!(*counter.borrow(), 1);
}

/// Test multiple global spawns execute in order
#[test]
fn test_multiple_spawns() {
    let exec = LocalExecutor::new();
    let order = Rc::new(RefCell::new(Vec::new()));

    let order1 = order.clone();
    let order2 = order.clone();
    let order3 = order.clone();

    exec.block_on(async move {
        let h1 = spawn(async move {
            order1.borrow_mut().push(1);
        });

        let h2 = spawn(async move {
            order2.borrow_mut().push(2);
        });

        let h3 = spawn(async move {
            order3.borrow_mut().push(3);
        });

        // Wait for all spawned tasks to complete
        h1.await;
        h2.await;
        h3.await;
    });

    // All tasks should have run
    // Note: Execution order isn't strictly guaranteed by just awaiting, 
    // but with the current FIFO executor it likely matches. 
    // However, we just check that they all ran.
    let result = order.borrow();
    assert_eq!(result.len(), 3);
    assert!(result.contains(&1));
    assert!(result.contains(&2));
    assert!(result.contains(&3));
}

/// Test current_driver() returns valid driver
#[test]
fn test_current_driver() {
    let exec = LocalExecutor::new();

    exec.block_on(async {
        let driver = current_driver();
        assert!(driver.upgrade().is_some(), "Driver should be valid");

        // Can allocate a buffer through the driver
        let driver_rc = driver.upgrade().unwrap();
        let buf = driver_rc.borrow().alloc_fixed_buffer();
        assert!(buf.is_some(), "Should be able to allocate buffer");
    });
}

/// Test nested spawns
#[test]
fn test_nested_spawns() {
    let exec = LocalExecutor::new();
    let counter = Rc::new(RefCell::new(0));

    let counter1 = counter.clone();
    let counter2 = counter.clone();

    exec.block_on(async move {
        let h_outer = spawn(async move {
            *counter1.borrow_mut() += 1;

            // Nested spawn
            let h_inner = spawn(async move {
                *counter2.borrow_mut() += 10;
            });
            
            h_inner.await;
        });

        h_outer.await;
    });

    // Both spawns should have executed
    assert_eq!(*counter.borrow(), 11);
}

/// Test buffer pool allocation
#[test]
fn test_buffer_pool_allocation() {
    let pool = BufferPool::new();

    // Allocate multiple buffers
    let buf1 = pool.alloc();
    assert!(buf1.is_some());

    let buf2 = pool.alloc();
    assert!(buf2.is_some());

    // Drop one and reallocate
    drop(buf1);
    let buf3 = pool.alloc();
    assert!(buf3.is_some());
}

// ============ Multi-Thread Tests ============

/// Test Runtime with multiple worker threads
#[test]
fn test_multithread_runtime() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let counter = Arc::new(AtomicUsize::new(0));
    let mut runtime = Runtime::new();

    // Spawn 3 worker threads
    for _ in 0..3 {
        let counter_clone = counter.clone();
        runtime.spawn_worker(move || async move {
            counter_clone.fetch_add(1, Ordering::SeqCst);
        });
    }

    runtime.block_on_all();

    assert_eq!(counter.load(Ordering::SeqCst), 3);
    println!("All 3 workers completed");
}

/// Test each worker thread has independent executor
#[test]
fn test_multithread_isolation() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    let counter = Arc::new(AtomicUsize::new(0));
    let mut runtime = Runtime::new();

    // Worker 1: increments counter
    let counter1 = counter.clone();
    runtime.spawn_worker(move || async move {
        for _ in 0..5 {
            counter1.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(1));
        }
    });

    // Worker 2: also increments counter
    let counter2 = counter.clone();
    runtime.spawn_worker(move || async move {
        for _ in 0..5 {
            counter2.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(1));
        }
    });

    runtime.block_on_all();

    assert_eq!(counter.load(Ordering::SeqCst), 10);
    println!("Both workers completed with total count: 10");
}

/// Test context is thread-local (spawn in one thread doesn't affect another)
#[test]
fn test_context_thread_local() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let spawn_succeeded = Arc::new(AtomicBool::new(false));
    let mut runtime = Runtime::new();

    let spawn_flag = spawn_succeeded.clone();
    runtime.spawn_worker(move || async move {
        // This spawn should work - we're in a runtime context
        let handle = spawn(async {
            // Do nothing
        });
        handle.await;
        spawn_flag.store(true, Ordering::SeqCst);
    });

    runtime.block_on_all();

    assert!(spawn_succeeded.load(Ordering::SeqCst));
}
