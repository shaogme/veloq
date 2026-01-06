//! UDP network tests - single-threaded and multi-threaded.

use crate::io::buffer::{AnyBufPool, BufPool, FixedBuf, HybridPool};
use crate::net::udp::UdpSocket;
use crate::runtime::executor::{LocalExecutor, Runtime};
use crate::spawn_local;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ============ Helper Functions ============

/// Helper function to allocate a buffer from a pool
fn alloc_buf(pool: &HybridPool, size: usize) -> FixedBuf {
    pool.alloc(size)
        .expect("Failed to allocate buffer from pool")
}

// ============ Single-Thread UDP Tests ============

/// Test basic UDP socket binding and local_addr
#[test]
fn test_udp_bind() {
    let mut exec = LocalExecutor::default();

    exec.block_on(async move {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind UDP socket");

        let addr = socket.local_addr().expect("Failed to get local address");

        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_ne!(addr.port(), 0);

        println!("UDP socket bound to: {}", addr);
    });
}

/// Test UDP send and receive
#[test]
fn test_udp_send_recv() {
    let pool = HybridPool::new().unwrap();
    crate::runtime::context::bind_pool(pool.clone());
    for size in [8192, 16384] {
        println!("Testing with BufferSize: {:?}", size);
        let mut exec = LocalExecutor::default();

        let pool_clone = pool.clone();

        exec.block_on(async move {
            let pool = pool_clone.clone();

            let socket1 = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind socket 1");
            let socket2 = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind socket 2");

            let addr1 = socket1.local_addr().expect("Failed to get addr1");
            let addr2 = socket2.local_addr().expect("Failed to get addr2");
            println!("Socket 1 bound to: {}", addr1);
            println!("Socket 2 bound to: {}", addr2);

            let socket1_rc = Rc::new(socket1);
            let socket2_rc = Rc::new(socket2);
            let socket1_clone = socket1_rc.clone();
            let pool_clone = pool.clone();

            // Receiver task: socket1 waits for data
            let handler = spawn_local(async move {
                let buf = alloc_buf(&pool_clone, size);
                // buf.set_len(buf.capacity());
                let (result, _buf) = socket1_clone.recv_from(buf).await;
                let (bytes_read, from_addr) = result.expect("recv_from failed");
                println!("Socket 1 received {} bytes from {}", bytes_read, from_addr);
                assert_eq!(from_addr, addr2);
            });

            // Sender: socket2 sends data to socket1
            let mut send_buf = alloc_buf(&pool, size);
            let test_data = b"Hello, UDP!";
            send_buf.spare_capacity_mut()[..test_data.len()].copy_from_slice(test_data);

            let (result, _) = socket2_rc.send_to(send_buf, addr1).await;
            let bytes_sent = result.expect("send_to failed");
            println!("Socket 2 sent {} bytes to {}", bytes_sent, addr1);

            handler.await;
        });
    }
}

/// Test UDP echo (send and receive response)
#[test]
fn test_udp_echo() {
    let pool = HybridPool::new().unwrap();
    crate::runtime::context::bind_pool(pool.clone());
    for size in [8192, 16384] {
        println!("Testing with BufferSize: {:?}", size);
        let mut exec = LocalExecutor::default();

        let pool_clone = pool.clone();

        exec.block_on(async move {
            let pool = pool_clone.clone();
            // Create server and client sockets
            let server = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind server socket");
            let client = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind client socket");

            let server_addr = server.local_addr().expect("Failed to get server address");
            let client_addr = client.local_addr().expect("Failed to get client address");
            println!("Server bound to: {}", server_addr);
            println!("Client bound to: {}", client_addr);

            let server_rc = Rc::new(server);
            let client_rc = Rc::new(client);
            let server_clone = server_rc.clone();
            let pool_server = pool.clone();

            // Server task: receive and echo back
            let server_h = spawn_local(async move {
                // Receive data
                let buf = pool_server.alloc(size).unwrap();
                let (result, buf) = server_clone.recv_from(buf).await;
                let (bytes_read, from_addr) = result.expect("Server recv_from failed");
                println!("Server received {} bytes from {}", bytes_read, from_addr);

                // Echo back
                let mut echo_buf = pool_server.alloc(size).unwrap();
                echo_buf.spare_capacity_mut()[..bytes_read as usize]
                    .copy_from_slice(&buf.as_slice()[..bytes_read as usize]);

                let (result, _) = server_clone.send_to(echo_buf, from_addr).await;
                result.expect("Server send_to failed");
                println!("Server echoed data back to {}", from_addr);
            });

            // Client: send data to server
            let mut send_buf = pool.alloc(size).unwrap();
            let test_data = b"Echo this message!";
            send_buf.spare_capacity_mut()[..test_data.len()].copy_from_slice(test_data);

            let (result, _) = client_rc.send_to(send_buf, server_addr).await;
            let bytes_sent = result.expect("Client send_to failed");
            println!("Client sent {} bytes", bytes_sent);

            // Receive echo response
            let recv_buf = pool.alloc(size).unwrap();
            let (result, recv_buf) = client_rc.recv_from(recv_buf).await;
            let (bytes_received, from_addr) = result.expect("Client recv_from failed");

            println!(
                "Client received {} bytes from {}",
                bytes_received, from_addr
            );

            // Verify
            assert_eq!(from_addr, server_addr);
            assert_eq!(&recv_buf.as_slice()[..test_data.len()], test_data);
            println!("UDP echo test successful!");

            server_h.await;
        });
    }
}

/// Test multiple UDP messages
#[test]
fn test_udp_multiple_messages() {
    let pool = HybridPool::new().unwrap();
    crate::runtime::context::bind_pool(pool.clone());
    for size in [8192, 16384] {
        let mut exec = LocalExecutor::default();
        let pool_clone = pool.clone();

        exec.block_on(async move {
            let pool = pool_clone.clone();

            let socket1 = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind socket 1");
            let socket2 = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind socket 2");

            let addr1 = socket1.local_addr().expect("Failed to get addr1");
            let _addr2 = socket2.local_addr().expect("Failed to get addr2");

            const NUM_MESSAGES: usize = 5;

            let socket1_rc = Rc::new(socket1);
            let socket2_rc = Rc::new(socket2);
            let socket1_clone = socket1_rc.clone();
            let pool_clone = pool.clone();

            // Receiver task
            let h_recv = spawn_local(async move {
                for i in 0..NUM_MESSAGES {
                    let buf = alloc_buf(&pool_clone, size);
                    let (result, _buf) = socket1_clone.recv_from(buf).await;
                    let (bytes, from) = result.expect("recv_from failed");
                    println!("Received message {} ({} bytes) from {}", i, bytes, from);
                }
                println!("Received all {} messages", NUM_MESSAGES);
            });

            // Sender
            for i in 0..NUM_MESSAGES {
                let mut buf = alloc_buf(&pool, size);
                let msg = format!("Message {}", i);
                buf.spare_capacity_mut()[..msg.len()].copy_from_slice(msg.as_bytes());

                let (result, _) = socket2_rc.send_to(buf, addr1).await;
                result.expect("send_to failed");
                println!("Sent message {}", i);
            }
            println!("Sent all {} messages", NUM_MESSAGES);

            h_recv.await;
        });
    }
}

/// Test UDP with large data
#[test]
fn test_udp_large_data() {
    let pool = HybridPool::new().unwrap();
    crate::runtime::context::bind_pool(pool.clone());
    for size in [8192, 16384] {
        let mut exec = LocalExecutor::default();
        let pool_clone = pool.clone();

        exec.block_on(async move {
            let pool = pool_clone.clone();

            let socket1 = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind socket 1");
            let socket2 = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind socket 2");

            let addr1 = socket1.local_addr().expect("Failed to get addr1");

            // UDP datagrams are limited, use a reasonable size (less than MTU)
            const DATA_SIZE: usize = 1024;

            let socket1_rc = Rc::new(socket1);
            let socket2_rc = Rc::new(socket2);
            let socket1_clone = socket1_rc.clone();
            let pool_clone = pool.clone();

            // Receiver task
            let h_recv = spawn_local(async move {
                let buf = alloc_buf(&pool_clone, size);
                let (result, buf) = socket1_clone.recv_from(buf).await;
                let (bytes, _from) = result.expect("recv_from failed");
                println!("Received {} bytes", bytes);

                // Verify data pattern
                for i in 0..DATA_SIZE {
                    assert_eq!(buf.as_slice()[i], (i % 256) as u8);
                }
                println!("Data verification successful!");
            });

            // Sender
            let mut buf = alloc_buf(&pool, size);
            for i in 0..DATA_SIZE {
                buf.spare_capacity_mut()[i] = (i % 256) as u8;
            }

            let (result, _) = socket2_rc.send_to(buf, addr1).await;
            let bytes = result.expect("send_to failed") as usize;
            println!("Sent {} bytes", bytes);

            h_recv.await;
        });
    }
}

/// Test IPv6 UDP
#[test]
fn test_udp_ipv6() {
    let mut exec = LocalExecutor::default();

    exec.block_on(async move {
        let socket_result = UdpSocket::bind("::1:0");

        if socket_result.is_err() {
            println!("IPv6 not available, skipping test");
            return;
        }

        let socket = socket_result.unwrap();
        let addr = socket.local_addr().expect("Failed to get local address");

        assert!(addr.is_ipv6());
        println!("IPv6 UDP socket bound to: {}", addr);

        drop(socket);
    });
}

// ============ Multi-Thread UDP Tests ============

/// Test UDP across multiple worker threads
#[test]
fn test_multithread_udp() {
    for size in [8192, 16384] {
        let message_count = Arc::new(AtomicUsize::new(0));
        const NUM_WORKERS: usize = 3;

        let runtime = Runtime::builder()
            .config(crate::config::Config {
                worker_threads: Some(NUM_WORKERS),
                ..Default::default()
            })
            .pool_constructor(|_| AnyBufPool::new(HybridPool::new().unwrap()))
            .build()
            .unwrap();

        let (tx, rx) = std::sync::mpsc::channel();

        let message_count_clone = message_count.clone();
        runtime.block_on(async move {
            for worker_id in 0..NUM_WORKERS {
                let counter = message_count_clone.clone();
                let tx_done = tx.clone();

                crate::runtime::context::spawn_to(
                    async move || {
                        // Each worker creates its own UDP sockets and tests send/recv
                        let socket1 =
                            UdpSocket::bind("127.0.0.1:0").expect("Failed to bind socket 1");
                        let socket2 =
                            UdpSocket::bind("127.0.0.1:0").expect("Failed to bind socket 2");

                        let addr1 = socket1.local_addr().expect("Failed to get addr1");
                        println!("Worker {} socket 1 bound to: {}", worker_id, addr1);

                        let socket1_rc = Rc::new(socket1);
                        let socket2_rc = Rc::new(socket2);
                        let socket1_clone = socket1_rc.clone();
                        let pool = crate::runtime::context::current_pool().unwrap(); // Get thread pool
                        let pool_clone = pool.clone();

                        // Receiver task via crate::spawn_local (uses current context)
                        let h_recv = crate::spawn_local(async move {
                            let buf = pool_clone.alloc(size).unwrap();
                            let (result, _buf) = socket1_clone.recv_from(buf).await;
                            result.expect("recv_from failed");
                            println!("Worker {} received message", worker_id);
                        });

                        // Sender
                        let mut buf = pool.alloc(size).unwrap();
                        let msg = format!("Hello from worker {}", worker_id);
                        buf.spare_capacity_mut()[..msg.len()].copy_from_slice(msg.as_bytes());

                        let (result, _) = socket2_rc.send_to(buf, addr1).await;
                        result.expect("send_to failed");
                        println!("Worker {} sent message", worker_id);

                        h_recv.await;
                        counter.fetch_add(1, Ordering::SeqCst);
                        tx_done.send(()).unwrap();
                    },
                    worker_id,
                );
            }

            for _ in 0..NUM_WORKERS {
                rx.recv().unwrap();
            }
        });

        assert_eq!(message_count.load(Ordering::SeqCst), NUM_WORKERS);
        println!(
            "All {} workers completed UDP self-communication",
            NUM_WORKERS
        );
    }
}

/// Test UDP echo server on one worker, clients on another
#[test]
fn test_multithread_udp_echo() {
    use std::sync::mpsc;
    use std::time::Duration;

    for size in [8192, 16384] {
        let (addr_tx, addr_rx) = mpsc::channel();
        // Wrap receiver in Arc<Mutex> so it can be shared across threads
        let addr_rx = Arc::new(Mutex::new(addr_rx));
        let runtime = Runtime::builder()
            .config(crate::config::Config::default().worker_threads(2)) // 2 workers (0 and 1)
            .pool_constructor(|_| AnyBufPool::new(HybridPool::new().unwrap()))
            .build()
            .unwrap();

        let (done_tx, done_rx) = mpsc::channel();

        // Worker 0: Echo server
        runtime.block_on(async move {
            let addr_tx = addr_tx.clone();

            crate::runtime::context::spawn_to(
                async move || {
                    let socket =
                        UdpSocket::bind("127.0.0.1:0").expect("Failed to bind server socket");
                    let server_addr = socket.local_addr().expect("Failed to get server address");
                    println!("UDP echo server listening on {}", server_addr);

                    // Send address to client worker
                    addr_tx.send(server_addr).unwrap();

                    let pool = crate::runtime::context::current_pool().unwrap();

                    // Receive and echo
                    let buf = pool.alloc(size).unwrap();

                    let (result, buf) = socket.recv_from(buf).await;
                    let (bytes, from_addr) = result.expect("Server recv_from failed");
                    println!("Server received {} bytes from {}", bytes, from_addr);

                    // Echo back
                    let mut echo_buf = pool.alloc(size).unwrap();
                    echo_buf.spare_capacity_mut()[..bytes as usize]
                        .copy_from_slice(&buf.as_slice()[..bytes as usize]);

                    let (result, _) = socket.send_to(echo_buf, from_addr).await;
                    result.expect("Server send_to failed");
                    println!("Server echoed response");
                },
                0,
            );

            // Worker 1: Client
            let addr_rx = addr_rx.clone();
            let done_tx = done_tx.clone();

            crate::runtime::context::spawn_to(
                async move || {
                    // Wait for server address
                    let server_addr = {
                        addr_rx
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(5))
                            .expect("Timeout waiting for server address")
                    };
                    println!("Client connecting to {}", server_addr);

                    let client =
                        UdpSocket::bind("127.0.0.1:0").expect("Failed to bind client socket");
                    let pool = crate::runtime::context::current_pool().unwrap();

                    // Send data
                    let mut send_buf = pool.alloc(size).unwrap();
                    let data = b"Hello from worker 2!";
                    send_buf.as_slice_mut()[..data.len()].copy_from_slice(data);

                    let (result, _) = client.send_to(send_buf, server_addr).await;
                    let sent = result.expect("Client send_to failed");
                    println!("Client sent {} bytes", sent);

                    // Receive echo
                    let recv_buf = pool.alloc(size).unwrap();
                    let (result, recv_buf) = client.recv_from(recv_buf).await;
                    let (_received, from) = result.expect("Client recv_from failed");

                    assert_eq!(from, server_addr);
                    assert_eq!(&recv_buf.as_slice()[..data.len()], data);
                    println!("Client received correct echo");

                    done_tx.send(()).unwrap();
                },
                1,
            );

            // Wait for completion
            done_rx.recv().unwrap();
        });

        println!("Multi-thread UDP echo test completed");
    }
}

/// Test concurrent UDP clients from multiple workers to shared server
#[test]
fn test_multithread_concurrent_udp_clients() {
    use std::sync::mpsc;
    use std::time::Duration;

    for size in [8192, 16384] {
        let (addr_tx, addr_rx) = mpsc::channel::<SocketAddr>();
        let addr_rx = Arc::new(Mutex::new(addr_rx));
        let message_count = Arc::new(AtomicUsize::new(0));

        const NUM_CLIENTS: usize = 3;
        const NUM_WORKERS: usize = 4; // 0=Server, 1,2,3=Clients

        let runtime = Runtime::builder()
            .config(crate::config::Config {
                worker_threads: Some(NUM_WORKERS),
                ..Default::default()
            })
            .pool_constructor(|_| AnyBufPool::new(HybridPool::new().unwrap()))
            .build()
            .unwrap();

        let (done_tx, done_rx) = mpsc::channel();

        let message_count_clone = message_count.clone();
        runtime.block_on(async move {
            // Server worker (0)
            let addr_tx = addr_tx.clone();
            crate::runtime::context::spawn_to(
                async move || {
                    let socket =
                        UdpSocket::bind("127.0.0.1:0").expect("Failed to bind server socket");
                    let server_addr = socket.local_addr().expect("Failed to get server address");
                    println!("Server listening on {}", server_addr);

                    // Broadcast address to all clients
                    for _ in 0..NUM_CLIENTS {
                        addr_tx.send(server_addr).unwrap();
                    }

                    let pool = crate::runtime::context::current_pool().unwrap();

                    // Receive messages from all clients
                    for i in 0..NUM_CLIENTS {
                        let buf = pool.alloc(size).unwrap();
                        let (result, _buf) = socket.recv_from(buf).await;
                        let (bytes, from) = result.expect("Server recv_from failed");
                        println!(
                            "Server received message {} ({} bytes) from {}",
                            i, bytes, from
                        );
                    }
                    println!("Server received all {} messages", NUM_CLIENTS);
                },
                0,
            );

            let counter_clone = message_count_clone.clone();
            // Client workers (1..=3)
            for client_id in 1..=NUM_CLIENTS {
                let rx = addr_rx.clone();
                let counter = counter_clone.clone();
                let done_tx = done_tx.clone();

                crate::runtime::context::spawn_to(
                    async move || {
                        let server_addr = {
                            rx.lock()
                                .unwrap()
                                .recv_timeout(Duration::from_secs(5))
                                .expect("Timeout waiting for server address")
                        };

                        let client =
                            UdpSocket::bind("127.0.0.1:0").expect("Failed to bind client socket");
                        let pool = crate::runtime::context::current_pool().unwrap();

                        let mut buf = pool.alloc(size).unwrap();
                        let msg = format!("Hello from client {}", client_id);
                        buf.as_slice_mut()[..msg.len()].copy_from_slice(msg.as_bytes());

                        let (result, _) = client.send_to(buf, server_addr).await;
                        result.expect("Client send_to failed");
                        println!("Client {} sent message", client_id);

                        counter.fetch_add(1, Ordering::SeqCst);
                        done_tx.send(()).unwrap();
                    },
                    client_id,
                );
            }

            // Wait for clients
            for _ in 0..NUM_CLIENTS {
                done_rx.recv().unwrap();
            }
        });

        assert_eq!(message_count.load(Ordering::SeqCst), NUM_CLIENTS);
        println!("All {} clients completed", NUM_CLIENTS);
    }
}
