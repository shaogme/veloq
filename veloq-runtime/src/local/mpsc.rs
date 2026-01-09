use futures_core::Future;
use futures_core::stream::Stream;
use intrusive_collections::{
    Adapter, LinkedList, LinkedListLink, PointerOps, container_of, linked_list::LinkOps, offset_of,
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    fmt,
    marker::PhantomPinned,
    pin::Pin,
    ptr::NonNull,
    rc::Rc,
    task::{Context, Poll, Waker},
};

/// 发送操作可能遇到的错误
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SendError<T> {
    /// 接收端已关闭
    Closed(T), // 返回未发送的消息
    /// 通道已满（仅限有界通道）
    Full(T), // 返回未发送的消息
}

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendError::Closed(_) => write!(f, "SendError::Closed(..)"),
            SendError::Full(_) => write!(f, "SendError::Full(..)"),
        }
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendError::Closed(_) => write!(f, "SendError::Closed(..)"),
            SendError::Full(_) => write!(f, "SendError::Full(..)"),
        }
    }
}

impl<T> std::error::Error for SendError<T> {}

/// 通道容量配置
#[derive(Debug, Clone, Copy)]
pub enum ChannelCapacity {
    Unbounded,
    Bounded(usize),
}

/// 本地通道的发送端
///
/// 由于基于 `Rc` 和 `RefCell`，只能在单线程（Local）使用。
#[derive(Debug)]
pub struct LocalSender<T> {
    channel: LocalChannel<T>,
}

/// 本地通道的接收端
///
/// 提供 `recv` 方法用于接收消息，也可以通过 `stream()` 转换为 `Stream`。
#[derive(Debug)]
pub struct LocalReceiver<T> {
    channel: LocalChannel<T>,
}

#[derive(Debug, Eq, PartialEq, Copy, Clone)]
enum WaiterKind {
    Sender,
    Receiver,
}

#[derive(Debug)]
struct Waiter<'a, T, F> {
    node: WaiterNode,
    channel: &'a LocalChannel<T>,
    poll_fn: F,
}

#[derive(Debug)]
enum PollResult<T> {
    Pending(WaiterKind),
    Ready(T),
}

impl<'a, T, F, R> Waiter<'a, T, F>
where
    F: FnMut() -> PollResult<R>,
{
    fn new(poll_fn: F, channel: &'a LocalChannel<T>) -> Self {
        Waiter {
            poll_fn,
            channel,
            node: WaiterNode {
                waker: RefCell::new(None),
                link: LinkedListLink::new(),
                kind: Cell::new(None),
                _p: PhantomPinned,
            },
        }
    }
}

impl<T, F, R> Future for Waiter<'_, T, F>
where
    F: FnMut() -> PollResult<R>,
{
    type Output = R;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let future_mut = unsafe { self.get_unchecked_mut() };
        let pinned_node = unsafe { Pin::new_unchecked(&mut future_mut.node) };

        let result = (future_mut.poll_fn)();
        match result {
            PollResult::Pending(kind) => {
                pinned_node.kind.set(Some(kind));
                *pinned_node.waker.borrow_mut() = Some(cx.waker().clone());

                register_into_waiting_queue(
                    pinned_node,
                    &mut future_mut.channel.state.borrow_mut(),
                );
                Poll::Pending
            }
            PollResult::Ready(result) => {
                remove_from_the_waiting_queue(
                    pinned_node,
                    &mut future_mut.channel.state.borrow_mut(),
                );
                Poll::Ready(result)
            }
        }
    }
}

fn register_into_waiting_queue<T>(node: Pin<&mut WaiterNode>, state: &mut State<T>) {
    if node.link.is_linked() {
        return;
    }

    let kind = node.kind.get().expect("Unknown part of the channel");
    match kind {
        WaiterKind::Sender => {
            state
                .send_waiters
                .push_back(unsafe { NonNull::new_unchecked(node.get_unchecked_mut()) });
        }
        WaiterKind::Receiver => {
            state
                .recv_waiters
                .push_back(unsafe { NonNull::new_unchecked(node.get_unchecked_mut()) });
        }
    }
}

fn remove_from_the_waiting_queue<T>(node: Pin<&mut WaiterNode>, state: &mut State<T>) {
    if !node.link.is_linked() {
        return;
    }

    let kind = node.kind.get().expect("Unknown part of the channel");
    let mut cursor = match kind {
        WaiterKind::Sender => unsafe {
            state
                .send_waiters
                .cursor_mut_from_ptr(node.get_unchecked_mut())
        },
        WaiterKind::Receiver => unsafe {
            state
                .recv_waiters
                .cursor_mut_from_ptr(node.get_unchecked_mut())
        },
    };

    cursor.remove();
}

impl<T, F> Drop for Waiter<'_, T, F> {
    fn drop(&mut self) {
        if self.node.link.is_linked() {
            let pinned_node = unsafe { Pin::new_unchecked(&mut self.node) };
            let kind = pinned_node
                .kind
                .get()
                .expect("If Future is queued type of the queue has to be specified");

            let mut state = self.channel.state.borrow_mut();
            let waiters = match kind {
                WaiterKind::Sender => &mut state.send_waiters,
                WaiterKind::Receiver => &mut state.recv_waiters,
            };

            let mut cursor =
                unsafe { waiters.cursor_mut_from_ptr(pinned_node.get_unchecked_mut()) };
            cursor.remove();
        }
    }
}

#[derive(Debug)]
struct WaiterNode {
    waker: RefCell<Option<Waker>>,
    link: LinkedListLink,
    kind: Cell<Option<WaiterKind>>,
    _p: PhantomPinned,
}

struct WaiterPointerOps;

unsafe impl PointerOps for WaiterPointerOps {
    type Value = WaiterNode;
    type Pointer = NonNull<WaiterNode>;

    unsafe fn from_raw(&self, value: *const Self::Value) -> Self::Pointer {
        NonNull::new(value as *mut Self::Value).expect("Pointer to the value can not be null")
    }

    fn into_raw(&self, ptr: Self::Pointer) -> *const Self::Value {
        ptr.as_ptr() as *const Self::Value
    }
}

struct WaiterAdapter {
    pointers_ops: WaiterPointerOps,
    link_ops: LinkOps,
}

impl WaiterAdapter {
    pub const NEW: Self = WaiterAdapter {
        pointers_ops: WaiterPointerOps,
        link_ops: LinkOps,
    };
}

unsafe impl Adapter for WaiterAdapter {
    type LinkOps = LinkOps;
    type PointerOps = WaiterPointerOps;

    unsafe fn get_value(
        &self,
        link: <Self::LinkOps as intrusive_collections::LinkOps>::LinkPtr,
    ) -> *const <Self::PointerOps as PointerOps>::Value {
        container_of!(link.as_ptr(), WaiterNode, link)
    }

    unsafe fn get_link(
        &self,
        value: *const <Self::PointerOps as PointerOps>::Value,
    ) -> <Self::LinkOps as intrusive_collections::LinkOps>::LinkPtr {
        if value.is_null() {
            panic!("Value pointer can not be null");
        }
        unsafe {
            let ptr = (value as *const u8).add(offset_of!(WaiterNode, link));
            core::ptr::NonNull::new_unchecked(ptr as *mut _)
        }
    }

    fn link_ops(&self) -> &Self::LinkOps {
        &self.link_ops
    }

    fn link_ops_mut(&mut self) -> &mut Self::LinkOps {
        &mut self.link_ops
    }

    fn pointer_ops(&self) -> &Self::PointerOps {
        &self.pointers_ops
    }
}

#[derive(Debug)]
struct State<T> {
    capacity: ChannelCapacity,
    channel: VecDeque<T>,
    tx_count: usize,
    is_closed: bool,
    recv_waiters: LinkedList<WaiterAdapter>,
    send_waiters: LinkedList<WaiterAdapter>,
}

impl<T> State<T> {
    fn push(&mut self, item: T) -> Result<Option<Waker>, SendError<T>> {
        if self.is_closed {
            Err(SendError::Closed(item))
        } else if self.is_full() {
            Err(SendError::Full(item))
        } else {
            self.channel.push_back(item);

            Ok(self.recv_waiters.pop_front().map(|n| {
                unsafe { n.as_ref() }
                    .waker
                    .borrow_mut()
                    .take()
                    .expect("Future was added to the waiting queue without a waker")
            }))
        }
    }

    fn is_full(&self) -> bool {
        match self.capacity {
            ChannelCapacity::Unbounded => false,
            ChannelCapacity::Bounded(x) => self.channel.len() >= x,
        }
    }

    fn wait_for_room(&mut self) -> PollResult<()> {
        if self.is_closed {
            PollResult::Ready(())
        } else if !self.is_full() {
            PollResult::Ready(())
        } else {
            PollResult::Pending(WaiterKind::Sender)
        }
    }

    fn recv_one(&mut self) -> PollResult<Option<(T, Option<Waker>)>> {
        match self.channel.pop_front() {
            Some(item) => PollResult::Ready(Some((
                item,
                self.send_waiters
                    .pop_front()
                    .and_then(|node| unsafe { node.as_ref() }.waker.borrow_mut().take()),
            ))),
            None => {
                if self.tx_count > 0 {
                    PollResult::Pending(WaiterKind::Receiver)
                } else {
                    PollResult::Ready(None)
                }
            }
        }
    }
}

#[derive(Debug)]
struct LocalChannel<T> {
    state: Rc<RefCell<State<T>>>,
}

impl<T> Clone for LocalChannel<T> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}

impl<T> Drop for LocalChannel<T> {
    fn drop(&mut self) {
        // 当 LocalChannel 被回收时，意味着它是最后一个引用（或 Sender/Receiver 已经被 Drop）。
        // 实际上 LocalChannel 只有在 Sender 和 Receiver 都被 Drop 后，或者
        // 手动 clone 的 LocalChannel 句柄被 Drop 后才会真正释放 State。
        // State 的生命周期由 Rc 控制。
        // 原则是：只要 State 的 push/recv 功能不再可用，waiters 就应该为空。

        // 由于 Sender 和 Receiver 在 Drop 时会分别清理 send_waiters 和 recv_waiters，
        // 理论上这里如果 State 还在（通过 Rc），列表应该已经被正确管理。
        // 如果 LocalChannel 是通过 Sender/Receiver 持有的，当它们 Drop 时会清理。
        // 我们只在调试模式下断言。

        #[cfg(debug_assertions)]
        {
            let should_check = Rc::strong_count(&self.state) == 1;
            if should_check {
                if let Ok(state) = self.state.try_borrow() {
                    assert!(state.recv_waiters.is_empty(), "Receiver waiters mismatch");
                    assert!(state.send_waiters.is_empty(), "Sender waiters mismatch");
                }
            }
        }
    }
}

impl<T> LocalChannel<T> {
    fn new(capacity: ChannelCapacity) -> (LocalSender<T>, LocalReceiver<T>) {
        let channel_buffer = match capacity {
            ChannelCapacity::Unbounded => VecDeque::new(),
            ChannelCapacity::Bounded(x) => VecDeque::with_capacity(x),
        };

        let channel = LocalChannel {
            state: Rc::new(RefCell::new(State {
                capacity,
                channel: channel_buffer,
                tx_count: 1,
                is_closed: false,
                send_waiters: LinkedList::new(WaiterAdapter::NEW),
                recv_waiters: LinkedList::new(WaiterAdapter::NEW),
            })),
        };

        (
            LocalSender {
                channel: channel.clone(),
            },
            LocalReceiver { channel },
        )
    }
}

/// 创建一个新的无界通道
pub fn new_unbounded<T>() -> (LocalSender<T>, LocalReceiver<T>) {
    LocalChannel::new(ChannelCapacity::Unbounded)
}

/// 创建一个新的有界通道
pub fn new_bounded<T>(size: usize) -> (LocalSender<T>, LocalReceiver<T>) {
    LocalChannel::new(ChannelCapacity::Bounded(size))
}

impl<T> Clone for LocalSender<T> {
    fn clone(&self) -> Self {
        self.channel.state.borrow_mut().tx_count += 1;
        Self {
            channel: self.channel.clone(),
        }
    }
}

impl<T> LocalSender<T> {
    /// 尝试发送数据，如果通道已满或接收端关闭则返回错误
    pub fn try_send(&self, item: T) -> Result<(), SendError<T>> {
        if let Some(w) = self.channel.state.borrow_mut().push(item)? {
            w.wake();
        }
        Ok(())
    }

    /// 异步发送数据，如果通道已满则等待
    pub async fn send(&self, item: T) -> Result<(), SendError<T>> {
        // 先等待空间，但不持有 borrow
        Waiter::new(|| self.wait_for_room(), &self.channel).await;
        // 等待结束后尝试发送
        self.try_send(item)
    }

    /// 检查通道是否已满
    pub fn is_full(&self) -> bool {
        self.channel.state.borrow().is_full()
    }

    /// 获取当前通道中的消息数量
    pub fn len(&self) -> usize {
        self.channel.state.borrow().channel.len()
    }

    /// 检查通道是否为空
    pub fn is_empty(&self) -> bool {
        self.channel.state.borrow().channel.is_empty()
    }

    fn wait_for_room(&self) -> PollResult<()> {
        self.channel.state.borrow_mut().wait_for_room()
    }
}

fn wake_up_all(waiters: &mut LinkedList<WaiterAdapter>) {
    let mut cursor = waiters.front_mut();
    while !cursor.is_null() {
        {
            let node = unsafe { Pin::new_unchecked(cursor.get().expect("Waiter queue check")) };
            node.waker
                .borrow_mut()
                .take()
                .expect("Future queued without waker")
                .wake();
        }
        cursor.remove();
    }
}

impl<T> Drop for LocalSender<T> {
    fn drop(&mut self) {
        let mut state = self.channel.state.borrow_mut();
        state.tx_count -= 1;

        if state.tx_count == 0 {
            wake_up_all(&mut state.send_waiters);
            wake_up_all(&mut state.recv_waiters);
        }
    }
}

impl<T> Drop for LocalReceiver<T> {
    fn drop(&mut self) {
        let mut state = self.channel.state.borrow_mut();
        // Receiver 只有一个，所以可以直接关闭
        state.is_closed = true;
        wake_up_all(&mut state.recv_waiters);
        wake_up_all(&mut state.send_waiters);
    }
}

struct ChannelStream<'a, T> {
    channel: &'a LocalChannel<T>,
    node: Pin<Box<WaiterNode>>,
}

impl<'a, T> ChannelStream<'a, T> {
    fn new(channel: &'a LocalChannel<T>) -> Self {
        ChannelStream {
            channel,
            node: Box::pin(WaiterNode {
                waker: RefCell::new(None),
                link: LinkedListLink::new(),
                kind: Cell::new(None),
                _p: PhantomPinned,
            }),
        }
    }
}

impl<T> Stream for ChannelStream<'_, T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let result = self.channel.state.borrow_mut().recv_one();
        let this = unsafe { self.get_unchecked_mut() };

        match result {
            PollResult::Pending(kind) => {
                *this.node.waker.borrow_mut() = Some(cx.waker().clone());
                this.node.kind.set(Some(kind));

                register_into_waiting_queue(
                    this.node.as_mut(),
                    &mut this.channel.state.borrow_mut(),
                );

                Poll::Pending
            }
            PollResult::Ready(result) => {
                remove_from_the_waiting_queue(
                    this.node.as_mut(),
                    &mut this.channel.state.borrow_mut(),
                );

                Poll::Ready(result.map(|(ret, mw)| {
                    if let Some(waker) = mw {
                        waker.wake();
                    }
                    ret
                }))
            }
        }
    }
}

impl<T> Drop for ChannelStream<'_, T> {
    fn drop(&mut self) {
        // 必须确保移除 waiting node，否则会导致悬挂指针（unsafe linked list）。
        // 使用 borrow_mut() 而非 try_borrow_mut() 以确保在异常情况下也能清理。
        // 如果 panic 发生，RefCell 已经 poison 也没关系，主要是防止后续 UB。
        let mut state = self.channel.state.borrow_mut();
        remove_from_the_waiting_queue(self.node.as_mut(), &mut state);
    }
}

impl<T> LocalReceiver<T> {
    /// 接收下一条消息
    pub async fn recv(&self) -> Option<T> {
        Waiter::new(|| self.recv_one(), &self.channel).await
    }

    /// 转换为 Stream
    pub fn stream(&self) -> impl Stream<Item = T> + '_ {
        ChannelStream::new(&self.channel)
    }

    fn recv_one(&self) -> PollResult<Option<T>> {
        let result = self.channel.state.borrow_mut().recv_one();
        match result {
            PollResult::Pending(kind) => PollResult::Pending(kind),
            PollResult::Ready(opt) => PollResult::Ready(opt.map(|(ret, mw)| {
                if let Some(w) = mw {
                    w.wake();
                }
                ret
            })),
        }
    }
}
