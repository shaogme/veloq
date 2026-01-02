use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::collections::VecDeque;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;
use veloq_runtime::io::buffer::BufferSize;
use veloq_runtime::io::fs::File;
use veloq_runtime::{LocalExecutor, current_buffer_pool, spawn_local};

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
        let exec = LocalExecutor::new();
        b.iter(|| {
            // 复用 LocalExecutor 避免每次迭代创建 driver 的开销
            exec.block_on(async {
                const CHUNK_SIZE_ENUM: BufferSize = BufferSize::Size64K;
                let chunk_size = CHUNK_SIZE_ENUM.size();
                let file_path = Path::new("bench_1gb_test.tmp");

                if file_path.exists() {
                    let _ = std::fs::remove_file(file_path);
                }

                let file = File::options()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&file_path)
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
                        if let Some(mut buf) = current_buffer_pool()
                            .upgrade()
                            .unwrap()
                            .alloc(CHUNK_SIZE_ENUM)
                        {
                            let remaining = TOTAL_SIZE - offset;
                            // 对于 O_DIRECT，读写长度和偏移量通常需要对齐（如 512 或 4096 字节）
                            // BufferPool 的块大小是 64K，是对齐的。
                            // 最后一块如果不满 64K，如果 align 不对可能会报错。
                            // 但通常 BufferSize 是 4K 对齐的。如果最后剩余不足，我们仍需小心。
                            // 这里 min 之后，如果 alignment issue 发生，write 会报错 EINVAL。
                            // 假设 TOTAL_SIZE (1GB) 是 64K 的倍数。
                            let write_len = std::cmp::min(remaining, chunk_size as u64) as usize;
                            buf.set_len(write_len);

                            let file_clone = file.clone();
                            let current_offset = offset;

                            let fut = async move { file_clone.write_at(buf, current_offset).await };

                            tasks.push_back(spawn_local(fut));
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
            });
        })
    });
    group.finish();
}

criterion_group!(benches, benchmark_1gb_write);
criterion_main!(benches);
