//! UDP network tests - single-threaded and multi-threaded.

use crate::runtime::buffer::{BufferPool, FixedBuf};
use crate::runtime::executor::{LocalExecutor, Runtime};
use crate::runtime::net::udp::UdpSocket;
use crate::runtime::{current_driver, spawn};
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ============ Helper Functions ============

/// Helper function to allocate a buffer from a pool
fn alloc_buf(pool: &BufferPool) -> FixedBuf {
    pool.alloc().expect("Failed to allocate buffer from pool")
}

// ============ Single-Thread UDP Tests ============

/// Test basic UDP socket binding and local_addr
#[test]
fn test_udp_bind() {
    let exec = LocalExecutor::new();
    let driver = exec.driver_handle();

    exec.block_on(async move {
        let socket =
            UdpSocket::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind UDP socket");

        let addr = socket.local_addr().expect("Failed to get local address");

        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_ne!(addr.port(), 0);

        println!("UDP socket bound to: {}", addr);
    });
}

/// Test UDP send and receive
#[test]
fn test_udp_send_recv() {
    let exec = LocalExecutor::new();
    let driver = exec.driver_handle();
    let pool = Rc::new(BufferPool::new());

    let socket1 = UdpSocket::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind socket 1");
    let socket2 = UdpSocket::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind socket 2");

    let addr1 = socket1.local_addr().expect("Failed to get addr1");
    let addr2 = socket2.local_addr().expect("Failed to get addr2");
    println!("Socket 1 bound to: {}", addr1);
    println!("Socket 2 bound to: {}", addr2);

    let socket1_rc = Rc::new(socket1);
    let socket2_rc = Rc::new(socket2);
    let socket1_clone = socket1_rc.clone();
    let pool_clone = pool.clone();

    exec.block_on(async move {
        // Receiver task: socket1 waits for data
        spawn(async move {
            let mut buf = alloc_buf(&pool_clone);
            buf.set_len(buf.capacity());
            let (result, _buf) = socket1_clone.recv_from(buf).await;
            let (bytes_read, from_addr) = result.expect("recv_from failed");
            println!("Socket 1 received {} bytes from {}", bytes_read, from_addr);
            assert_eq!(from_addr, addr2);
        });

        // Sender: socket2 sends data to socket1
        let mut send_buf = alloc_buf(&pool);
        let test_data = b"Hello, UDP!";
        send_buf.spare_capacity_mut()[..test_data.len()].copy_from_slice(test_data);
        send_buf.set_len(test_data.len());

        let (result, _) = socket2_rc.send_to(send_buf, addr1).await;
        let bytes_sent = result.expect("send_to failed");
        println!("Socket 2 sent {} bytes to {}", bytes_sent, addr1);
    });
}

/// Test UDP echo (send and receive response)
#[test]
fn test_udp_echo() {
    let exec = LocalExecutor::new();
    let driver = exec.driver_handle();

    // Create server and client sockets
    let server =
        UdpSocket::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind server socket");
    let client =
        UdpSocket::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind client socket");

    let server_addr = server.local_addr().expect("Failed to get server address");
    let client_addr = client.local_addr().expect("Failed to get client address");
    println!("Server bound to: {}", server_addr);
    println!("Client bound to: {}", client_addr);

    let server_rc = Rc::new(server);
    let client_rc = Rc::new(client);
    let server_clone = server_rc.clone();

    exec.block_on(async move {
        let driver_weak = current_driver();
        let driver_inner = driver_weak.upgrade().unwrap();

        // Server task: receive and echo back
        spawn(async move {
            let driver = current_driver().upgrade().unwrap();

            // Receive data
            let mut buf = driver.borrow().alloc_fixed_buffer().unwrap();
            buf.set_len(buf.capacity());
            let (result, buf) = server_clone.recv_from(buf).await;
            let (bytes_read, from_addr) = result.expect("Server recv_from failed");
            println!("Server received {} bytes from {}", bytes_read, from_addr);

            // Echo back
            let mut echo_buf = driver.borrow().alloc_fixed_buffer().unwrap();
            echo_buf.spare_capacity_mut()[..bytes_read as usize]
                .copy_from_slice(&buf.as_slice()[..bytes_read as usize]);
            echo_buf.set_len(bytes_read as usize);

            let (result, _) = server_clone.send_to(echo_buf, from_addr).await;
            result.expect("Server send_to failed");
            println!("Server echoed data back to {}", from_addr);
        });

        // Client: send data to server
        let mut send_buf = driver_inner.borrow().alloc_fixed_buffer().unwrap();
        let test_data = b"Echo this message!";
        send_buf.spare_capacity_mut()[..test_data.len()].copy_from_slice(test_data);
        send_buf.set_len(test_data.len());

        let (result, _) = client_rc.send_to(send_buf, server_addr).await;
        let bytes_sent = result.expect("Client send_to failed");
        println!("Client sent {} bytes", bytes_sent);

        // Receive echo response
        let mut recv_buf = driver_inner.borrow().alloc_fixed_buffer().unwrap();
        recv_buf.set_len(recv_buf.capacity());
        let (result, recv_buf) = client_rc.recv_from(recv_buf).await;
        let (bytes_received, from_addr) = result.expect("Client recv_from failed");

        println!(
            "Client received {} bytes from {}",
            bytes_received, from_addr
        );

        // Verify
        assert_eq!(from_addr, server_addr);
        assert_eq!(bytes_sent as u32, bytes_received);
        assert_eq!(&recv_buf.as_slice()[..bytes_received as usize], test_data);
        println!("UDP echo test successful!");
    });
}

/// Test multiple UDP messages
#[test]
fn test_udp_multiple_messages() {
    let exec = LocalExecutor::new();
    let driver = exec.driver_handle();
    let pool = Rc::new(BufferPool::new());

    let socket1 = UdpSocket::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind socket 1");
    let socket2 = UdpSocket::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind socket 2");

    let addr1 = socket1.local_addr().expect("Failed to get addr1");
    let _addr2 = socket2.local_addr().expect("Failed to get addr2");

    const NUM_MESSAGES: usize = 5;

    let socket1_rc = Rc::new(socket1);
    let socket2_rc = Rc::new(socket2);
    let socket1_clone = socket1_rc.clone();
    let pool_clone = pool.clone();

    exec.block_on(async move {
        // Receiver task
        spawn(async move {
            for i in 0..NUM_MESSAGES {
                let mut buf = alloc_buf(&pool_clone);
                buf.set_len(buf.capacity());
                let (result, _buf) = socket1_clone.recv_from(buf).await;
                let (bytes, from) = result.expect("recv_from failed");
                println!("Received message {} ({} bytes) from {}", i, bytes, from);
            }
            println!("Received all {} messages", NUM_MESSAGES);
        });

        // Sender
        for i in 0..NUM_MESSAGES {
            let mut buf = alloc_buf(&pool);
            let msg = format!("Message {}", i);
            buf.spare_capacity_mut()[..msg.len()].copy_from_slice(msg.as_bytes());
            buf.set_len(msg.len());

            let (result, _) = socket2_rc.send_to(buf, addr1).await;
            result.expect("send_to failed");
            println!("Sent message {}", i);
        }
        println!("Sent all {} messages", NUM_MESSAGES);
    });
}

/// Test UDP with large data
#[test]
fn test_udp_large_data() {
    let exec = LocalExecutor::new();
    let driver = exec.driver_handle();
    let pool = Rc::new(BufferPool::new());

    let socket1 = UdpSocket::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind socket 1");
    let socket2 = UdpSocket::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind socket 2");

    let addr1 = socket1.local_addr().expect("Failed to get addr1");

    // UDP datagrams are limited, use a reasonable size (less than MTU)
    const DATA_SIZE: usize = 1024;

    let socket1_rc = Rc::new(socket1);
    let socket2_rc = Rc::new(socket2);
    let socket1_clone = socket1_rc.clone();
    let pool_clone = pool.clone();

    exec.block_on(async move {
        // Receiver task
        spawn(async move {
            let mut buf = alloc_buf(&pool_clone);
            buf.set_len(buf.capacity());
            let (result, buf) = socket1_clone.recv_from(buf).await;
            let (bytes, _from) = result.expect("recv_from failed");

            assert_eq!(bytes as usize, DATA_SIZE);
            println!("Received {} bytes", bytes);

            // Verify data pattern
            for i in 0..DATA_SIZE {
                assert_eq!(buf.as_slice()[i], (i % 256) as u8);
            }
            println!("Data verification successful!");
        });

        // Sender
        let mut buf = alloc_buf(&pool);
        for i in 0..DATA_SIZE {
            buf.spare_capacity_mut()[i] = (i % 256) as u8;
        }
        buf.set_len(DATA_SIZE);

        let (result, _) = socket2_rc.send_to(buf, addr1).await;
        let bytes = result.expect("send_to failed") as usize;
        assert_eq!(bytes, DATA_SIZE);
        println!("Sent {} bytes", bytes);
    });
}

/// Test IPv6 UDP
#[test]
fn test_udp_ipv6() {
    let exec = LocalExecutor::new();
    let driver = exec.driver_handle();

    let socket_result = UdpSocket::bind("::1:0", driver.clone());

    if socket_result.is_err() {
        println!("IPv6 not available, skipping test");
        return;
    }

    let socket = socket_result.unwrap();
    let addr = socket.local_addr().expect("Failed to get local address");

    assert!(addr.is_ipv6());
    println!("IPv6 UDP socket bound to: {}", addr);

    drop(socket);
}

// ============ Multi-Thread UDP Tests ============

/// Test UDP across multiple worker threads
#[test]
fn test_multithread_udp() {
    let message_count = Arc::new(AtomicUsize::new(0));
    let mut runtime = Runtime::new();

    const NUM_WORKERS: usize = 3;

    for worker_id in 0..NUM_WORKERS {
        let counter = message_count.clone();
        runtime.spawn_worker(move || async move {
            let driver = current_driver();

            // Each worker creates its own UDP sockets and tests send/recv
            let socket1 =
                UdpSocket::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind socket 1");
            let socket2 =
                UdpSocket::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind socket 2");

            let addr1 = socket1.local_addr().expect("Failed to get addr1");
            println!("Worker {} socket 1 bound to: {}", worker_id, addr1);

            let socket1_rc = Rc::new(socket1);
            let socket2_rc = Rc::new(socket2);
            let socket1_clone = socket1_rc.clone();

            // Receiver task
            spawn(async move {
                let driver = current_driver().upgrade().unwrap();
                let mut buf = driver.borrow().alloc_fixed_buffer().unwrap();
                buf.set_len(buf.capacity());
                let (result, _buf) = socket1_clone.recv_from(buf).await;
                result.expect("recv_from failed");
                println!("Worker {} received message", worker_id);
            });

            // Sender
            let driver_rc = driver.upgrade().unwrap();
            let mut buf = driver_rc.borrow().alloc_fixed_buffer().unwrap();
            let msg = format!("Hello from worker {}", worker_id);
            buf.spare_capacity_mut()[..msg.len()].copy_from_slice(msg.as_bytes());
            buf.set_len(msg.len());

            let (result, _) = socket2_rc.send_to(buf, addr1).await;
            result.expect("send_to failed");
            println!("Worker {} sent message", worker_id);

            counter.fetch_add(1, Ordering::SeqCst);
        });
    }

    runtime.block_on_all();

    assert_eq!(message_count.load(Ordering::SeqCst), NUM_WORKERS);
    println!(
        "All {} workers completed UDP self-communication",
        NUM_WORKERS
    );
}

/// Test UDP echo server on one worker, clients on another
#[test]
fn test_multithread_udp_echo() {
    use std::sync::mpsc;
    use std::time::Duration;

    let (addr_tx, addr_rx) = mpsc::channel();
    let mut runtime = Runtime::new();

    // Worker 1: Echo server
    runtime.spawn_worker(move || async move {
        let driver = current_driver();

        let socket =
            UdpSocket::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind server socket");
        let server_addr = socket.local_addr().expect("Failed to get server address");
        println!("UDP echo server listening on {}", server_addr);

        // Send address to client worker
        addr_tx.send(server_addr).unwrap();

        // Receive and echo
        let driver_rc = driver.upgrade().unwrap();
        let mut buf = driver_rc.borrow().alloc_fixed_buffer().unwrap();
        buf.set_len(buf.capacity());

        let (result, buf) = socket.recv_from(buf).await;
        let (bytes, from_addr) = result.expect("Server recv_from failed");
        println!("Server received {} bytes from {}", bytes, from_addr);

        // Echo back
        let mut echo_buf = driver_rc.borrow().alloc_fixed_buffer().unwrap();
        echo_buf.spare_capacity_mut()[..bytes as usize]
            .copy_from_slice(&buf.as_slice()[..bytes as usize]);
        echo_buf.set_len(bytes as usize);

        let (result, _) = socket.send_to(echo_buf, from_addr).await;
        result.expect("Server send_to failed");
        println!("Server echoed response");
    });

    // Worker 2: Client
    runtime.spawn_worker(move || async move {
        // Wait for server address
        let server_addr = addr_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("Timeout waiting for server address");
        println!("Client connecting to {}", server_addr);

        let driver = current_driver();
        let driver_rc = driver.upgrade().unwrap();

        let client =
            UdpSocket::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind client socket");

        // Send data
        let mut send_buf = driver_rc.borrow().alloc_fixed_buffer().unwrap();
        let data = b"Hello from worker 2!";
        send_buf.spare_capacity_mut()[..data.len()].copy_from_slice(data);
        send_buf.set_len(data.len());

        let (result, _) = client.send_to(send_buf, server_addr).await;
        let sent = result.expect("Client send_to failed");
        println!("Client sent {} bytes", sent);

        // Receive echo
        let mut recv_buf = driver_rc.borrow().alloc_fixed_buffer().unwrap();
        recv_buf.set_len(recv_buf.capacity());
        let (result, recv_buf) = client.recv_from(recv_buf).await;
        let (received, from) = result.expect("Client recv_from failed");

        assert_eq!(from, server_addr);
        assert_eq!(&recv_buf.as_slice()[..received as usize], data);
        println!("Client received correct echo");
    });

    runtime.block_on_all();
    println!("Multi-thread UDP echo test completed");
}

/// Test concurrent UDP clients from multiple workers to shared server
#[test]
fn test_multithread_concurrent_udp_clients() {
    use std::sync::mpsc;
    use std::time::Duration;

    let (addr_tx, addr_rx) = mpsc::channel::<SocketAddr>();
    let addr_rx = Arc::new(Mutex::new(addr_rx));
    let message_count = Arc::new(AtomicUsize::new(0));
    let mut runtime = Runtime::new();

    const NUM_CLIENTS: usize = 3;

    // Server worker
    runtime.spawn_worker(move || async move {
        let driver = current_driver();

        let socket =
            UdpSocket::bind("127.0.0.1:0", driver.clone()).expect("Failed to bind server socket");
        let server_addr = socket.local_addr().expect("Failed to get server address");
        println!("Server listening on {}", server_addr);

        // Broadcast address to all clients
        for _ in 0..NUM_CLIENTS {
            addr_tx.send(server_addr).unwrap();
        }

        let driver_rc = driver.upgrade().unwrap();

        // Receive messages from all clients
        for i in 0..NUM_CLIENTS {
            let mut buf = driver_rc.borrow().alloc_fixed_buffer().unwrap();
            buf.set_len(buf.capacity());
            let (result, _buf) = socket.recv_from(buf).await;
            let (bytes, from) = result.expect("Server recv_from failed");
            println!(
                "Server received message {} ({} bytes) from {}",
                i, bytes, from
            );
        }
        println!("Server received all {} messages", NUM_CLIENTS);
    });

    // Client workers
    for client_id in 0..NUM_CLIENTS {
        let rx = addr_rx.clone();
        let counter = message_count.clone();
        runtime.spawn_worker(move || async move {
            // Get address from shared receiver
            let server_addr = {
                let rx_guard = rx.lock().unwrap();
                rx_guard
                    .recv_timeout(Duration::from_secs(5))
                    .expect("Timeout waiting for server address")
            };

            let driver = current_driver();
            let driver_rc = driver.upgrade().unwrap();

            let client = UdpSocket::bind("127.0.0.1:0", driver.clone())
                .expect("Failed to bind client socket");

            let mut buf = driver_rc.borrow().alloc_fixed_buffer().unwrap();
            let msg = format!("Hello from client {}", client_id);
            buf.spare_capacity_mut()[..msg.len()].copy_from_slice(msg.as_bytes());
            buf.set_len(msg.len());

            let (result, _) = client.send_to(buf, server_addr).await;
            result.expect("Client send_to failed");
            println!("Client {} sent message", client_id);

            counter.fetch_add(1, Ordering::SeqCst);
        });
    }

    runtime.block_on_all();

    assert_eq!(message_count.load(Ordering::SeqCst), NUM_CLIENTS);
    println!("All {} clients completed", NUM_CLIENTS);
}
