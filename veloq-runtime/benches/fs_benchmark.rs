use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use veloq_runtime::LocalExecutor;
use veloq_runtime::io::buffer::BuddyPool;
use veloq_runtime::io::buffer::buddy::BufferSize;
use veloq_runtime::io::fs::File;

fn benchmark_1gb_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("fs_throughput");

    // 1GB Total Size
    const TOTAL_SIZE: u64 = 1 * 1024 * 1024 * 1024;

    // 设置吞吐量统计单位
    group.throughput(Throughput::Bytes(TOTAL_SIZE));
    // 1GB写入耗时较长，减少采样次数并增加单次超时时间
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(120));

    group.bench_function("write_1gb_concurrent", |b| {
        let exec = LocalExecutor::<BuddyPool>::default();
        b.iter(|| {
            // 复用 LocalExecutor 避免每次迭代创建 driver 的开销
            exec.block_on(|cx| {
                let cx = cx.clone();
                async move {
                    const CHUNK_SIZE_ENUM: BufferSize = BufferSize::Size4M;
                    let chunk_size = CHUNK_SIZE_ENUM.size();
                    let file_path = Path::new("bench_1gb_test.tmp");

                    if file_path.exists() {
                        let _ = std::fs::remove_file(file_path);
                    }

                    // Use File::create which takes context
                    let file = File::create(&file_path, &cx)
                        .await
                        .expect("Failed to create");

                    let file = Rc::new(file);

                    // Pre-allocate space to avoid metadata lock contention during extended writes
                    file.fallocate(0, TOTAL_SIZE)
                        .await
                        .expect("Fallocate failed");

                    // 限制并发度为 BufferPool 中该尺寸 Chunk 的最大可用数 (32)
                    let concurrency_limit = 32;
                    let mut tasks = VecDeque::new();
                    let mut offset: u64 = 0;

                    while offset < TOTAL_SIZE {
                        // 1. 尝试分配并在此窗口内提交任务
                        if tasks.len() < concurrency_limit {
                            // Use cx.buffer_pool()
                            if let Some(buf) =
                                cx.buffer_pool().upgrade().unwrap().alloc(CHUNK_SIZE_ENUM)
                            {
                                let remaining = TOTAL_SIZE - offset;
                                let write_len =
                                    std::cmp::min(remaining, chunk_size as u64) as usize;

                                let file_clone = file.clone();
                                let current_offset = offset;

                                let fut =
                                    async move { file_clone.write_at(buf, current_offset).await };

                                // Use cx.spawn_local
                                tasks.push_back(cx.spawn_local(fut));
                                offset += write_len as u64;
                                continue;
                            }
                        }

                        // 2. 无法分配或达到并发限制，等待最早的任务完成以释放资源
                        if let Some(handle) = tasks.pop_front() {
                            let (res, _buf) = handle.await;
                            res.expect("Write failed");
                        } else {
                            panic!("Deadlock: No tasks to wait for but cannot allocate buffer");
                        }
                    }

                    // 3. 等待剩余任务
                    while let Some(handle) = tasks.pop_front() {
                        let (res, _buf) = handle.await;
                        res.expect("Write failed");
                    }

                    // 使用 verify range 替代 sync_all
                    file.sync_range(0, TOTAL_SIZE).await.expect("Sync failed");

                    // 清理
                    drop(file);
                    let _ = std::fs::remove_file(file_path);
                }
            });
        })
    });
    group.finish();
}

fn benchmark_32_files_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("fs_throughput_32_files");

    // 1GB Total Size
    const FILE_COUNT: usize = 32;
    const TOTAL_SIZE: u64 = 1 * 1024 * 1024 * 1024;
    const FILE_SIZE: u64 = TOTAL_SIZE / FILE_COUNT as u64;

    group.throughput(Throughput::Bytes(TOTAL_SIZE));
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(120));

    group.bench_function("write_32_files_concurrent", |b| {
        let exec = LocalExecutor::<BuddyPool>::default();
        b.iter(|| {
            exec.block_on(|cx| {
                let cx = cx.clone();
                async move {
                    const CHUNK_SIZE_ENUM: BufferSize = BufferSize::Size4M;
                    let chunk_size = CHUNK_SIZE_ENUM.size();

                    let mut files = Vec::with_capacity(FILE_COUNT);
                    let mut file_paths = Vec::with_capacity(FILE_COUNT);

                    for i in 0..FILE_COUNT {
                        let path_str = format!("bench_32_{}.tmp", i);
                        let path = PathBuf::from(path_str);

                        if path.exists() {
                            let _ = std::fs::remove_file(&path);
                        }

                        let file = File::create(&path, &cx).await.expect("Failed to create");
                        let file = Rc::new(file);

                        file.fallocate(0, FILE_SIZE)
                            .await
                            .expect("Fallocate failed");

                        files.push(file);
                        file_paths.push(path);
                    }

                    let concurrency_limit = 32;
                    let mut tasks = VecDeque::new();
                    let mut offsets = vec![0u64; FILE_COUNT];
                    let mut current_file_idx = 0;

                    loop {
                        let all_submitted = offsets.iter().all(|&o| o >= FILE_SIZE);

                        if all_submitted && tasks.is_empty() {
                            break;
                        }

                        // 1. 尝试分配并在此窗口内提交任务
                        if tasks.len() < concurrency_limit && !all_submitted {
                            // Find next file that needs writing
                            let mut found = None;
                            for _ in 0..FILE_COUNT {
                                if offsets[current_file_idx] < FILE_SIZE {
                                    found = Some(current_file_idx);
                                    current_file_idx = (current_file_idx + 1) % FILE_COUNT;
                                    break;
                                }
                                current_file_idx = (current_file_idx + 1) % FILE_COUNT;
                            }

                            if let Some(idx) = found {
                                if let Some(buf) =
                                    cx.buffer_pool().upgrade().unwrap().alloc(CHUNK_SIZE_ENUM)
                                {
                                    let remaining = FILE_SIZE - offsets[idx];
                                    let write_len =
                                        std::cmp::min(remaining, chunk_size as u64) as usize;

                                    let file_clone = files[idx].clone();
                                    let current_offset = offsets[idx];

                                    let fut = async move {
                                        file_clone.write_at(buf, current_offset).await
                                    };

                                    tasks.push_back(cx.spawn_local(fut));
                                    offsets[idx] += write_len as u64;
                                    continue;
                                }
                            }
                        }

                        // 2. 无法分配或达到并发限制，等待最早的任务完成以释放资源
                        if let Some(handle) = tasks.pop_front() {
                            let (res, _buf) = handle.await;
                            res.expect("Write failed");
                        } else {
                            if !all_submitted {
                                panic!("Deadlock: No tasks to wait for but cannot allocate buffer");
                            }
                        }
                    }

                    for file in &files {
                        file.sync_range(0, FILE_SIZE).await.expect("Sync failed");
                    }

                    drop(files);
                    for path in file_paths {
                        let _ = std::fs::remove_file(path);
                    }
                }
            });
        })
    });
    group.finish();
}

criterion_group!(benches, benchmark_1gb_write, benchmark_32_files_write);
criterion_main!(benches);
