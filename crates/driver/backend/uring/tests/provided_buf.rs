//! provided buffer ring 的驱动级测试。
//!
//! 每个用例都会在缺 `IORING_REGISTER_PBUF_RING` 的内核（< 5.19）上自行跳过——那是仓库声明
//! 支持的区间的一部分，不是失败。

use std::{
    num::{NonZeroU16, NonZeroU32, NonZeroUsize},
    os::fd::RawFd,
    time::{Duration, Instant},
};

use veloq_buf::{AnyBufPool, BufPool, FixedBuf, NoopRegistrar};
use veloq_driver_core::{
    driver::{
        CompletionRecord, DriveMode, Driver, DriverSubmitResult, OpToken, PollRecordResult,
        RegisterFd,
    },
    op::{IntoPlatformOp, types::RecvProvided as CoreRecvProvided},
};
use veloq_driver_uring::{
    IoFd, ProvidedBufConfig, RawHandle, UringConfig, UringDriver, UringOp, UringRawHandle,
    UringSlotSpec, UringUserPayload,
};

type RecvProvided = CoreRecvProvided<UringRawHandle>;

/// 一个只会走堆分配的池。
///
/// 这些用例关心的是环的记账，不是池的实现——用最朴素的那个，失败就一定是环的问题。
#[derive(Debug, Clone)]
struct HeapPool;

impl BufPool for HeapPool {
    fn alloc(&self, len: NonZeroUsize) -> Option<FixedBuf> {
        FixedBuf::alloc_heap(len).ok()
    }
}

fn new_driver_or_skip(entries: u16, buf_size: usize) -> Option<UringDriver<'static>> {
    let config = UringConfig {
        entries: NonZeroU32::new(64).unwrap(),
        provided_buffers: Some(ProvidedBufConfig {
            entries: NonZeroU16::new(entries).unwrap(),
            buf_size: NonZeroUsize::new(buf_size).unwrap(),
        }),
        ..UringConfig::default()
    };
    static REGISTRAR: NoopRegistrar = NoopRegistrar;

    let mut driver = match UringDriver::new(config, &REGISTRAR) {
        Ok(driver) => driver,
        Err(report) => {
            eprintln!("skipping provided-buffer test: {report}");
            return None;
        }
    };
    driver
        .attach_buffer_pool(AnyBufPool::new(HeapPool))
        .expect("attaching a buffer pool must not fail");

    if !driver.capabilities().provided_buffers {
        eprintln!("skipping provided-buffer test: kernel has no IORING_REGISTER_PBUF_RING");
        return None;
    }
    Some(driver)
}

/// 一对已连接的 `AF_UNIX` socket，省掉一整套 TCP 握手。
struct SocketPair {
    rx: RawFd,
    tx: RawFd,
}

impl SocketPair {
    fn new() -> Self {
        let mut fds = [0i32; 2];
        // SAFETY: `fds` 是一个长度为 2 的数组，正是 `socketpair` 要写入的形状。
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(
            rc,
            0,
            "socketpair failed: {}",
            std::io::Error::last_os_error()
        );
        Self {
            rx: fds[0],
            tx: fds[1],
        }
    }

    fn send(&self, data: &[u8]) {
        // SAFETY: `data` 是一段活着的切片，`tx` 是本结构持有的 fd。
        let written = unsafe { libc::write(self.tx, data.as_ptr().cast(), data.len()) };
        assert_eq!(written, data.len() as isize, "short write to socketpair");
    }

    fn register_rx(&self, driver: &mut UringDriver<'static>) -> IoFd {
        let raw = RawHandle::new(UringRawHandle::for_socket(self.rx));
        driver
            .register_files(vec![RegisterFd::Borrowed(raw.borrow())])
            .expect("registering the receiving socket")
            .into_iter()
            .next()
            .expect("register_files returned nothing")
    }
}

impl Drop for SocketPair {
    fn drop(&mut self) {
        // SAFETY: 两个 fd 都由本结构拥有，且只在这里关闭一次。
        unsafe {
            libc::close(self.rx);
            libc::close(self.tx);
        }
    }
}

fn submit_recv_provided(driver: &mut UringDriver<'static>, fd: IoFd) -> OpToken {
    let (kernel, payload) =
        <RecvProvided as IntoPlatformOp<UringSlotSpec>>::into_kernel_and_payload(RecvProvided {
            fd,
        });
    let mut op: Option<UringOp> = Some(kernel);
    let mut slot = driver.reserve_op().expect("reserve op failed");
    slot.set_payload(<RecvProvided as IntoPlatformOp<UringSlotSpec>>::payload_into_erased(payload));
    match slot.submit(&mut op) {
        DriverSubmitResult::Submitted(_) => {
            let token = slot.persist().token();
            driver.completion_table().mark_waiting(token);
            token
        }
        DriverSubmitResult::Failed { report, status } => {
            panic!("submit RecvProvided failed: status={status:?}, error={report}")
        }
    }
}

/// 取一条完成，返回 `(CQE 结果, 内核挑给它的 buffer)`。
fn take_completion(driver: &mut UringDriver<'static>, token: OpToken) -> (i32, Option<FixedBuf>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "provided-buffer recv timed out");
        driver.drive(DriveMode::Poll).expect("drive failed");
        let table = driver.completion_table();
        match table.try_take_record(token).unwrap() {
            PollRecordResult::Ready(record) => {
                let CompletionRecord {
                    event,
                    payload,
                    mut cleanup,
                    ..
                } = record;
                cleanup.disarm();
                let buf = match payload {
                    UringUserPayload::ProvidedBuf(provided) => provided.buf,
                    other => panic!(
                        "a RecvProvided completion must carry a ProvidedBuf, got kind {:?}",
                        std::mem::discriminant(&other)
                    ),
                };
                return (event.res(), buf);
            }
            PollRecordResult::Unavailable { kind, .. } => {
                panic!("completion record unavailable: {kind:?}")
            }
            PollRecordResult::Pending => std::thread::sleep(Duration::from_millis(2)),
        }
    }
}

/// 连续收 N 次（N 远大于环的深度），每次都拿到数据，环始终填得回来。
///
/// 这条用例锚定的是「移交 + 补充」：交出去的 buffer 不回环，回环的是从池里新取的一个。做
/// 成「借出 + 归还」的话前 4 次一样绿，第 5 次就没 buffer 了。
#[test]
fn provided_buffers_are_recycled_after_each_completion() {
    const ENTRIES: u16 = 4;
    const ROUNDS: usize = 16;

    let Some(mut driver) = new_driver_or_skip(ENTRIES, 256) else {
        return;
    };
    let sock = SocketPair::new();
    let fd = sock.register_rx(&mut driver);

    for round in 0..ROUNDS {
        let payload = format!("round-{round}");
        sock.send(payload.as_bytes());

        let token = submit_recv_provided(&mut driver, fd);
        let (res, buf) = take_completion(&mut driver, token);

        assert_eq!(
            res,
            payload.len() as i32,
            "round {round} read the wrong length"
        );
        let buf = buf.unwrap_or_else(|| panic!("round {round} produced no buffer"));
        assert_eq!(buf.as_slice(), payload.as_bytes(), "round {round} content");
        assert_eq!(
            buf.capacity(),
            256,
            "the ring must hand out its own buffers"
        );
    }

    let stats = driver.provided_buf_stats().expect("the ring is registered");
    assert_eq!(stats.handed_out, ROUNDS as u64);
    // 初始填满 ENTRIES 个，之后每交出一个补一个。
    assert_eq!(stats.refilled, ENTRIES as u64 + ROUNDS as u64);
    assert_eq!(stats.refill_failed, 0);
    assert_eq!(
        stats.available, ENTRIES,
        "every consumed entry must have been replaced"
    );

    driver.unregister_files(vec![fd]).unwrap();
}

/// 环被掏空时内核报 `-ENOBUFS`，而不是随便写进别人的内存。
///
/// 单发 recv 对此没有特殊处理——那一次以错误完成，用户可以重试。这里断言的是错误确实到得
/// 了用户手上，并且被记进了诊断。
#[test]
fn an_exhausted_ring_completes_with_enobufs() {
    const ENTRIES: u16 = 2;
    const CONCURRENT: usize = 4;

    let Some(mut driver) = new_driver_or_skip(ENTRIES, 256) else {
        return;
    };

    let socks: Vec<SocketPair> = (0..CONCURRENT).map(|_| SocketPair::new()).collect();
    let fds: Vec<IoFd> = socks.iter().map(|s| s.register_rx(&mut driver)).collect();

    // 先把数据都准备好，再一次性提交：内核在处理每条 SQE 时就会取走一个 buffer，而补充要
    // 等我们收割 CQE，所以第 ENTRIES + 1 条起必然找不到 buffer。
    for sock in &socks {
        sock.send(b"x");
    }
    let tokens: Vec<OpToken> = fds
        .iter()
        .map(|&fd| submit_recv_provided(&mut driver, fd))
        .collect();

    let mut delivered = 0usize;
    let mut enobufs = 0usize;
    for token in tokens {
        let (res, buf) = take_completion(&mut driver, token);
        if res == -libc::ENOBUFS {
            assert!(buf.is_none(), "an ENOBUFS completion cannot carry a buffer");
            enobufs += 1;
        } else {
            assert_eq!(res, 1);
            assert!(buf.is_some(), "a successful recv must carry its buffer");
            delivered += 1;
        }
    }

    assert!(
        delivered >= ENTRIES as usize,
        "the ring must serve at least its own depth, served {delivered}"
    );
    assert!(
        enobufs > 0,
        "{CONCURRENT} concurrent recvs against a ring of {ENTRIES} must exhaust it"
    );

    let stats = driver.provided_buf_stats().expect("the ring is registered");
    assert_eq!(stats.exhausted, enobufs as u64);
    assert_eq!(
        stats.available, ENTRIES,
        "the ring must be full again once every completion has been processed"
    );

    driver.unregister_files(fds).unwrap();
}

/// 一条被放弃的 recv 收到完成时，它选中的 buffer 要原样还回环。
///
/// 「取消不等于结束」在 provided buffer 上的形态：内核已经把 buffer 从环里取走并写好了
/// 数据，而这条完成没人要。不还回去的话每取消一次环就永久少一个 bid——几次之后所有 recv
/// 都会 `-ENOBUFS`，而现场看起来像是「消费方太慢」。
#[test]
fn a_discarded_completion_returns_its_buffer_to_the_ring() {
    const ENTRIES: u16 = 2;

    let Some(mut driver) = new_driver_or_skip(ENTRIES, 256) else {
        return;
    };
    let sock = SocketPair::new();
    let fd = sock.register_rx(&mut driver);

    // 数据还没来就放弃这次 recv：slot 转 `InFlightOrphaned`，token 仍然有效。
    let token = submit_recv_provided(&mut driver, fd);
    driver.drive(DriveMode::Poll).expect("drive failed");
    driver.completion_table().mark_orphaned(token);

    // 现在才把数据送来：内核取走一个 buffer、投递一条谁也不要的完成。
    sock.send(b"orphaned");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        driver.drive(DriveMode::Poll).expect("drive failed");
        let stats = driver.provided_buf_stats().expect("the ring is registered");
        if stats.returned > 0 {
            assert_eq!(stats.handed_out, 0, "nobody consumed this completion");
            assert_eq!(
                stats.available, ENTRIES,
                "a discarded completion must leave the ring full"
            );
            // 还回环用的是原来那个 buffer，不是新从池里取的。
            assert_eq!(stats.refilled, ENTRIES as u64);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the orphaned completion never arrived"
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    // 环没被削短：后面的 recv 照常拿得到 buffer。
    sock.send(b"after");
    let token = submit_recv_provided(&mut driver, fd);
    let (res, buf) = take_completion(&mut driver, token);
    assert_eq!(res, b"after".len() as i32);
    assert_eq!(buf.expect("buffer after an orphan").as_slice(), b"after");

    driver.unregister_files(vec![fd]).unwrap();
}
