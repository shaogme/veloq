//! TCP network tests - single-threaded and multi-threaded.

use crate::io::buffer::{AnyBufPool, BufPool, FixedBuf, HybridPool, RegisteredPool};
use crate::net::tcp::{TcpListener, TcpStream};
use crate::runtime::{LocalExecutor, Runtime};
use crate::spawn_local;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ============ Helper Functions ============

/// Helper function to allocate a buffer from a pool
fn alloc_buf(pool: &impl BufPool, size: usize) -> FixedBuf {
    pool.alloc(size)
        .expect("Failed to allocate buffer from pool")
}

// ============ Single-Thread TCP Tests ============

/// Test basic TCP connection using global spawn and current_driver
#[test]
fn test_tcp_connect_with_global_api() {
    let mut exec = LocalExecutor::default();

    exec.block_on(async move {
        // Create listener inside block_on to access context
        let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind listener");
        let listen_addr = listener.local_addr().expect("Failed to get local address");
        println!("Listener bound to: {}", listen_addr);

        let listener_rc = Rc::new(listener);
        let listener_clone = listener_rc.clone();

        // Server task using cx.spawn_local
        let server_h = spawn_local(async move {
            let (stream, peer_addr) = listener_clone.accept().await.expect("Accept failed");
            println!("Accepted connection from: {}", peer_addr);
            drop(stream);
        });

        // Client uses cx.driver()
        // Client uses cx implicitly
        let stream = TcpStream::connect(listen_addr)
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
    let backing_pool = HybridPool::new().unwrap();
    // Move binding inside the loop/executor since we need a registrar
    let mut exec = LocalExecutor::default();
    let registrar = exec.registrar();
    let pool = RegisteredPool::new(backing_pool.clone(), registrar).unwrap();
    crate::runtime::context::bind_pool(pool.clone());

    for size in [8192, 16384, 65536] {
        println!("Testing with BufferSize: {:?}", size);
        let pool_clone = pool.clone();

        exec.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind listener");
            let listen_addr = listener.local_addr().expect("Failed to get local address");

            let listener_rc = Rc::new(listener);
            let listener_clone = listener_rc.clone();
            let pool = pool_clone.clone();

            let pool_server = pool.clone();

            // Server task: Robust Echo Loop
            let server_h = spawn_local(async move {
                let (stream, _) = listener_clone.accept().await.expect("Accept failed");

                loop {
                    let buf = alloc_buf(&pool_server, size);
                    let (result, mut buf) = stream.recv(buf).await;
                    let bytes_read = match result {
                        Ok(n) if n > 0 => n as usize,
                        _ => break, // EOF or Error
                    };

                    // Echo exact bytes received
                    buf.set_len(bytes_read);
                    if let Err(e) = stream.send(buf).await.0 {
                        println!("Server echo failed: {}", e);
                        break;
                    }
                }
            });

            // Client
            let stream = TcpStream::connect(listen_addr)
                .await
                .expect("Failed to connect");

            // Prepare data
            let mut send_buf = pool.alloc(size).unwrap();
            let test_data = b"Hello, TCP!";
            send_buf.spare_capacity_mut()[..test_data.len()].copy_from_slice(test_data);
            // Buffer is full length (clamped) by default, containing test_data + zeros

            // Send data
            let bytes_to_send = send_buf.len();
            let (result, _) = stream.send(send_buf).await;
            let bytes_sent = result.expect("Client send failed") as usize;
            assert_eq!(bytes_sent, bytes_to_send);
            println!("Client sent {} bytes", bytes_sent);

            // Receive loop verify
            let mut total_received = 0;

            while total_received < bytes_sent {
                let recv_buf = alloc_buf(&pool, size);
                let (result, recv_buf) = stream.recv(recv_buf).await;
                let n = result.expect("Client recv failed") as usize;
                if n == 0 {
                    break;
                } // Unexpected EOF?

                if total_received == 0 {
                    // Verify first chunk header
                    assert!(n >= test_data.len(), "First chunk too small");
                    assert_eq!(&recv_buf.as_slice()[..test_data.len()], test_data);
                }
                total_received += n;
            }

            println!("Client received {} bytes", total_received);

            // Verify
            assert_eq!(bytes_sent, total_received);
            println!("Data verification successful!");

            // Close client to let server exit loop
            drop(stream);
            server_h.await;
        });
    }
}

/// Test multiple concurrent connections on single thread
#[test]
fn test_tcp_multiple_connections() {
    let mut exec = LocalExecutor::default();

    exec.block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind listener");
        let listen_addr = listener.local_addr().expect("Failed to get local address");

        const NUM_CONNECTIONS: usize = 5;

        let listener_rc = Rc::new(listener);
        let listener_clone = listener_rc.clone();

        // Server task: accept all connections
        let server_h = spawn_local(async move {
            for i in 0..NUM_CONNECTIONS {
                let (stream, peer) = listener_clone.accept().await.expect("Accept failed");
                println!("Accepted connection {} from {}", i, peer);
                drop(stream);
            }
            println!("All {} connections accepted", NUM_CONNECTIONS);
        });

        // Client: make connections sequentially
        for i in 0..NUM_CONNECTIONS {
            let stream = TcpStream::connect(listen_addr)
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
    let backing_pool = HybridPool::new().unwrap();
    let mut exec = LocalExecutor::default();
    let registrar = exec.registrar();
    let pool = RegisteredPool::new(backing_pool.clone(), registrar).unwrap();
    crate::runtime::context::bind_pool(pool.clone());

    for size in [8192, 16384, 65536] {
        let pool_clone = pool.clone();

        exec.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind listener");
            let listen_addr = listener.local_addr().expect("Failed to get local address");

            const DATA_SIZE: usize = 8192; // 8KB
            const CHUNK_SIZE: usize = 4096;

            let listener_rc = Rc::new(listener);
            let listener_clone = listener_rc.clone();
            let pool = pool_clone.clone();
            let pool_server = pool.clone();

            // Server task
            let server_h = spawn_local(async move {
                let (stream, _) = listener_clone.accept().await.expect("Accept failed");

                let mut total_received = 0;
                while total_received < DATA_SIZE {
                    let buf = alloc_buf(&pool_server, size);
                    // buf.set_len(buf.capacity());
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

                assert!(total_received >= DATA_SIZE);
                println!("Server received all {} bytes", DATA_SIZE);
            });

            // Client
            let stream = TcpStream::connect(listen_addr)
                .await
                .expect("Failed to connect");

            let mut total_sent = 0;
            while total_sent < DATA_SIZE {
                let chunk_size = std::cmp::min(CHUNK_SIZE, DATA_SIZE - total_sent);

                let mut buf = alloc_buf(&pool, size);

                for i in 0..chunk_size {
                    buf.spare_capacity_mut()[i] = (i % 256) as u8;
                }

                let (result, _buf) = stream.send(buf).await;
                let bytes = result.expect("Send failed") as usize;
                total_sent += bytes;
                println!("Client sent {} bytes (total: {})", bytes, total_sent);
            }

            assert!(total_sent >= DATA_SIZE);
            println!("Client sent all {} bytes", total_sent);

            server_h.await;
        });
    }
}

/// Test listener local_addr
#[test]
fn test_listener_local_addr() {
    let mut exec = LocalExecutor::default();

    exec.block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind listener");

        let addr = listener.local_addr().expect("Failed to get local address");

        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_ne!(addr.port(), 0);

        println!("Listener local address: {}", addr);
    });
}

/// Test connection refused
#[test]
fn test_tcp_connect_refused() {
    let mut exec = LocalExecutor::default();

    exec.block_on(async move {
        let addr: SocketAddr = "127.0.0.1:65534".parse().unwrap();

        let result = TcpStream::connect(addr).await;

        assert!(result.is_err());
        println!("Connection refused as expected: {:?}", result.err());
    });
}

/// Test receiving zero bytes (EOF)
#[test]
fn test_tcp_recv_zero_bytes() {
    let backing_pool = HybridPool::new().unwrap();
    let mut exec = LocalExecutor::default();

    let registrar = exec.registrar();
    let pool = RegisteredPool::new(backing_pool.clone(), registrar).unwrap();
    crate::runtime::context::bind_pool(pool.clone());

    for size in [8192, 16384, 65536] {
        let pool_clone = pool.clone();
        exec.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind listener");
            let listen_addr = listener.local_addr().expect("Failed to get local address");

            let listener_rc = Rc::new(listener);
            let listener_clone = listener_rc.clone();
            // Server: accept and immediately close
            let server_h = spawn_local(async move {
                let (stream, _) = listener_clone.accept().await.expect("Accept failed");
                println!("Server accepted and closing connection");
                drop(stream);
            });

            // Client
            let stream = TcpStream::connect(listen_addr)
                .await
                .expect("Failed to connect");

            let buf = alloc_buf(&pool_clone, size);
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
    let mut exec = LocalExecutor::default();

    exec.block_on(async move {
        let listener_result = TcpListener::bind("::1:0");

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

        let server_h = spawn_local(async move {
            let (stream, peer) = listener_clone.accept().await.expect("Accept failed");
            println!("Accepted IPv6 connection from: {}", peer);
            drop(stream);
        });

        let stream = TcpStream::connect(listen_addr)
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
    const NUM_WORKERS: usize = 3;

    let runtime = Runtime::builder()
        .config(crate::config::Config {
            worker_threads: Some(NUM_WORKERS),
            ..Default::default()
        })
        .pool_constructor(|_, registrar| {
            let pool = HybridPool::new().unwrap();
            let reg_pool = RegisteredPool::new(pool, registrar).unwrap();
            AnyBufPool::new(reg_pool)
        })
        .build()
        .unwrap();

    let (tx, rx) = std::sync::mpsc::channel();

    let connection_count_for_block = connection_count.clone();

    runtime.block_on(async move {
        // We will spawn a task for each worker pinned to that worker
        for worker_id in 0..NUM_WORKERS {
            let counter = connection_count_for_block.clone();
            let tx_done = tx.clone();

            crate::runtime::context::spawn_to(
                async move || {
                    let listener =
                        TcpListener::bind("127.0.0.1:0").expect("Failed to bind listener");
                    let listen_addr = listener.local_addr().expect("Failed to get local address");
                    println!("Worker {} listening on {}", worker_id, listen_addr);

                    {
                        let listener_rc = Rc::new(listener);
                        let listener_clone = listener_rc.clone();

                        // Spawn server task inside using spawn_local
                        crate::spawn_local(async move {
                            let (stream, peer) =
                                listener_clone.accept().await.expect("Accept failed");
                            println!("Worker {} accepted from {}", worker_id, peer);
                            drop(stream);
                        });
                    } // Rc is dropped here

                    // Client connects to self
                    let stream = TcpStream::connect(listen_addr)
                        .await
                        .expect("Failed to connect");
                    println!("Worker {} connected to self", worker_id);
                    drop(stream);

                    counter.fetch_add(1, Ordering::SeqCst);
                    tx_done.send(()).unwrap();
                },
                worker_id,
            );
        }

        // Wait for all 3
        for _ in 0..NUM_WORKERS {
            rx.recv().unwrap();
        }
    });

    assert_eq!(connection_count.load(Ordering::SeqCst), NUM_WORKERS);
    println!("All {} workers completed TCP self-connections", NUM_WORKERS);
}

/// Test TCP echo server on one worker, clients on another
#[test]
fn test_multithread_tcp_echo() {
    use std::sync::mpsc;
    use std::time::Duration;

    for size in [8192, 16384, 65536] {
        let (addr_tx, addr_rx) = mpsc::channel();
        // 2 Workers
        let runtime = Runtime::builder()
            .config(crate::config::Config {
                worker_threads: Some(2),
                ..Default::default()
            })
            .pool_constructor(|_, registrar| {
                let pool = HybridPool::new().unwrap();
                let reg_pool = RegisteredPool::new(pool, registrar).unwrap();
                AnyBufPool::new(reg_pool)
            })
            .build()
            .unwrap();

        let (done_tx, done_rx) = mpsc::channel();

        runtime.block_on(async move {
            let addr_tx = addr_tx.clone(); // Move into task
            let addr_rx = Arc::new(Mutex::new(addr_rx)); // Share receiver attempt? 

            // Worker 0: Echo server
            crate::runtime::context::spawn_to(
                async move || {
                    let listener =
                        TcpListener::bind("127.0.0.1:0").expect("Failed to bind listener");
                    let listen_addr = listener.local_addr().expect("Failed to get local address");
                    println!("Echo server listening on {}", listen_addr);

                    // Send address to client (via channel)
                    addr_tx.send(listen_addr).unwrap();

                    // Accept and echo
                    let (stream, _) = Rc::new(listener).accept().await.expect("Accept failed");
                    let pool = crate::runtime::context::current_pool().unwrap(); // Get thread pool

                    let buf = pool.alloc(size).unwrap();

                    let (result, buf) = stream.recv(buf).await;
                    let bytes = result.expect("Recv failed") as usize;

                    // Echo back
                    let mut echo_buf = pool.alloc(size).unwrap();
                    echo_buf.as_slice_mut()[..bytes].copy_from_slice(&buf.as_slice()[..bytes]);

                    let (result, _) = stream.send(echo_buf).await;
                    result.expect("Send failed");
                    println!("Echo server sent response");
                },
                0,
            );

            // Worker 1: Client
            let done_tx = done_tx.clone();
            let addr_rx = addr_rx.clone();
            crate::runtime::context::spawn_to(
                async move || {
                    // Wait for server address
                    let listen_addr = {
                        addr_rx
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(5))
                            .unwrap()
                    };

                    let stream = TcpStream::connect(listen_addr)
                        .await
                        .expect("Failed to connect");

                    let pool = crate::runtime::context::current_pool().unwrap();

                    // Send data
                    let mut send_buf = pool.alloc(size).unwrap();
                    let data = b"Hello from worker 1!";
                    send_buf.as_slice_mut()[..data.len()].copy_from_slice(data);

                    let (result, _) = stream.send(send_buf).await;
                    let _sent = result.expect("Send failed");

                    // Receive echo
                    let recv_buf = pool.alloc(size).unwrap();
                    let (result, recv_buf) = stream.recv(recv_buf).await;
                    let _received = result.expect("Recv failed") as usize;

                    assert_eq!(&recv_buf.as_slice()[..data.len()], data);
                    println!("Client received correct echo");

                    done_tx.send(()).unwrap();
                },
                1,
            );

            // Wait for client to finish
            done_rx.recv().unwrap();
        });

        println!("Multi-thread echo test completed");
    }
}

/// Test concurrent connections from multiple workers to shared server
#[test]
fn test_multithread_concurrent_clients() {
    use std::sync::mpsc;
    use std::time::Duration;

    let (addr_tx, addr_rx) = mpsc::channel::<SocketAddr>();
    let addr_rx = Arc::new(Mutex::new(addr_rx));
    let connection_count = Arc::new(AtomicUsize::new(0));

    const NUM_CLIENTS: usize = 3;
    const NUM_WORKERS: usize = 4; // 0=Server, 1,2,3=Clients

    let runtime = Runtime::builder()
        .config(crate::config::Config {
            worker_threads: Some(NUM_WORKERS),
            ..Default::default()
        })
        .pool_constructor(|_, registrar| {
            let pool = HybridPool::new().unwrap();
            let reg_pool = RegisteredPool::new(pool, registrar).unwrap();
            AnyBufPool::new(reg_pool)
        })
        .build()
        .unwrap();

    let (done_tx, done_rx) = mpsc::channel();

    let connection_count_clone = connection_count.clone();
    runtime.block_on(async move {
        // Server worker (0)
        let addr_tx = addr_tx.clone();
        crate::runtime::context::spawn_to(
            async move || {
                let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind listener");
                let listen_addr = listener.local_addr().expect("Failed to get local address");

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
            },
            0,
        );

        // Client workers (1..=3)
        let connection_count_clone = connection_count_clone.clone();
        for client_id in 1..=NUM_CLIENTS {
            let rx = addr_rx.clone();
            let counter = connection_count_clone.clone();
            let done_tx = done_tx.clone();

            crate::runtime::context::spawn_to(
                async move || {
                    let listen_addr = {
                        rx.lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(5))
                            .expect("Timeout waiting for server address")
                    };

                    let stream = TcpStream::connect(listen_addr)
                        .await
                        .expect("Failed to connect");

                    println!("Client {} connected", client_id);
                    drop(stream);

                    counter.fetch_add(1, Ordering::SeqCst);
                    done_tx.send(()).unwrap();
                },
                client_id,
            );
        }

        // Wait for all clients
        for _ in 0..NUM_CLIENTS {
            done_rx.recv().unwrap();
        }
    });

    assert_eq!(connection_count.load(Ordering::SeqCst), NUM_CLIENTS);
    println!("All {} clients completed", NUM_CLIENTS);
}
