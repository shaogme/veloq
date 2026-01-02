//! TCP network tests - single-threaded and multi-threaded.

use crate::io::buffer::{BufferPool, FixedBuf};
use crate::net::tcp::{TcpListener, TcpStream};
use crate::runtime::executor::{LocalExecutor, Runtime};
use crate::{current_driver, spawn_local as spawn};
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ============ Helper Functions ============

use crate::io::buffer::BufferSize;

/// Helper function to allocate a buffer from a pool
fn alloc_buf(pool: &BufferPool, size: BufferSize) -> FixedBuf {
    pool.alloc(size)
        .expect("Failed to allocate buffer from pool")
}

// ============ Single-Thread TCP Tests ============

/// Test basic TCP connection using global spawn and current_driver
#[test]
fn test_tcp_connect_with_global_api() {
    let exec = LocalExecutor::new();
    let driver = exec.driver_handle();

    // Create listener before block_on
    let listener =
        TcpListener::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind listener");
    let listen_addr = listener.local_addr().expect("Failed to get local address");
    println!("Listener bound to: {}", listen_addr);

    let listener_rc = Rc::new(listener);
    let listener_clone = listener_rc.clone();

    exec.block_on(async move {
        // Server task using global spawn
        let server_h = spawn(async move {
            let (stream, peer_addr) = listener_clone.accept().await.expect("Accept failed");
            println!("Accepted connection from: {}", peer_addr);
            drop(stream);
        });

        // Client uses current_driver
        let driver = current_driver();
        let stream = TcpStream::connect(listen_addr, driver)
            .await
            .expect("Failed to connect");
        println!("Connected successfully");
        drop(stream);

        server_h.await;
    });
}

/// Test TCP data send and receive (echo)
#[test]
fn test_tcp_send_recv() {
    for size in [BufferSize::Size4K, BufferSize::Size16K, BufferSize::Size64K] {
        println!("Testing with BufferSize: {:?}", size);
        let exec = LocalExecutor::new();
        let driver = exec.driver_handle();

        let listener =
            TcpListener::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind listener");
        let listen_addr = listener.local_addr().expect("Failed to get local address");

        let listener_rc = Rc::new(listener);
        let listener_clone = listener_rc.clone();

        exec.block_on(async move {
            // Get driver and allocate buffers inside async context
            let driver_weak = current_driver();

            // Server task
            let server_h = spawn(async move {
                let (stream, _) = listener_clone.accept().await.expect("Accept failed");

                // Receive data
                let mut buf = crate::current_buffer_pool()
                    .upgrade()
                    .unwrap()
                    .alloc(size)
                    .unwrap();
                buf.set_len(buf.capacity());
                let (result, buf) = stream.recv(buf).await;
                let bytes_read = result.expect("Recv failed") as usize;
                println!("Server received {} bytes", bytes_read);

                // Echo data back
                let mut echo_buf = crate::current_buffer_pool()
                    .upgrade()
                    .unwrap()
                    .alloc(size)
                    .unwrap();
                echo_buf.spare_capacity_mut()[..bytes_read]
                    .copy_from_slice(&buf.as_slice()[..bytes_read]);
                echo_buf.set_len(bytes_read);

                let (result, _) = stream.send(echo_buf).await;
                result.expect("Send failed");
                println!("Server echoed data");
            });

            // Client
            let stream = TcpStream::connect(listen_addr, driver_weak)
                .await
                .expect("Failed to connect");

            // Prepare data
            let mut send_buf = crate::current_buffer_pool()
                .upgrade()
                .unwrap()
                .alloc(size)
                .unwrap();
            let test_data = b"Hello, TCP!";
            send_buf.spare_capacity_mut()[..test_data.len()].copy_from_slice(test_data);
            send_buf.set_len(test_data.len());

            // Send data
            let (result, _) = stream.send(send_buf).await;
            let bytes_sent = result.expect("Client send failed");
            println!("Client sent {} bytes", bytes_sent);

            // Receive echo
            let mut recv_buf = crate::current_buffer_pool()
                .upgrade()
                .unwrap()
                .alloc(size)
                .unwrap();
            recv_buf.set_len(recv_buf.capacity());
            let (result, recv_buf) = stream.recv(recv_buf).await;
            let bytes_received = result.expect("Client recv failed") as usize;

            println!("Client received {} bytes", bytes_received);

            // Verify
            assert_eq!(bytes_sent as usize, bytes_received);
            assert_eq!(&recv_buf.as_slice()[..bytes_received], test_data);
            println!("Data verification successful!");

            server_h.await;
        });
    }
}

/// Test multiple concurrent connections on single thread
#[test]
fn test_tcp_multiple_connections() {
    let exec = LocalExecutor::new();
    let driver = exec.driver_handle();

    let listener =
        TcpListener::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind listener");
    let listen_addr = listener.local_addr().expect("Failed to get local address");

    const NUM_CONNECTIONS: usize = 5;

    let listener_rc = Rc::new(listener);
    let listener_clone = listener_rc.clone();

    exec.block_on(async move {
        // Server task: accept all connections
        let server_h = spawn(async move {
            for i in 0..NUM_CONNECTIONS {
                let (stream, peer) = listener_clone.accept().await.expect("Accept failed");
                println!("Accepted connection {} from {}", i, peer);
                drop(stream);
            }
            println!("All {} connections accepted", NUM_CONNECTIONS);
        });

        // Client: make connections sequentially
        let driver = current_driver();
        for i in 0..NUM_CONNECTIONS {
            let stream = TcpStream::connect(listen_addr, driver.clone())
                .await
                .expect("Failed to connect");
            println!("Client {} connected", i);
            drop(stream);
        }
        println!("All {} connections completed", NUM_CONNECTIONS);

        server_h.await;
    });
}

/// Test large data transfer
#[test]
fn test_tcp_large_data_transfer() {
    for size in [BufferSize::Size4K, BufferSize::Size16K, BufferSize::Size64K] {
        let exec = LocalExecutor::new();
        let driver = exec.driver_handle();
        let pool = Rc::new(BufferPool::new());

        let listener =
            TcpListener::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind listener");
        let listen_addr = listener.local_addr().expect("Failed to get local address");

        const DATA_SIZE: usize = 8192; // 8KB
        const CHUNK_SIZE: usize = 4096;

        let listener_rc = Rc::new(listener);
        let listener_clone = listener_rc.clone();
        let pool_clone = pool.clone();

        exec.block_on(async move {
            // Server task
            let server_h = spawn(async move {
                let (stream, _) = listener_clone.accept().await.expect("Accept failed");

                let mut total_received = 0;
                while total_received < DATA_SIZE {
                    let mut buf = alloc_buf(&pool_clone, size);
                    buf.set_len(buf.capacity());
                    let (result, _buf) = stream.recv(buf).await;
                    let bytes = result.expect("Recv failed") as usize;
                    if bytes == 0 {
                        break;
                    }
                    total_received += bytes;
                    println!(
                        "Server received {} bytes (total: {})",
                        bytes, total_received
                    );
                }

                assert_eq!(total_received, DATA_SIZE);
                println!("Server received all {} bytes", DATA_SIZE);
            });

            // Client
            let driver = current_driver();
            let stream = TcpStream::connect(listen_addr, driver)
                .await
                .expect("Failed to connect");

            let mut total_sent = 0;
            while total_sent < DATA_SIZE {
                let chunk_size = std::cmp::min(CHUNK_SIZE, DATA_SIZE - total_sent);
                // Ensure chunk size <= buffer size, but since chunks are <= 4096 and min buf is 4096, it's fine

                let mut buf = alloc_buf(&pool, size);

                for i in 0..chunk_size {
                    buf.spare_capacity_mut()[i] = (i % 256) as u8;
                }
                buf.set_len(chunk_size);

                let (result, _buf) = stream.send(buf).await;
                let bytes = result.expect("Send failed") as usize;
                total_sent += bytes;
                println!("Client sent {} bytes (total: {})", bytes, total_sent);
            }

            assert_eq!(total_sent, DATA_SIZE);
            println!("Client sent all {} bytes", DATA_SIZE);

            server_h.await;
        });
    }
}

/// Test listener local_addr
#[test]
fn test_listener_local_addr() {
    let exec = LocalExecutor::new();
    let driver = exec.driver_handle();

    exec.block_on(async move {
        let listener =
            TcpListener::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind listener");

        let addr = listener.local_addr().expect("Failed to get local address");

        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_ne!(addr.port(), 0);

        println!("Listener local address: {}", addr);
    });
}

/// Test connection refused
#[test]
fn test_tcp_connect_refused() {
    let exec = LocalExecutor::new();

    exec.block_on(async move {
        let addr: SocketAddr = "127.0.0.1:65534".parse().unwrap();
        let driver = current_driver();

        let result = TcpStream::connect(addr, driver).await;

        assert!(result.is_err());
        println!("Connection refused as expected: {:?}", result.err());
    });
}

/// Test receiving zero bytes (EOF)
#[test]
fn test_tcp_recv_zero_bytes() {
    for size in [BufferSize::Size4K, BufferSize::Size16K, BufferSize::Size64K] {
        let exec = LocalExecutor::new();
        let driver = exec.driver_handle();
        let pool = Rc::new(BufferPool::new());

        let listener =
            TcpListener::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind listener");
        let listen_addr = listener.local_addr().expect("Failed to get local address");

        let listener_rc = Rc::new(listener);
        let listener_clone = listener_rc.clone();

        exec.block_on(async move {
            // Server: accept and immediately close
            let server_h = spawn(async move {
                let (stream, _) = listener_clone.accept().await.expect("Accept failed");
                println!("Server accepted and closing connection");
                drop(stream);
            });

            // Client
            let driver = current_driver();
            let stream = TcpStream::connect(listen_addr, driver)
                .await
                .expect("Failed to connect");

            let mut buf = alloc_buf(&pool, size);
            buf.set_len(buf.capacity());
            let (result, _buf) = stream.recv(buf).await;

            if let Ok(bytes) = result {
                assert_eq!(bytes, 0, "Should receive 0 bytes on closed connection");
                println!("Correctly received 0 bytes (EOF)");
            } else {
                println!("Received error on closed connection: {:?}", result.err());
            }

            server_h.await;
        });
    }
}

/// Test IPv6 connection
#[test]
fn test_tcp_ipv6() {
    let exec = LocalExecutor::new();
    let driver = exec.driver_handle();

    let listener_result = TcpListener::bind("::1:0", driver.clone());

    if listener_result.is_err() {
        println!("IPv6 not available, skipping test");
        return;
    }

    let listener = listener_result.unwrap();
    let listen_addr = listener.local_addr().expect("Failed to get local address");

    assert!(listen_addr.is_ipv6());
    println!("IPv6 listener bound to: {}", listen_addr);

    let listener_rc = Rc::new(listener);
    let listener_clone = listener_rc.clone();

    exec.block_on(async move {
        let server_h = spawn(async move {
            let (stream, peer) = listener_clone.accept().await.expect("Accept failed");
            println!("Accepted IPv6 connection from: {}", peer);
            drop(stream);
        });

        let driver = current_driver();
        let stream = TcpStream::connect(listen_addr, driver)
            .await
            .expect("Failed to connect via IPv6");

        println!("IPv6 connection successful");
        drop(stream);

        server_h.await;
    });
}

// ============ Multi-Thread TCP Tests ============

/// Test TCP connection across multiple worker threads (each thread is independent)
#[test]
fn test_multithread_tcp_connections() {
    let connection_count = Arc::new(AtomicUsize::new(0));
    let mut runtime = Runtime::new(crate::config::Config::default());

    const NUM_WORKERS: usize = 3;

    for worker_id in 0..NUM_WORKERS {
        let counter = connection_count.clone();
        runtime.spawn_worker(move || async move {
            // Each worker has its own executor, driver, and listener
            let driver = current_driver();

            let listener =
                TcpListener::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind listener");
            let listen_addr = listener.local_addr().expect("Failed to get local address");
            println!("Worker {} listening on {}", worker_id, listen_addr);

            let listener_rc = Rc::new(listener);
            let listener_clone = listener_rc.clone();

            // Server task
            let server_h = spawn(async move {
                let (stream, peer) = listener_clone.accept().await.expect("Accept failed");
                println!("Worker {} accepted from {}", worker_id, peer);
                drop(stream);
            });

            // Client connects to self
            let stream = TcpStream::connect(listen_addr, driver.clone())
                .await
                .expect("Failed to connect");
            println!("Worker {} connected to self", worker_id);
            drop(stream);

            server_h.await;

            counter.fetch_add(1, Ordering::SeqCst);
        });
    }

    runtime.block_on_all();

    assert_eq!(connection_count.load(Ordering::SeqCst), NUM_WORKERS);
    println!("All {} workers completed TCP self-connections", NUM_WORKERS);
}

/// Test TCP echo server on one worker, clients on another
#[test]
fn test_multithread_tcp_echo() {
    use std::sync::mpsc;
    use std::time::Duration;

    for size in [BufferSize::Size4K, BufferSize::Size16K, BufferSize::Size64K] {
        let (addr_tx, addr_rx) = mpsc::channel();
        let mut runtime = Runtime::new(crate::config::Config::default());

        // Worker 1: Echo server
        runtime.spawn_worker(move || async move {
            let driver = current_driver();

            let listener =
                TcpListener::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind listener");
            let listen_addr = listener.local_addr().expect("Failed to get local address");
            println!("Echo server listening on {}", listen_addr);

            // Send address to client worker
            addr_tx.send(listen_addr).unwrap();

            // Accept and echo
            let (stream, _) = Rc::new(listener).accept().await.expect("Accept failed");

            let mut buf = crate::current_buffer_pool()
                .upgrade()
                .unwrap()
                .alloc(size)
                .unwrap();
            buf.set_len(buf.capacity());

            let (result, buf) = stream.recv(buf).await;
            let bytes = result.expect("Recv failed") as usize;
            println!("Echo server received {} bytes", bytes);

            // Echo back
            let mut echo_buf = crate::current_buffer_pool()
                .upgrade()
                .unwrap()
                .alloc(size)
                .unwrap();
            echo_buf.spare_capacity_mut()[..bytes].copy_from_slice(&buf.as_slice()[..bytes]);
            echo_buf.set_len(bytes);

            let (result, _) = stream.send(echo_buf).await;
            result.expect("Send failed");
            println!("Echo server sent response");
        });

        // Worker 2: Client
        runtime.spawn_worker(move || async move {
            // Wait for server address
            let listen_addr = addr_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("Timeout waiting for server address");
            println!("Client connecting to {}", listen_addr);

            let driver = current_driver();

            let stream = TcpStream::connect(listen_addr, driver)
                .await
                .expect("Failed to connect");

            // Send data
            let mut send_buf = crate::current_buffer_pool()
                .upgrade()
                .unwrap()
                .alloc(size)
                .unwrap();
            let data = b"Hello from worker 2!";
            send_buf.spare_capacity_mut()[..data.len()].copy_from_slice(data);
            send_buf.set_len(data.len());

            let (result, _) = stream.send(send_buf).await;
            let sent = result.expect("Send failed");
            println!("Client sent {} bytes", sent);

            // Receive echo
            let mut recv_buf = crate::current_buffer_pool()
                .upgrade()
                .unwrap()
                .alloc(size)
                .unwrap();
            recv_buf.set_len(recv_buf.capacity());
            let (result, recv_buf) = stream.recv(recv_buf).await;
            let received = result.expect("Recv failed") as usize;

            assert_eq!(&recv_buf.as_slice()[..received], data);
            println!("Client received correct echo");
        });

        runtime.block_on_all();
        println!("Multi-thread echo test completed");
    }
}

/// Test concurrent connections from multiple workers to shared server
#[test]
fn test_multithread_concurrent_clients() {
    use std::sync::mpsc;
    use std::time::Duration;

    let (addr_tx, addr_rx) = mpsc::channel::<SocketAddr>();
    // Wrap receiver in Arc<Mutex> so it can be shared across threads
    let addr_rx = Arc::new(Mutex::new(addr_rx));
    let connection_count = Arc::new(AtomicUsize::new(0));
    let mut runtime = Runtime::new(crate::config::Config::default());

    const NUM_CLIENTS: usize = 3;

    // Server worker
    runtime.spawn_worker(move || async move {
        let driver = current_driver();

        let listener =
            TcpListener::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind listener");
        let listen_addr = listener.local_addr().expect("Failed to get local address");
        println!("Server listening on {}", listen_addr);

        // Broadcast address to all clients
        for _ in 0..NUM_CLIENTS {
            addr_tx.send(listen_addr).unwrap();
        }

        let listener_rc = Rc::new(listener);

        // Accept all connections
        for i in 0..NUM_CLIENTS {
            let (stream, peer) = listener_rc.accept().await.expect("Accept failed");
            println!("Server accepted connection {} from {}", i, peer);
            drop(stream);
        }
        println!("Server accepted all {} connections", NUM_CLIENTS);
    });

    // Client workers
    for client_id in 0..NUM_CLIENTS {
        let rx = addr_rx.clone();
        let counter = connection_count.clone();
        runtime.spawn_worker(move || async move {
            // Get address from shared receiver
            let listen_addr = {
                let rx_guard = rx.lock().unwrap();
                rx_guard
                    .recv_timeout(Duration::from_secs(5))
                    .expect("Timeout waiting for server address")
            };

            let driver = current_driver();
            let stream = TcpStream::connect(listen_addr, driver)
                .await
                .expect("Failed to connect");

            println!("Client {} connected", client_id);
            drop(stream);

            counter.fetch_add(1, Ordering::SeqCst);
        });
    }

    runtime.block_on_all();

    assert_eq!(connection_count.load(Ordering::SeqCst), NUM_CLIENTS);
    println!("All {} clients completed", NUM_CLIENTS);
}
