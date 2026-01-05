use crate::fs::File;
use crate::io::buffer::HybridPool;
use crate::runtime::executor::LocalExecutor;
use std::fs;
use std::path::Path;

#[test]
fn test_file_integrity() {
    use crate::io::buffer::hybrid::BufferSize;

    for size in [BufferSize::Size8K, BufferSize::Size16K, BufferSize::Size64K] {
        std::thread::spawn(move || {
            println!("Testing with BufferSize: {:?}", size);
            let mut exec = LocalExecutor::default();
            let pool = HybridPool::new().unwrap();

            crate::runtime::context::bind_pool(pool.clone());
            let pool_clone = pool.clone();

            exec.block_on(async move {
                let pool = pool_clone.clone();

                let file_path_string = format!("test_file_integrity_{:?}.tmp", size);
                let file_path = Path::new(&file_path_string);
                // Remove file if exists
                if file_path.exists() {
                    let _ = fs::remove_file(file_path);
                }

                // 1. Create and Write
                {
                    let file = File::create(&file_path).await.expect("Failed to create");

                    let mut write_buf = pool.alloc(size).unwrap();
                    let data = b"Hello World!";
                    write_buf.spare_capacity_mut()[..data.len()].copy_from_slice(data);

                    let (res, _) = file.write_at(write_buf, 0).await;
                    let wrote = res.expect("Write failed");
                    assert_eq!(wrote, size.size());

                    file.sync_all().await.expect("Sync failed");
                }

                // 2. Open and Read
                {
                    let file = File::open(&file_path).await.expect("Failed to open");

                    let read_buf = pool.alloc(size).unwrap();

                    let (res, read_buf) = file.read_at(read_buf, 0).await;
                    let n = res.expect("Read failed");
                    assert_eq!(n, size.size());
                    assert_eq!(&read_buf.as_slice()[..12], b"Hello World!");
                }

                // Cleanup
                if file_path.exists() {
                    let _ = fs::remove_file(file_path);
                }
            });
        })
        .join()
        .unwrap();
    }
}
