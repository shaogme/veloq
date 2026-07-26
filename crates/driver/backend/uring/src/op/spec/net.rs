use crate::{
    OwnedRawHandle,
    driver::{CqeEnv, SqeEnv},
    error::UringResult,
    op::{
        Accept, AcceptMulti, AcceptedSocket, Connect, OpSend, ProvidedBuf, Recv, RecvProvided,
        SendTo, UdpConnect, UdpRecv, UdpRecvFrom, UdpSend, UringUserPayload, payload, submit,
    },
};
use io_uring::squeue;
use veloq_driver_core::{
    driver::{CompletionCleanupGuard, SubmitTokenContext},
    op::OpKind,
};

use super::UringOpSpec;

impl UringOpSpec for Recv {
    type KernelPayload = payload::KernelRef<Self>;
    type Completion = usize;

    const PAYLOAD_KIND: OpKind = OpKind::Recv;

    fn new_kernel_payload(user: &Self) -> Self::KernelPayload {
        payload::kernel_ref(user)
    }

    unsafe fn make_sqe(
        kernel: &mut Self::KernelPayload,
        payload: &mut Self,
        env: &SqeEnv<'_>,
        token: SubmitTokenContext,
    ) -> UringResult<squeue::Entry> {
        unsafe { submit::make_sqe_recv(kernel, payload, env, token) }
    }

    fn map_completion(_payload: &Self, res: UringResult<usize>) -> UringResult<Self::Completion> {
        res
    }
}

impl UringOpSpec for RecvProvided {
    type KernelPayload = payload::KernelRef<Self>;
    type Completion = usize;

    const PAYLOAD_KIND: OpKind = OpKind::RecvProvided;

    fn new_kernel_payload(user: &Self) -> Self::KernelPayload {
        payload::kernel_ref(user)
    }

    unsafe fn make_sqe(
        kernel: &mut Self::KernelPayload,
        payload: &mut Self,
        env: &SqeEnv<'_>,
        token: SubmitTokenContext,
    ) -> UringResult<squeue::Entry> {
        unsafe { submit::make_sqe_recv_provided(kernel, payload, env, token) }
    }

    /// 产物只能在这里造：提交时没有 buffer，内核在数据到达时才从环里挑一个，bid 通过 CQE
    /// 的 `IORING_CQE_F_BUFFER` 回来。
    ///
    /// 恒返回 `Some`——**包括**没挑到 buffer 的那些完成（`-ENOBUFS`，或选中之前就失败）。
    /// 返回 `None` 会让完成路径退回去取提交 payload，而那是个 `RecvProvided`，投影必然
    /// 失配并报出一个含义完全错误的 `PayloadTypeMismatch`。
    fn record_item(
        _kernel: &mut Self::KernelPayload,
        _payload: &mut Self,
        result: i32,
        flags: u32,
        env: &mut CqeEnv<'_>,
    ) -> UringResult<Option<UringUserPayload>> {
        let buf = env.take_provided_buf(flags, result)?;
        Ok(Some(UringUserPayload::ProvidedBuf(ProvidedBuf { buf })))
    }

    fn map_completion(_payload: &Self, res: UringResult<usize>) -> UringResult<Self::Completion> {
        res
    }
}

impl UringOpSpec for OpSend {
    type KernelPayload = payload::KernelRef<Self>;
    type Completion = usize;

    const PAYLOAD_KIND: OpKind = OpKind::Send;

    fn new_kernel_payload(user: &Self) -> Self::KernelPayload {
        payload::kernel_ref(user)
    }

    unsafe fn make_sqe(
        kernel: &mut Self::KernelPayload,
        payload: &mut Self,
        env: &SqeEnv<'_>,
        token: SubmitTokenContext,
    ) -> UringResult<squeue::Entry> {
        unsafe { submit::make_sqe_send(kernel, payload, env, token) }
    }

    fn map_completion(_payload: &Self, res: UringResult<usize>) -> UringResult<Self::Completion> {
        res
    }
}

impl UringOpSpec for UdpRecv {
    type KernelPayload = payload::KernelRef<Self>;
    type Completion = usize;

    const PAYLOAD_KIND: OpKind = OpKind::UdpRecv;

    fn new_kernel_payload(user: &Self) -> Self::KernelPayload {
        payload::kernel_ref(user)
    }

    unsafe fn make_sqe(
        kernel: &mut Self::KernelPayload,
        payload: &mut Self,
        env: &SqeEnv<'_>,
        token: SubmitTokenContext,
    ) -> UringResult<squeue::Entry> {
        unsafe { submit::make_sqe_udp_recv(kernel, payload, env, token) }
    }

    fn map_completion(_payload: &Self, res: UringResult<usize>) -> UringResult<Self::Completion> {
        res
    }
}

impl UringOpSpec for UdpSend {
    type KernelPayload = payload::KernelRef<Self>;
    type Completion = usize;

    const PAYLOAD_KIND: OpKind = OpKind::UdpSend;

    fn new_kernel_payload(user: &Self) -> Self::KernelPayload {
        payload::kernel_ref(user)
    }

    unsafe fn make_sqe(
        kernel: &mut Self::KernelPayload,
        payload: &mut Self,
        env: &SqeEnv<'_>,
        token: SubmitTokenContext,
    ) -> UringResult<squeue::Entry> {
        unsafe { submit::make_sqe_udp_send(kernel, payload, env, token) }
    }

    fn map_completion(_payload: &Self, res: UringResult<usize>) -> UringResult<Self::Completion> {
        res
    }
}

impl UringOpSpec for Connect {
    type KernelPayload = payload::KernelRef<Self>;
    type Completion = usize;

    const PAYLOAD_KIND: OpKind = OpKind::Connect;

    fn new_kernel_payload(user: &Self) -> Self::KernelPayload {
        payload::kernel_ref(user)
    }

    unsafe fn make_sqe(
        kernel: &mut Self::KernelPayload,
        payload: &mut Self,
        env: &SqeEnv<'_>,
        token: SubmitTokenContext,
    ) -> UringResult<squeue::Entry> {
        unsafe { submit::make_sqe_connect(kernel, payload, env, token) }
    }

    fn map_completion(_payload: &Self, res: UringResult<usize>) -> UringResult<Self::Completion> {
        res
    }
}

impl UringOpSpec for UdpConnect {
    type KernelPayload = payload::KernelRef<Self>;
    type Completion = usize;

    const PAYLOAD_KIND: OpKind = OpKind::UdpConnect;

    fn new_kernel_payload(user: &Self) -> Self::KernelPayload {
        payload::kernel_ref(user)
    }

    unsafe fn make_sqe(
        kernel: &mut Self::KernelPayload,
        payload: &mut Self,
        env: &SqeEnv<'_>,
        token: SubmitTokenContext,
    ) -> UringResult<squeue::Entry> {
        unsafe { submit::make_sqe_udp_connect(kernel, payload, env, token) }
    }

    fn map_completion(_payload: &Self, res: UringResult<usize>) -> UringResult<Self::Completion> {
        res
    }
}

impl UringOpSpec for Accept {
    type KernelPayload = payload::AcceptPayload;
    type Completion = OwnedRawHandle;

    const PAYLOAD_KIND: OpKind = OpKind::Accept;

    fn new_kernel_payload(_user: &Self) -> Self::KernelPayload {
        payload::AcceptPayload::new()
    }

    unsafe fn make_sqe(
        kernel: &mut Self::KernelPayload,
        payload: &mut Self,
        env: &SqeEnv<'_>,
        token: SubmitTokenContext,
    ) -> UringResult<squeue::Entry> {
        unsafe { submit::make_sqe_accept(kernel, payload, env, token) }
    }

    unsafe fn on_complete(
        kernel: &mut Self::KernelPayload,
        payload: &mut Self,
        result: i32,
    ) -> UringResult<usize> {
        unsafe { submit::on_complete_accept(kernel, payload, result) }
    }

    fn completion_cleanup(
        _kernel: &mut Self::KernelPayload,
        result: i32,
    ) -> CompletionCleanupGuard {
        submit::completion_cleanup_close_raw_fd(result)
    }

    fn map_completion(_payload: &Self, res: UringResult<usize>) -> UringResult<Self::Completion> {
        submit::accepted_handle_from_res(res)
    }
}

impl UringOpSpec for AcceptMulti {
    type KernelPayload = payload::KernelRef<Self>;
    type Completion = OwnedRawHandle;

    const PAYLOAD_KIND: OpKind = OpKind::AcceptMulti;

    fn new_kernel_payload(user: &Self) -> Self::KernelPayload {
        payload::kernel_ref(user)
    }

    unsafe fn make_sqe(
        kernel: &mut Self::KernelPayload,
        payload: &mut Self,
        env: &SqeEnv<'_>,
        token: SubmitTokenContext,
    ) -> UringResult<squeue::Entry> {
        unsafe { submit::make_sqe_accept_multi(kernel, payload, env, token) }
    }

    /// 每条完成产出一个新连接。提交 payload（`AcceptMulti { fd }`，监听 socket）必须
    /// 留在 slot 里——内核还要用它继续 accept。
    fn record_item(
        _kernel: &mut Self::KernelPayload,
        _payload: &mut Self,
        _result: i32,
        _flags: u32,
        _env: &mut CqeEnv<'_>,
    ) -> UringResult<Option<UringUserPayload>> {
        Ok(Some(UringUserPayload::AcceptedSocket(AcceptedSocket)))
    }

    /// 与单发 `Accept` 相同：一条被丢弃的完成里那个已经被内核创建出来的 fd 必须关掉，
    /// 否则每次取消泄漏一个描述符。
    fn completion_cleanup(
        _kernel: &mut Self::KernelPayload,
        result: i32,
    ) -> CompletionCleanupGuard {
        submit::completion_cleanup_close_raw_fd(result)
    }

    fn map_completion(_payload: &Self, res: UringResult<usize>) -> UringResult<Self::Completion> {
        submit::accepted_handle_from_res(res)
    }
}

impl UringOpSpec for SendTo {
    type KernelPayload = payload::SendToPayload;
    type Completion = usize;

    const PAYLOAD_KIND: OpKind = OpKind::SendTo;

    fn new_kernel_payload(_user: &Self) -> Self::KernelPayload {
        payload::SendToPayload::new()
    }

    unsafe fn make_sqe(
        kernel: &mut Self::KernelPayload,
        payload: &mut Self,
        env: &SqeEnv<'_>,
        token: SubmitTokenContext,
    ) -> UringResult<squeue::Entry> {
        unsafe { submit::make_sqe_send_to(kernel, payload, env, token) }
    }

    fn map_completion(_payload: &Self, res: UringResult<usize>) -> UringResult<Self::Completion> {
        res
    }
}

impl UringOpSpec for UdpRecvFrom {
    type KernelPayload = payload::UdpRecvFromPayload;
    type Completion = usize;

    const PAYLOAD_KIND: OpKind = OpKind::UdpRecvFrom;

    fn new_kernel_payload(_user: &Self) -> Self::KernelPayload {
        payload::UdpRecvFromPayload::new()
    }

    unsafe fn make_sqe(
        kernel: &mut Self::KernelPayload,
        payload: &mut Self,
        env: &SqeEnv<'_>,
        token: SubmitTokenContext,
    ) -> UringResult<squeue::Entry> {
        unsafe { submit::make_sqe_udp_recv_from(kernel, payload, env, token) }
    }

    unsafe fn on_complete(
        kernel: &mut Self::KernelPayload,
        payload: &mut Self,
        result: i32,
    ) -> UringResult<usize> {
        unsafe { submit::on_complete_udp_recv_from(kernel, payload, result) }
    }

    fn map_completion(_payload: &Self, res: UringResult<usize>) -> UringResult<Self::Completion> {
        res
    }
}
