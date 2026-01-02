use crate::io::buffer::HybridPool;
use crate::io::fs::File;
use std::fs;
use std::path::Path;

#[test]
fn test_file_integrity() {
    use crate::io::buffer::hybrid::BufferSize;

    for size in [BufferSize::Size4K, BufferSize::Size16K, BufferSize::Size64K] {
        println!("Testing with BufferSize: {:?}", size);
        crate::runtime::LocalExecutor::<HybridPool>::default().block_on(|cx| {
            let cx = cx.clone();
            async move {
                let file_path = Path::new("test_file_integrity.tmp");
                if file_path.exists() {
                    fs::remove_file(file_path).unwrap();
                }

                // 1. Create and Write
                {
                    let file = File::create(&file_path, &cx)
                        .await
                        .expect("Failed to create");

                    let mut write_buf = cx.buffer_pool().upgrade().unwrap().alloc(size).unwrap();
                    let data = b"Hello World!";
                    write_buf.spare_capacity_mut()[..data.len()].copy_from_slice(data);
                    // write_buf.set_len(data.len()); // Buffer defaults to full capacity

                    let (res, _) = file.write_at(write_buf, 0).await;
                    let wrote = res.expect("Write failed");
                    assert_eq!(wrote, size.size()); // Expect full buffer write

                    file.sync_all().await.expect("Sync failed");
                }

                // 2. Open and Read
                {
                    let file = File::open(&file_path, &cx).await.expect("Failed to open");

                    let read_buf = cx.buffer_pool().upgrade().unwrap().alloc(size).unwrap();
                    // read_buf.set_len(read_buf.capacity()); // Default is full capacity

                    let (res, read_buf) = file.read_at(read_buf, 0).await;
                    let n = res.expect("Read failed");
                    // Read should return full size since we wrote full size
                    assert_eq!(n, size.size());
                    assert_eq!(&read_buf.as_slice()[..12], b"Hello World!");
                }

                // Cleanup
                fs::remove_file(file_path).unwrap();
            }
        });
    }
}
