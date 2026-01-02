use super::*;
use crate::io::buffer::{BufferPool, BufferSize};
use crate::io::op::{Accept, Connect, IoOp, IoResources, OpLifecycle, Recv, Timeout};
use crate::io::socket::Socket;
use std::net::TcpListener;
use std::os::windows::io::IntoRawSocket;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use windows_sys::Win32::Foundation::HANDLE;

fn noop_waker() -> Waker {
    unsafe fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &VTABLE)
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn wake_by_ref(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}

#[test]
fn test_extensions_load() {
    let ext = Extensions::new();
    assert!(ext.is_ok(), "Extensions should load on Windows");
}

#[test]
fn test_driver_creation() {
    let driver = IocpDriver::new(&crate::config::Config::default());
    assert!(driver.is_ok(), "Driver should be created");
}

#[test]
fn test_iocp_accept() {
    let mut driver =
        IocpDriver::new(&crate::config::Config::default()).expect("Driver creation failed");

    // Listener (Bind to random port)
    let std_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    let listener_handle = std_listener.into_raw_socket() as HANDLE;

    // Acceptor
    let acceptor = Socket::new_tcp_v4().expect("Acceptor socket creation failed");
    let acceptor_handle = acceptor.into_raw();

    // Prepare Accept Op
    // Accept::into_op expects pre-alloc (accept_socket) which is SysRawOp (*mut c_void)
    let accept_op = Accept::into_op(listener_handle as _, acceptor_handle as usize as _);
    let resources = IoResources::Accept(accept_op);
    let user_data = driver.reserve_op();
    driver.submit_op_resources(user_data, resources);

    // Connect Client in background
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::net::TcpStream::connect(addr).expect("Client connect failed");
    });

    // Poll
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > std::time::Duration::from_secs(5) {
            panic!("Test timed out");
        }

        driver.process_completions();

        match driver.poll_op(user_data, &mut cx) {
            Poll::Ready((res, resources)) => {
                assert!(res.is_ok(), "Accept failed: {:?}", res.err());
                match resources {
                    IoResources::Accept(op) => {
                        assert!(op.remote_addr.is_some(), "Remote addr should be populated");
                        unsafe {
                            if let Some(fd) = op.fd.raw() {
                                windows_sys::Win32::Foundation::CloseHandle(fd as _);
                            }
                            windows_sys::Win32::Foundation::CloseHandle(op.accept_socket as _);
                        }
                        break;
                    }
                    _ => panic!("Wrong resource type returned"),
                }
            }
            Poll::Pending => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

#[test]
fn test_iocp_connect() {
    let mut driver = IocpDriver::new(&crate::config::Config::default()).unwrap();

    // Listener
    let std_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();

    // Client Socket
    let client = Socket::new_tcp_v4().unwrap();
    let client_handle = client.into_raw();

    // Create Connect Op manually as it doesn't have into_op
    use crate::io::socket::socket_addr_trans;
    let (addr_buf, addr_len) = socket_addr_trans(addr);

    let connect_op = Connect {
        fd: crate::io::op::IoFd::Raw(client_handle),
        addr: addr_buf.into_boxed_slice(),
        addr_len: addr_len as u32,
    };

    // Connect implements IoOp
    let resources = connect_op.into_resource();
    let user_data = driver.reserve_op();
    driver.submit_op_resources(user_data, resources);

    // Poll
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > std::time::Duration::from_secs(5) {
            panic!("Connect Timed out");
        }
        driver.process_completions();
        match driver.poll_op(user_data, &mut cx) {
            Poll::Ready((res, _)) => {
                assert!(res.is_ok(), "Connect failed: {:?}", res.err());
                unsafe { windows_sys::Win32::Foundation::CloseHandle(client_handle as _) };
                break;
            }
            Poll::Pending => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
}

#[test]
fn test_iocp_timeout() {
    let mut driver = IocpDriver::new(&crate::config::Config::default()).unwrap();

    let timeout_op = Timeout {
        duration: std::time::Duration::from_millis(100),
    };

    let resources = IoResources::Timeout(timeout_op);
    let user_data = driver.reserve_op();
    driver.submit_op_resources(user_data, resources);

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let start = std::time::Instant::now();

    loop {
        // Safety timeout
        if start.elapsed() > std::time::Duration::from_secs(1) {
            panic!("Timeout Op didn't complete in time");
        }

        driver.process_completions();

        match driver.poll_op(user_data, &mut cx) {
            Poll::Ready((res, _)) => {
                assert!(res.is_ok(), "Timeout should succeed");
                let elapsed = start.elapsed();
                assert!(
                    elapsed >= std::time::Duration::from_millis(50),
                    "Should wait at least ~100ms, got {:?}",
                    elapsed
                );
                break;
            }
            Poll::Pending => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

#[test]
fn test_iocp_recv_with_buffer_pool() {
    let mut driver = IocpDriver::new(&crate::config::Config::default()).unwrap();
    let pool = BufferPool::new();

    // Setup connection
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let client_thread = std::thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        use std::io::Write;
        stream.write_all(b"Hello Buffer").unwrap();
    });

    let (stream, _) = listener.accept().unwrap();
    let stream_handle = stream.into_raw_socket() as HANDLE;

    // Alloc buffer
    let buf = pool
        .alloc(BufferSize::Size4K)
        .expect("Failed to alloc buffer");

    // Create Recv Op
    let recv_op = Recv {
        fd: crate::io::op::IoFd::Raw(stream_handle),
        buf,
    };

    let resources = recv_op.into_resource();
    let user_data = driver.reserve_op();
    driver.submit_op_resources(user_data, resources);

    // Poll
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > std::time::Duration::from_secs(5) {
            panic!("Recv timed out");
        }
        driver.process_completions();

        match driver.poll_op(user_data, &mut cx) {
            Poll::Ready((res, resources)) => {
                assert!(res.is_ok(), "Recv failed: {:?}", res.err());
                let bytes_read = res.unwrap();
                assert_eq!(bytes_read, 12);

                if let IoResources::Recv(mut op) = resources {
                    op.buf.set_len(bytes_read);
                    assert_eq!(&op.buf.as_slice()[..12], b"Hello Buffer");
                } else {
                    panic!("Wrong resource type");
                }

                unsafe { windows_sys::Win32::Foundation::CloseHandle(stream_handle) };
                break;
            }
            Poll::Pending => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
    client_thread.join().unwrap();
}
