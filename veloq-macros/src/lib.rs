#[macro_export]
macro_rules! for_all_io_ops {
    ($mac:ident) => {
        $mac! {
            WithFd {
                ReadFixed(ReadFixed),
                WriteFixed(WriteFixed),
                Send(Send),
                Recv(Recv),
                Accept(Accept),
                Connect(Connect),
                SendTo(SendTo),
                RecvFrom(RecvFrom),
                Close(Close),
                Fsync(Fsync),
                #[cfg(unix)]
                Wakeup(Wakeup),
            },
            WithoutFd {
                Timeout(Timeout),
                Open(Open),
                #[cfg(windows)]
                Wakeup(Wakeup),
            }
        }
    };
}
