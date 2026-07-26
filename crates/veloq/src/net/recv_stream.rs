//! multishot recv 的用户侧流。
//!
//! 与 [`AcceptStream`](crate::net::AcceptStream) 同形，两条实现路径：
//!
//! - `Native`：一次提交，内核持续投递完成（io_uring `RecvMulti`，Linux 6.0+）；
//! - `Emulated`：每次 `poll_next` 提交一次单发 `RecvProvided`（Linux 5.19–5.x）。
//!
//! 但与 accept 有一处根本不同：**两条路径都要求 provided buffer 环**。这不是设计选择而是
//! 内核语义——`IORING_OP_RECV` 的 multishot 变体强制 `IOSQE_BUFFER_SELECT`（一个调用方交
//! 出来的 buffer 装不下多条完成的数据），而 `Emulated` 那一侧必须与它语义等价，也就不能反
//! 过来向调用方要 buffer。于是 IOCP 上这条流根本不存在，[`RecvStream`] 在那里是一个没有值
//! 的类型。
//!
//! 收益就是 provided buffer 的收益：**buffer 只在数据到达时才与连接绑定**。一万个挂着
//! `recv_multi()` 的空闲连接不占任何接收缓冲，而一万个挂着 `recv()` 的连接各压一个。
//!
//! 见 `MULTISHOT_PROVIDED_BUFFERS_DESIGN.md` §6 / §7。

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Stream;
use veloq_buf::FixedBuf;
use veloq_driver_native::op::OpSubmitter;

use crate::{
    error::Result,
    net::common::{InnerSocket, SocketTokenPtr},
    runtime::context::Ctx,
};

#[cfg(target_os = "linux")]
use diagweave::{prelude::*, report::Report};

#[cfg(target_os = "linux")]
use veloq_driver_native::{
    driver::{Driver, DriverCapability, PlatformSlotSpec},
    error::Error as DriverError,
    op::{Op, OpItem, OpResult, RecvMulti, RecvProvided},
};

#[cfg(target_os = "linux")]
use crate::net::{
    error::NetError,
    multishot::{is_buffer_ring_exhausted, is_capability_rejected},
};

#[cfg(not(target_os = "linux"))]
use std::{convert::Infallible, marker::PhantomData};

/// 一条完成产出的东西，两条路径共用。
///
/// `RecvMulti` 与 `RecvProvided` 的 `Output` / `Completion` 逐个相同（都是 `ProvidedBuf`
/// 加 `usize`），所以这个别名对两者都成立——「产物不是提交物」这件事两者一样，不一样的只是
/// 「一次提交产出几条」。
#[cfg(target_os = "linux")]
type ProvidedItem = OpItem<RecvProvided, PlatformSlotSpec>;

/// 环被掏空后连着重 arm 多少次仍然立刻 `-ENOBUFS`，就把错误交给用户。
///
/// 重 arm 本身是必需的：`-ENOBUFS` 那条 CQE 不带 `IORING_CQE_F_MORE`，内核顺带把整个
/// multishot 也终止了。但只重 arm 不设上限，遇上「消费方长期跟不上」就会变成一个安静的忙
/// 循环——每次唤醒提交一次、立刻再失败一次。到了上限就说明这不是抖动，用户该知道。
#[cfg(target_os = "linux")]
const MAX_EXHAUSTED_REARMS: u32 = 8;

#[cfg(target_os = "linux")]
enum RecvMode<'rt, 'reg, S: OpSubmitter<'reg, Ctx<'rt, 'reg>>> {
    /// 一次提交，多条完成：句柄本身就是流。
    Native(S::Stream<RecvMulti>),
    /// 每取一条都重新提交一次单发 recv。`None` 表示当前没有在途的那一次。
    Emulated {
        pending: Option<S::Stream<RecvProvided>>,
    },
    /// 不再提交任何东西：终态错误已经交给用户了。
    Done,
}

/// [`crate::net::TcpStream::recv_multi`] 产出的数据流。
///
/// 每一项是内核挑给这条连接的一个 [`FixedBuf`]，长度已经是实际收到的字节数。**对端关闭时
/// 流正常结束**（`poll_next` 返回 `None`），而不是产出一个空 buffer——「读到 0 字节」在这里
/// 只有一种意思，用流的终点表达它比让每个调用方都去判长度可靠。
///
/// 流被丢弃时会取消在途的操作（句柄的 `Drop` → `mark_orphaned` +
/// `CancelRequest::abandon`），内核随后投递的完成走 orphan cleanup，其中被挑走的 buffer 会
/// 还回环而不是泄漏。
///
/// 与所有其它操作一样，这条流在**创建它的那个 worker** 上提交——socket 的注册描述符是
/// per-worker 的，provided buffer 环也是。
#[cfg(target_os = "linux")]
pub struct RecvStream<'rt, 'reg, S: OpSubmitter<'reg, Ctx<'rt, 'reg>>, P: SocketTokenPtr<'rt, 'reg>>
{
    mode: RecvMode<'rt, 'reg, S>,
    inner: InnerSocket<'rt, 'reg, P>,
    submitter: S,
    ctx: Ctx<'rt, 'reg>,
    /// `Native` 路径有没有产出过至少一项，见 [`RecvStream::downgraded`]。
    native_delivered: bool,
    /// 因为环空了而重新 arm 的累计次数（诊断用）。
    rearms: u32,
    /// 连续几次重 arm 之后仍然立刻 `-ENOBUFS`。收到任何一条数据就清零。
    exhausted_streak: u32,
}

/// IOCP 上这条流没有值。
///
/// 类型在两个平台上都在，[`crate::net::TcpStream::recv_multi`] 的签名因此也一样，调用方不
/// 必写 `cfg`；但它在这里恒返回 `Err(ProvidedBuffersUnavailable)`。用一个不可居留的类型
/// 表达「构造不出来」，而不是留一堆永远走不到的分支等人去读注释。
#[cfg(not(target_os = "linux"))]
pub enum RecvStream<'rt, 'reg, S: OpSubmitter<'reg, Ctx<'rt, 'reg>>, P: SocketTokenPtr<'rt, 'reg>> {
    /// 唯一的变体带着一个 [`Infallible`]，所以整个类型不可居留。
    Never(Infallible, PhantomData<(S, InnerSocket<'rt, 'reg, P>)>),
}

#[cfg(target_os = "linux")]
impl<'rt, 'reg, S, P> RecvStream<'rt, 'reg, S, P>
where
    S: OpSubmitter<'reg, Ctx<'rt, 'reg>> + Copy,
    P: SocketTokenPtr<'rt, 'reg>,
{
    pub(crate) fn new(
        ctx: Ctx<'rt, 'reg>,
        inner: InnerSocket<'rt, 'reg, P>,
        submitter: S,
    ) -> Result<Self> {
        let capabilities = ctx.driver(|driver| driver.capabilities());
        if !capabilities.provided_buffers {
            // 没有环，两条路都走不了（见模块文档）。这里明确报错而不是悄悄退回普通 recv：
            // 调用方没有交出 buffer，「换一条路」意味着运行时得凭空造一个，那就把这个 API
            // 唯一的卖点丢掉了。
            Err(NetError::ProvidedBuffersUnavailable)?;
        }

        let mode = if capabilities.recv_multi {
            RecvMode::Native(Self::arm_native(ctx, &inner, submitter))
        } else {
            RecvMode::Emulated { pending: None }
        };

        Ok(Self {
            mode,
            inner,
            submitter,
            ctx,
            native_delivered: false,
            rearms: 0,
            exhausted_streak: 0,
        })
    }

    /// 因为环空了而重新 arm 过多少次。
    ///
    /// 驱动那一侧的对应量是 `ProvidedBufStats` 的 `exhausted`（内核报了多少条 `-ENOBUFS`）
    /// 与 `available_low_water`。两者一起才说得清「偶发」还是「持续」。
    #[inline]
    pub fn rearms(&self) -> u32 {
        self.rearms
    }

    fn arm_native(
        ctx: Ctx<'rt, 'reg>,
        inner: &InnerSocket<'rt, 'reg, P>,
        submitter: S,
    ) -> S::Stream<RecvMulti> {
        let fd = inner.fd();
        ctx.submit_stream(&submitter, Op::new(RecvMulti { fd }))
    }

    /// 这一项是不是「内核不认识 multishot recv」，是的话就地降级。
    ///
    /// 与 [`AcceptStream`](crate::net::AcceptStream) 上同名方法逐条同理：`IORING_OP_RECV`
    /// 从 5.6 就在，它的 multishot 变体要 6.0，probe 分不出这两者。
    fn downgraded(&mut self, item: &ProvidedItem) -> bool {
        if self.native_delivered || !matches!(self.mode, RecvMode::Native(_)) {
            return false;
        }
        let OpResult::Completed(Err(report), _) = item else {
            return false;
        };
        if !is_capability_rejected(report) {
            return false;
        }
        // 关掉的是 driver 上的能力，不是这条流的：同一个 worker 后续开的每条流都直接从
        // `Emulated` 起步，不再各自付一次失败提交。
        self.ctx
            .driver(|mut driver| driver.note_capability_rejected(DriverCapability::RecvMulti));
        self.mode = RecvMode::Emulated { pending: None };
        true
    }

    /// 环空了：重新 arm 并要求再来一轮，除非已经连着试了太多次。
    ///
    /// 返回 `false` 表示放弃，调用方把那条 `-ENOBUFS` 交给用户。
    fn rearm_exhausted(&mut self) -> bool {
        self.exhausted_streak += 1;
        if self.exhausted_streak > MAX_EXHAUSTED_REARMS {
            self.mode = RecvMode::Done;
            return false;
        }
        self.rearms = self.rearms.saturating_add(1);
        if matches!(self.mode, RecvMode::Native(_)) {
            // multishot 已经被内核连同这条 `-ENOBUFS` 一起终止了，句柄里那条流是空的。
            self.mode = RecvMode::Native(Self::arm_native(self.ctx, &self.inner, self.submitter));
        }
        // `Emulated` 不必做什么：`poll_step` 取走结果时已经把 `pending` 清了，下一轮自然会
        // 重新提交。
        true
    }

    /// 从当前模式取一轮原始结果。`Ready(None)` 表示后端句柄的流空了。
    fn poll_step(&mut self, cx: &mut Context<'_>) -> Poll<Option<ProvidedItem>> {
        match &mut self.mode {
            RecvMode::Done => Poll::Ready(None),
            RecvMode::Native(stream) => {
                // SAFETY: 句柄不是自引用的，投影出 `&mut` 不违反 pin 契约。
                let stream = unsafe { Pin::new_unchecked(stream) };
                stream.poll_next(cx)
            }
            RecvMode::Emulated { pending } => {
                if pending.is_none() {
                    let fd = self.inner.fd();
                    *pending = Some(
                        self.ctx
                            .submit_stream(&self.submitter, Op::new(RecvProvided { fd })),
                    );
                }

                let op = pending.as_mut().expect("just armed above");
                // SAFETY: 与上面同理。
                let op = unsafe { Pin::new_unchecked(op) };
                let result = match op.poll_next(cx) {
                    Poll::Ready(Some(result)) => result,
                    // 单发操作的流只有一项，不可能在这里就空——空了说明上一轮忘了清。
                    Poll::Ready(None) => unreachable!("a single-shot recv was polled twice"),
                    Poll::Pending => return Poll::Pending,
                };
                // 这一次已经结束，下一轮会重新提交。
                *pending = None;
                Poll::Ready(Some(result))
            }
        }
    }

    /// 把一条完成变成这条流的下一步动作。
    fn classify(&mut self, item: ProvidedItem) -> Flow {
        if self.downgraded(&item) {
            return Flow::Retry;
        }
        self.native_delivered = true;

        let (res, provided) = item.into_inner();
        let received = match res {
            // 对端关闭。那个（空的）buffer 随 `provided` 一起回池。
            Ok(0) => return Flow::End,
            Ok(received) => received,
            Err(report) => return self.classify_error(report),
        };

        self.exhausted_streak = 0;
        Flow::Yield(match provided.and_then(|provided| provided.buf) {
            Some(buf) => {
                debug_assert_eq!(
                    buf.len(),
                    received,
                    "the driver sizes the buffer it hands out"
                );
                Ok(buf)
            }
            // 内核写了字节数却没给 bid，只可能是驱动那一侧的账错了。
            None => Err(NetError::ProvidedBufferMissing).trans(),
        })
    }

    fn classify_error(&mut self, report: Report<DriverError>) -> Flow {
        if is_buffer_ring_exhausted(&report) && self.rearm_exhausted() {
            return Flow::Retry;
        }
        Flow::Yield(Err(report).trans())
    }
}

/// 一条完成之后这条流该做什么。
#[cfg(target_os = "linux")]
enum Flow {
    /// 交给用户。
    Yield(Result<FixedBuf>),
    /// 这一条不该交给用户（降级 / 重 arm），再取一轮。
    Retry,
    /// 流正常结束。
    End,
}

#[cfg(target_os = "linux")]
impl<'rt, 'reg, S, P> Stream for RecvStream<'rt, 'reg, S, P>
where
    S: OpSubmitter<'reg, Ctx<'rt, 'reg>> + Copy,
    P: SocketTokenPtr<'rt, 'reg>,
{
    type Item = Result<FixedBuf>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // SAFETY: `RecvStream` 的字段都不是自引用的，投影出 `&mut` 不违反 pin 契约。
        let this = unsafe { self.get_unchecked_mut() };

        loop {
            let item = match this.poll_step(cx) {
                Poll::Ready(Some(item)) => item,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            };

            match this.classify(item) {
                Flow::Yield(item) => return Poll::Ready(Some(item)),
                Flow::Retry => continue,
                Flow::End => return Poll::Ready(None),
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl<'rt, 'reg, S, P> Stream for RecvStream<'rt, 'reg, S, P>
where
    S: OpSubmitter<'reg, Ctx<'rt, 'reg>> + Copy,
    P: SocketTokenPtr<'rt, 'reg>,
{
    type Item = Result<FixedBuf>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // 这个类型不可居留，所以这个函数没有可达的实现。
        match &*self {
            RecvStream::Never(never, _) => match *never {},
        }
    }
}
