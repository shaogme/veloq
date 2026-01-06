use crate::runtime::{LocalExecutor, Runtime, spawn_to, yield_now};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn current_worker_id() -> Option<usize> {
    let ctx = crate::runtime::context::current();
    if let Some(mesh_weak) = &ctx.mesh {
        if let Some(mesh) = mesh_weak.upgrade() {
            return Some(mesh.borrow().id);
        }
    }
    None
}

#[test]
fn test_spawn_to_worker() {
    let config = crate::config::Config {
        worker_threads: Some(4),
        ..Default::default()
    };

    let mut runtime = Runtime::new(config);
    let finished = Arc::new(AtomicBool::new(false));
    let finished_clone = finished.clone();

    // Spawn 4 workers
    for _ in 0..4 {
        let f = finished.clone();
        runtime.spawn_worker(move || {
            let exec = LocalExecutor::new();
            (exec, async move {
                // Keep alive
                while !f.load(Ordering::Relaxed) {
                    yield_now().await;
                }
            })
        });
    }

    // Inject a task that spawns to a specific worker
    runtime.spawn(async move {
        // Target worker 2
        let target_id = 2;

        let handle = spawn_to(
            async move {
                let id = current_worker_id();
                id
            },
            target_id,
        );

        let executed_id = handle.await;

        assert_eq!(
            executed_id,
            Some(target_id),
            "Task should run on target worker 2"
        );

        // Target worker 3
        let target_id_3 = 3;
        let handle3 = spawn_to(async move { current_worker_id() }, target_id_3);

        assert_eq!(
            handle3.await,
            Some(target_id_3),
            "Task should run on target worker 3"
        );

        finished_clone.store(true, Ordering::Relaxed);
    });

    runtime.block_on_all();
}
