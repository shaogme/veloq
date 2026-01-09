use clap::{Parser, ValueEnum};
use rand::prelude::*;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Instant;
use veloq_runtime::fs::{BufferingMode, File};
use veloq_runtime::io::buffer::{AnyBufPool, BuddyPool, BufPool, RegisteredPool};
use veloq_runtime::runtime::Runtime;
use veloq_runtime::spawn_local;

#[derive(Clone, Copy, ValueEnum, Debug)]
enum WriteMode {
    Seq,
    Rand,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Write mode: sequential or random
    #[arg(long, value_enum)]
    mode: WriteMode,

    /// Number of worker threads
    #[arg(long, default_value_t = 1)]
    threads: usize,

    /// Queue depth (concurrent tasks per thread)
    #[arg(long, default_value_t = 32)]
    qdepth: usize,
}

// 1GB per thread for benchmarking
const FILE_SIZE_PER_THREAD: u64 = 1 * 1024 * 1024 * 1024;
const BLOCK_SIZE: usize = 1024 * 1024;

async fn run_worker(worker_id: usize, mode: WriteMode, qdepth: usize) -> std::io::Result<u64> {
    let pool =
        veloq_runtime::runtime::context::current_pool().expect("Worker should have bound pool");

    let path_str = format!("bench_test_{}.tmp", worker_id);
    let path = PathBuf::from(&path_str);

    // Clean up previous run if exists
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }

    let file = File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .buffering(BufferingMode::DirectSync)
        .open(&path)
        .await
        .expect("Failed to create file");

    let file = Rc::new(file);

    // Pre-allocate
    file.fallocate(0, FILE_SIZE_PER_THREAD)
        .await
        .expect("Fallocate failed");

    // Pre-allocate tasks (offsets)
    let num_blocks = FILE_SIZE_PER_THREAD / BLOCK_SIZE as u64;
    let mut offsets: Vec<u64> = (0..num_blocks).map(|i| i * BLOCK_SIZE as u64).collect();

    if let WriteMode::Rand = mode {
        let mut rng = rand::thread_rng();
        offsets.shuffle(&mut rng);
    }

    let mut pending_tasks = VecDeque::new();
    let mut current_idx = 0;
    let total_tasks = offsets.len();
    let mut written_bytes = 0u64;

    while current_idx < total_tasks {
        // Fill the queue up to qdepth
        while pending_tasks.len() < qdepth && current_idx < total_tasks {
            if let Some(buf) = pool.alloc(BLOCK_SIZE) {
                let offset = offsets[current_idx];
                let file_clone = file.clone();

                let fut = async move { file_clone.write_at(buf, offset).await };

                pending_tasks.push_back(spawn_local(fut));
                current_idx += 1;
            } else {
                // Buffer pool exhausted, must wait for tasks to finish
                break;
            }
        }

        // Wait for at least one task to finish to make progress
        if let Some(handle) = pending_tasks.pop_front() {
            let (res, _buf) = handle.await;
            let written = res?;
            written_bytes += written as u64;
        } else {
            // This case should be rare/impossible unless qdepth=0 or pool is empty initially
            if current_idx < total_tasks {
                panic!("Deadlock: No pending tasks but cannot allocate buffer.");
            }
        }
    }

    // Wait for remaining tasks
    while let Some(handle) = pending_tasks.pop_front() {
        let (res, _buf) = handle.await;
        let written = res?;
        written_bytes += written as u64;
    }

    // Sync
    file.sync_range(0, FILE_SIZE_PER_THREAD)
        .wait_before(false)
        .write(true)
        .wait_after(true)
        .await
        .expect("Sync failed");

    drop(file); // explicit drop before remove, though variable goes out of scope anyway
    let _ = std::fs::remove_file(path);

    Ok(written_bytes)
}

fn main() {
    let args = Args::parse();
    println!("Starting Disk Benchmark");
    println!(
        "Mode: {:?}, Threads: {}, Queue Depth: {}",
        args.mode, args.threads, args.qdepth
    );
    println!(
        "Block Size: {} Bytes, File Size per Thread: {} Bytes",
        BLOCK_SIZE, FILE_SIZE_PER_THREAD
    );

    let runtime = Runtime::builder()
        .config(veloq_runtime::config::Config {
            worker_threads: Some(args.threads),
            ..Default::default()
        })
        .pool_constructor(|_, registrar| {
            let pool = BuddyPool::new().expect("Failed to create BuddyPool");
            let reg_pool = RegisteredPool::new(pool, registrar).expect("Failed to register pool");
            AnyBufPool::new(reg_pool)
        })
        .build()
        .expect("Failed to build Runtime");

    let start_time = Instant::now();

    runtime.block_on(async {
        let (tx, rx) = mpsc::channel();

        for i in 0..args.threads {
            let tx = tx.clone();
            let mode = args.mode;
            let qdepth = args.qdepth;

            // Spawn worker task to specific thread
            // Note: spawn_to logic ensures it runs on worker `i` if configured that way,
            // but `spawn_to` API in veloq_runtime takes closure + ID.
            veloq_runtime::runtime::context::spawn_to(
                async move || {
                    let bytes = run_worker(i, mode, qdepth)
                        .await
                        .expect("Worker execution failed");
                    tx.send(bytes).unwrap();
                },
                i,
            );
        }

        // Aggregate results
        let mut total_bytes = 0;
        for _ in 0..args.threads {
            total_bytes += rx.recv().unwrap();
        }

        let elapsed = start_time.elapsed();
        let secs = elapsed.as_secs_f64();
        let mb = total_bytes as f64 / 1024.0 / 1024.0;
        let throughput = mb / secs;

        println!("Done.");
        println!("Total Write: {:.2} MB", mb);
        println!("Elapsed: {:.4} s", secs);
        println!("Throughput: {:.2} MB/s", throughput);
    });
}
