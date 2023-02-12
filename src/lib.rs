use core::{fmt::Debug, future::Future};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU8, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Poll, Waker},
};

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Error {
    LockPoisoned,
    Disconnected,
}

#[derive(Debug)]
pub enum Item<T> {
    Occupied(Option<T>),
    Empty,
    Cancelled,
}

#[derive(Debug)]
pub struct Channel<T> {
    sending: Vec<(Waker, Item<T>)>,
    waiting: Vec<(Waker, Item<T>)>,
    queue: VecDeque<Option<T>>,
    length: usize,
}

#[derive(Debug)]
pub struct Shared<T> {
    channel: Mutex<Channel<T>>,
    sender_count: AtomicUsize,
    receiver_count: AtomicUsize,

    nsending: AtomicU8,
    nwaiting: AtomicU8,
}

impl<T> Channel<T> {
    fn with_capacity(length: usize) -> (Sender<T>, Receiver<T>) {
        let channel = Self {
            sending: Vec::with_capacity(32),
            waiting: Vec::with_capacity(32),
            queue: if length == usize::MAX {
                VecDeque::new()
            } else {
                VecDeque::with_capacity(length)
            },
            length,
        };

        let shared = Arc::new(Shared {
            channel: Mutex::new(channel),
            sender_count: AtomicUsize::new(1),
            receiver_count: AtomicUsize::new(1),

            nsending: AtomicU8::new(0),
            nwaiting: AtomicU8::new(0),
        });

        (Sender(Arc::clone(&shared)), Receiver(Arc::clone(&shared)))
    }
}

pub fn bounded<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    Channel::with_capacity(cap)
}

pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    Channel::with_capacity(usize::MAX)
}

pub struct SendFut<'s, T> {
    sender: &'s Sender<T>,
    msg_op: Option<T>,
    waker_op: Option<Waker>,
}

impl<'s, T> Drop for SendFut<'s, T> {
    fn drop(&mut self) {
        self.sender.0.channel.lock().map_or((), |mut channel| {
            if let Some((w, state)) = channel
                .sending
                .iter_mut()
                .find(|(a, _)| self.waker_op.as_ref().map_or(false, |b| a.will_wake(&b)))
            {
                *state = Item::Cancelled;
                w.wake_by_ref();
            }
        });
    }
}

impl<'s, T> Unpin for SendFut<'s, T> {}

impl<'s, T> Future for SendFut<'s, T> {
    type Output = Result<(), SendError<T>>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        let mut channel = self.sender.0.channel.lock().expect("poison error");

        if let Some((slot_at, (w, state))) = channel
            .sending
            .iter_mut()
            .enumerate()
            .find(|(_, (a, _))| self.waker_op.as_ref().map_or(false, |b| a.will_wake(&b)))
        {
            match state {
                Item::Occupied(msg_op) => {
                    if self.sender.0.receiver_count.load(Ordering::SeqCst) == 0 {
                        Poll::Ready(Err(SendError(msg_op.take().unwrap())))
                    } else {
                        if !w.will_wake(cx.waker()) {
                            *w = cx.waker().clone();
                            self.waker_op = Some(w.clone());
                        }

                        Poll::Pending
                    }
                }
                Item::Empty | Item::Cancelled => {
                    channel.sending.swap_remove(slot_at);
                    self.sender.0.nsending.fetch_sub(1, Ordering::AcqRel);

                    Poll::Ready(Ok(()))
                }
            }
        } else {
            if self.sender.0.receiver_count.load(Ordering::SeqCst) == 0 {
                return Poll::Ready(Err(SendError(self.msg_op.take().unwrap())));
            }

            for (w, state) in channel.waiting.iter_mut() {
                if let Item::Empty = state {
                    *state = Item::Occupied(self.msg_op.take());
                    w.wake_by_ref();

                    return Poll::Ready(Ok(()));
                }
            }

            if channel.queue.len() < channel.length {
                channel.queue.push_back(self.msg_op.take());
                return Poll::Ready(Ok(()));
            }

            let waker = cx.waker().clone();
            self.waker_op = Some(waker.clone());

            channel
                .sending
                .push((waker, Item::Occupied(self.msg_op.take())));

            self.sender.0.nsending.fetch_add(1, Ordering::AcqRel);

            Poll::Pending
        }
    }
}

#[derive(Debug)]
pub struct Sender<T>(Arc<Shared<T>>);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct SendError<T>(pub T);

impl<T: Debug> Sender<T> {
    pub fn send(&self, msg: T) -> Result<(), SendError<T>> {
        futures_lite::future::block_on(SendFut {
            sender: &self,
            msg_op: Some(msg),
            waker_op: None,
        })
    }

    pub async fn send_async(&self, msg: T) -> Result<(), SendError<T>> {
        SendFut {
            sender: &self,
            msg_op: Some(msg),
            waker_op: None,
        }
        .await
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.0.sender_count.fetch_add(1, Ordering::AcqRel);
        Self(self.0.clone())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.0.sender_count.fetch_sub(1, Ordering::AcqRel) == 1
            && self.0.nwaiting.load(Ordering::SeqCst) > 0
        {
            self.0.channel.lock().map_or((), |channel| {
                channel.waiting.iter().for_each(|(w, _)| w.wake_by_ref())
            });
        }
    }
}

#[derive(Debug)]
pub struct Receiver<T>(Arc<Shared<T>>);

pub struct RecvFut<'s, T> {
    receiver: &'s Receiver<T>,
    waker_op: Option<Waker>,
}

impl<'s, T> Unpin for RecvFut<'s, T> {}

impl<'s, T> Drop for RecvFut<'s, T> {
    fn drop(&mut self) {
        self.receiver.0.channel.lock().map_or((), |mut channel| {
            if let Some((w, state)) = channel
                .waiting
                .iter_mut()
                .find(|(a, _)| self.waker_op.as_ref().map_or(false, |b| a.will_wake(&b)))
            {
                *state = Item::Cancelled;
                w.wake_by_ref();
            }
        });
    }
}

impl<'s, T> Future for RecvFut<'s, T> {
    type Output = Result<T, RecvError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Self::Output> {
        let mut channel = self.receiver.0.channel.lock().expect("poison error");

        if let Some((slot_at, (w, state))) = channel
            .waiting
            .iter_mut()
            .enumerate()
            .find(|(_, (a, _))| self.waker_op.as_ref().map_or(false, |b| a.will_wake(&b)))
        {
            match state {
                Item::Empty => {
                    if self.receiver.0.sender_count.load(Ordering::SeqCst) == 0 {
                        Poll::Ready(Err(RecvError::Disconnected))
                    } else {
                        let nwaker = cx.waker();
                        if !w.will_wake(nwaker) {
                            *w = nwaker.clone();
                            self.waker_op = Some(w.clone());
                        }

                        Poll::Pending
                    }
                }
                Item::Occupied(msg_op) => {
                    let msg_op = msg_op.take();
                    channel.waiting.swap_remove(slot_at);
                    self.receiver.0.nwaiting.fetch_sub(1, Ordering::AcqRel);

                    Poll::Ready(Ok(msg_op.unwrap()))
                }
                Item::Cancelled => {
                    channel.waiting.swap_remove(slot_at);
                    self.receiver.0.nwaiting.fetch_sub(1, Ordering::AcqRel);

                    Poll::Ready(Err(RecvError::Cancelled))
                }
            }
        } else {
            if self.receiver.0.sender_count.load(Ordering::SeqCst) == 0 {
                return Poll::Ready(Err(RecvError::Disconnected));
            }

            for (w, state) in channel.sending.iter_mut() {
                if let Item::Occupied(msg_op) = state {
                    let msg = msg_op.take().unwrap();
                    *state = Item::Empty;
                    w.wake_by_ref();

                    return Poll::Ready(Ok(msg));
                }
            }

            if let Some(Some(msg)) = channel.queue.swap_remove_front(0) {
                return Poll::Ready(Ok(msg));
            }

            self.waker_op = Some(cx.waker().clone());
            channel.waiting.push((cx.waker().clone(), Item::Empty));

            self.receiver.0.nwaiting.fetch_add(1, Ordering::AcqRel);

            Poll::Pending
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecvError {
    /// All senders were dropped and no messages are waiting in the channel, so no further messages can be received.
    Disconnected,
    Cancelled,
}

impl<T: Debug> Receiver<T> {
    pub fn recv(&self) -> Result<T, RecvError> {
        futures_lite::future::block_on(self.recv_async())
    }

    pub async fn recv_async(&self) -> Result<T, RecvError> {
        RecvFut {
            receiver: &self,
            waker_op: None,
        }
        .await
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.0.receiver_count.fetch_add(1, Ordering::AcqRel);
        Self(self.0.clone())
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        if self.0.receiver_count.fetch_sub(1, Ordering::AcqRel) == 1
            && self.0.nsending.load(Ordering::SeqCst) > 0
        {
            self.0
                .channel
                .lock()
                .map_or((), |c| c.sending.iter().for_each(|(w, _)| w.wake_by_ref()));
        }
    }
}

#[cfg(test)]
mod tests {

    #[cfg(feature = "flume-tests")]
    mod flume_async_tests {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        use std::sync::Arc;
        use std::task::Context;
        use std::task::Poll;
        use std::task::Waker;
        use std::time::Duration;

        use crate::bounded;
        use crate::unbounded;
        use futures::stream::FuturesUnordered;
        use futures::Future;
        use futures::StreamExt;
        use futures::TryFutureExt;

        use async_std::prelude::FutureExt;

        #[test]
        fn r#async_recv() {
            let (tx, rx) = unbounded();

            let t = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(3));
                tx.send(42u32).unwrap();
                println!("send done");
            });

            async_std::task::block_on(async {
                assert_eq!(rx.recv_async().await.unwrap(), 42);
            });

            t.join().unwrap();
        }

        #[test]
        fn r#async_send() {
            let (tx, rx) = bounded(1);

            let t = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(250));
                assert_eq!(rx.recv(), Ok(42));
            });

            async_std::task::block_on(async {
                tx.send_async(42u32).await.unwrap();
            });

            t.join().unwrap();
        }

        #[test]
        fn r#async_recv_disconnect() {
            let (tx, rx) = bounded::<i32>(0);

            let t = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(250));
                drop(tx)
            });

            async_std::task::block_on(async {
                assert_eq!(rx.recv_async().await, Err(crate::RecvError::Disconnected));
            });

            t.join().unwrap();
        }

        #[test]
        fn r#async_send_disconnect() {
            let (tx, rx) = bounded(0);

            let t = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(3));
                drop(rx)
            });

            async_std::task::block_on(async {
                assert_eq!(tx.send_async(42u32).await, Err(crate::SendError(42)));
            });

            t.join().unwrap();
        }

        #[test]
        fn r#async_recv_drop_recv() {
            let (tx, rx) = bounded::<i32>(10);

            let recv_fut = rx.recv_async();

            async_std::task::block_on(async {
                let res = async_std::future::timeout(
                    std::time::Duration::from_millis(500),
                    rx.recv_async(),
                )
                .await;
                assert!(res.is_err());
            });

            let rx2 = rx.clone();
            let t = std::thread::spawn(move || {
                async_std::task::block_on(async { rx2.recv_async().await })
            });

            std::thread::sleep(std::time::Duration::from_millis(500));

            tx.send(42).unwrap();

            drop(recv_fut);

            assert_eq!(t.join().unwrap(), Ok(42))
        }

        #[async_std::test]
        async fn r#async_send_1_million_no_drop_or_reorder() {
            #[derive(Debug)]
            enum Message {
                Increment { old: u64 },
                ReturnCount,
            }

            let (tx, rx) = unbounded();

            let t = async_std::task::spawn(async move {
                let mut count = 0u64;

                while let Ok(Message::Increment { old }) = rx.recv_async().await {
                    assert_eq!(old, count);
                    count += 1;
                }

                count
            });

            for next in 0..1_000_000 {
                tx.send(Message::Increment { old: next }).unwrap();
            }

            tx.send(Message::ReturnCount).unwrap();

            let count = t.await;
            assert_eq!(count, 1_000_000)
        }

        #[async_std::test]
        async fn parallel_async_receivers() {
            let (tx, rx) = unbounded();
            let send_fut = async move {
                let n_sends: usize = 100000;
                for _ in 0..n_sends {
                    tx.send_async(()).await.unwrap();
                }
            };

            async_std::task::spawn(
                send_fut
                    .timeout(Duration::from_secs(5))
                    .map_err(|_| panic!("Send timed out!")),
            );

            let mut futures_unordered = (0..250)
                .map(|_| async {
                    while let Ok(()) = rx.recv_async().await
                    /* rx.recv() is OK */
                    {}
                })
                .collect::<FuturesUnordered<_>>();

            let recv_fut = async { while futures_unordered.next().await.is_some() {} };

            recv_fut
                .timeout(Duration::from_secs(5))
                .map_err(|_| panic!("Receive timed out!"))
                .await
                .unwrap();

            println!("recv end");
        }

        #[test]
        fn change_waker() {
            let (tx, rx) = bounded(1);
            tx.send(()).unwrap();

            struct DebugWaker(Arc<AtomicUsize>, Waker);

            impl DebugWaker {
                fn new() -> Self {
                    let woken = Arc::new(AtomicUsize::new(0));
                    let woken_cloned = woken.clone();
                    let waker = waker_fn::waker_fn(move || {
                        woken.fetch_add(1, Ordering::SeqCst);
                    });
                    DebugWaker(woken_cloned, waker)
                }

                fn woken(&self) -> usize {
                    self.0.load(Ordering::SeqCst)
                }

                fn ctx(&self) -> Context {
                    Context::from_waker(&self.1)
                }
            }

            // Check that the waker is correctly updated when sending tasks change their wakers
            {
                let send_fut = tx.send_async(());
                futures::pin_mut!(send_fut);

                let (waker1, waker2) = (DebugWaker::new(), DebugWaker::new());

                // Set the waker to waker1
                assert_eq!(send_fut.as_mut().poll(&mut waker1.ctx()), Poll::Pending);

                // Change the waker to waker2
                assert_eq!(send_fut.poll(&mut waker2.ctx()), Poll::Pending);

                // Wake the future
                rx.recv().unwrap();

                // Check that waker2 was woken and waker1 was not
                assert_eq!(waker1.woken(), 0);
                assert_eq!(waker2.woken(), 1);
            }

            // Check that the waker is correctly updated when receiving tasks change their wakers
            {
                rx.recv().unwrap();
                let recv_fut = rx.recv_async();
                futures::pin_mut!(recv_fut);

                let (waker1, waker2) = (DebugWaker::new(), DebugWaker::new());

                // Set the waker to waker1
                assert_eq!(recv_fut.as_mut().poll(&mut waker1.ctx()), Poll::Pending);

                // Change the waker to waker2
                assert_eq!(recv_fut.poll(&mut waker2.ctx()), Poll::Pending);

                // Wake the future
                tx.send(()).unwrap();

                // Check that waker2 was woken and waker1 was not
                assert_eq!(waker1.woken(), 0);
                assert_eq!(waker2.woken(), 1);
            }
        }
    }

    #[cfg(feature = "crossbeam-tests")]
    const TOTAL_STEPS: usize = 40_000;

    #[cfg(feature = "crossbeam-tests")]
    mod unbounded {
        use super::TOTAL_STEPS;
        use crate::bounded;
        use crate::unbounded;
        use std::thread::scope;

        #[test]
        fn create() {
            (0..10).for_each(|_| {
                let _ = unbounded::<i32>();
            });
        }

        #[test]
        fn oneshot() {
            (0..10).for_each(|_| {
                let (s, r) = unbounded::<i32>();
                s.send(0).unwrap();
                r.recv().unwrap();
            });
        }

        #[test]
        fn inout() {
            let (s, r) = unbounded::<i32>();
            (0..10).for_each(|_| {
                s.send(0).unwrap();
                r.recv().unwrap();
            });
        }

        #[test]
        fn par_inout() {
            let threads = num_cpus::get();
            let steps = TOTAL_STEPS / threads;
            let (s, r) = unbounded::<i32>();

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for i in 0..steps {
                                s.send(i as i32).unwrap();
                                r.recv().unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }

                (0..10).for_each(|_| {
                    for _ in 0..threads {
                        s1.send(()).unwrap();
                    }
                    for _ in 0..threads {
                        r2.recv().unwrap();
                    }
                });
                drop(s1);
            });
        }

        #[test]
        fn spsc() {
            let steps = TOTAL_STEPS;
            let (s, r) = unbounded::<i32>();

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                scope.spawn(|| {
                    while r1.recv().is_ok() {
                        for i in 0..steps {
                            s.send(i as i32).unwrap();
                        }
                        s2.send(()).unwrap();
                    }
                });

                (0..10).for_each(|_| {
                    s1.send(()).unwrap();
                    for _ in 0..steps {
                        r.recv().unwrap();
                    }
                    r2.recv().unwrap();
                });
                drop(s1);
            });
        }

        #[test]
        fn spmc() {
            let threads = num_cpus::get() - 1;
            let steps = TOTAL_STEPS / threads;
            let (s, r) = unbounded::<i32>();

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for _ in 0..steps {
                                r.recv().unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }

                (0..10).for_each(|_| {
                    for _ in 0..threads {
                        s1.send(()).unwrap();
                    }
                    for i in 0..steps * threads {
                        s.send(i as i32).unwrap();
                    }
                    for _ in 0..threads {
                        r2.recv().unwrap();
                    }
                });
                drop(s1);
            });
        }

        #[test]
        fn mpsc() {
            let threads = num_cpus::get() - 1;
            let steps = TOTAL_STEPS / threads;
            let (s, r) = unbounded::<i32>();

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for i in 0..steps {
                                s.send(i as i32).unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }

                (0..10).for_each(|_| {
                    for _ in 0..threads {
                        s1.send(()).unwrap();
                    }
                    for _ in 0..steps * threads {
                        r.recv().unwrap();
                    }
                    for _ in 0..threads {
                        r2.recv().unwrap();
                    }
                });
                drop(s1);
            });
        }

        #[test]
        fn mpmc() {
            let threads = num_cpus::get();
            let steps = TOTAL_STEPS / threads;
            let (s, r) = unbounded::<i32>();

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                for _ in 0..threads / 2 {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for i in 0..steps {
                                s.send(i as i32).unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }
                for _ in 0..threads / 2 {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for _ in 0..steps {
                                r.recv().unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }

                (0..10).for_each(|_| {
                    for _ in 0..threads {
                        s1.send(()).unwrap();
                    }
                    for _ in 0..threads {
                        r2.recv().unwrap();
                    }
                });
                drop(s1);
            });
        }
    }

    #[cfg(feature = "crossbeam-tests")]
    mod bounded_n {
        use super::TOTAL_STEPS;
        use crate::bounded;
        use std::thread::scope;

        #[test]
        fn spsc() {
            let steps = TOTAL_STEPS;
            let (s, r) = bounded::<i32>(steps);

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                scope.spawn(|| {
                    while r1.recv().is_ok() {
                        for i in 0..steps {
                            s.send(i as i32).unwrap();
                        }
                        s2.send(()).unwrap();
                    }
                });

                (0..10).for_each(|_| {
                    s1.send(()).unwrap();
                    for _ in 0..steps {
                        r.recv().unwrap();
                    }
                    r2.recv().unwrap();
                });
                drop(s1);
            });
        }

        #[test]
        fn spmc() {
            let threads = num_cpus::get() - 1;
            let steps = TOTAL_STEPS / threads;
            let (s, r) = bounded::<i32>(steps * threads);

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for _ in 0..steps {
                                r.recv().unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }

                (0..10).for_each(|_| {
                    for _ in 0..threads {
                        s1.send(()).unwrap();
                    }
                    for i in 0..steps * threads {
                        s.send(i as i32).unwrap();
                    }
                    for _ in 0..threads {
                        r2.recv().unwrap();
                    }
                });
                drop(s1);
            });
        }

        #[test]
        fn mpsc() {
            let threads = num_cpus::get() - 1;
            let steps = TOTAL_STEPS / threads;
            let (s, r) = bounded::<i32>(steps * threads);

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for i in 0..steps {
                                s.send(i as i32).unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }

                (0..10).for_each(|_| {
                    for _ in 0..threads {
                        s1.send(()).unwrap();
                    }
                    for _ in 0..steps * threads {
                        r.recv().unwrap();
                    }
                    for _ in 0..threads {
                        r2.recv().unwrap();
                    }
                });
                drop(s1);
            });
        }

        #[test]
        fn par_inout() {
            let threads = num_cpus::get();
            let steps = TOTAL_STEPS / threads;
            let (s, r) = bounded::<i32>(threads);

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for i in 0..steps {
                                s.send(i as i32).unwrap();
                                r.recv().unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }

                (0..10).for_each(|_| {
                    for _ in 0..threads {
                        s1.send(()).unwrap();
                    }
                    for _ in 0..threads {
                        r2.recv().unwrap();
                    }
                });
                drop(s1);
            });
        }

        #[test]
        fn mpmc() {
            let threads = num_cpus::get();
            assert_eq!(threads % 2, 0);
            let steps = TOTAL_STEPS / threads;
            let (s, r) = bounded::<i32>(steps * threads);

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                for _ in 0..threads / 2 {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for i in 0..steps {
                                s.send(i as i32).unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }
                for _ in 0..threads / 2 {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for _ in 0..steps {
                                r.recv().unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }

                (0..10).for_each(|_| {
                    for _ in 0..threads {
                        s1.send(()).unwrap();
                    }
                    for _ in 0..threads {
                        r2.recv().unwrap();
                    }
                });
                drop(s1);
            });
        }
    }

    #[cfg(feature = "crossbeam-tests")]
    mod bounded_1 {
        use super::TOTAL_STEPS;
        use crate::bounded;
        use std::thread::scope;

        #[test]
        fn create() {
            (0..10).for_each(|_| {
                let _ = bounded::<i32>(1);
            });
        }

        #[test]
        fn oneshot() {
            (0..10).for_each(|_| {
                let (s, r) = bounded::<i32>(1);
                s.send(0).unwrap();
                r.recv().unwrap();
            });
        }

        #[test]
        fn spsc() {
            let steps = TOTAL_STEPS;
            let (s, r) = bounded::<i32>(1);

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                scope.spawn(|| {
                    while r1.recv().is_ok() {
                        for i in 0..steps {
                            s.send(i as i32).unwrap();
                        }
                        s2.send(()).unwrap();
                    }
                });

                (0..10).for_each(|_| {
                    s1.send(()).unwrap();
                    for _ in 0..steps {
                        r.recv().unwrap();
                    }
                    r2.recv().unwrap();
                });
                drop(s1);
            });
        }

        #[test]
        fn spmc() {
            let threads = num_cpus::get() - 1;
            let steps = TOTAL_STEPS / threads;
            let (s, r) = bounded::<i32>(1);

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for _ in 0..steps {
                                r.recv().unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }

                (0..10).for_each(|_| {
                    for _ in 0..threads {
                        s1.send(()).unwrap();
                    }
                    for i in 0..steps * threads {
                        s.send(i as i32).unwrap();
                    }
                    for _ in 0..threads {
                        r2.recv().unwrap();
                    }
                });
                drop(s1);
            });
        }

        #[test]
        fn mpsc() {
            let threads = num_cpus::get() - 1;
            let steps = TOTAL_STEPS / threads;
            let (s, r) = bounded::<i32>(1);

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for i in 0..steps {
                                s.send(i as i32).unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }

                (0..10).for_each(|_| {
                    for _ in 0..threads {
                        s1.send(()).unwrap();
                    }
                    for _ in 0..steps * threads {
                        r.recv().unwrap();
                    }
                    for _ in 0..threads {
                        r2.recv().unwrap();
                    }
                });
                drop(s1);
            });
        }

        #[test]
        fn mpmc() {
            let threads = num_cpus::get();
            let steps = TOTAL_STEPS / threads;
            let (s, r) = bounded::<i32>(1);

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                for _ in 0..threads / 2 {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for i in 0..steps {
                                s.send(i as i32).unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }
                for _ in 0..threads / 2 {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for _ in 0..steps {
                                r.recv().unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }

                (0..10).for_each(|_| {
                    for _ in 0..threads {
                        s1.send(()).unwrap();
                    }
                    for _ in 0..threads {
                        r2.recv().unwrap();
                    }
                });
                drop(s1);
            });
        }
    }

    #[cfg(feature = "crossbeam-tests")]
    mod bounded_0 {
        use super::TOTAL_STEPS;
        use crate::bounded;
        use std::thread::scope;

        #[test]
        fn create() {
            (0..10).for_each(|_| {
                let _ = bounded::<i32>(0);
            });
        }

        #[test]
        fn spsc() {
            let steps = TOTAL_STEPS;
            let (s, r) = bounded::<i32>(0);

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                scope.spawn(|| {
                    while r1.recv().is_ok() {
                        for i in 0..steps {
                            s.send(i as i32).unwrap();
                        }
                        s2.send(()).unwrap();
                    }
                });

                (0..10).for_each(|_| {
                    s1.send(()).unwrap();
                    for _ in 0..steps {
                        r.recv().unwrap();
                    }
                    r2.recv().unwrap();
                });
                drop(s1);
            });
        }

        #[test]
        fn spmc() {
            let threads = num_cpus::get() - 1;
            let steps = TOTAL_STEPS / threads;
            let (s, r) = bounded::<i32>(0);

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for _ in 0..steps {
                                r.recv().unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }

                (0..10).for_each(|_| {
                    for _ in 0..threads {
                        s1.send(()).unwrap();
                    }
                    for i in 0..steps * threads {
                        s.send(i as i32).unwrap();
                    }
                    for _ in 0..threads {
                        r2.recv().unwrap();
                    }
                });
                drop(s1);
            });
        }

        #[test]
        fn mpsc() {
            let threads = num_cpus::get() - 1;
            let steps = TOTAL_STEPS / threads;
            let (s, r) = bounded::<i32>(0);

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for i in 0..steps {
                                s.send(i as i32).unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }

                (0..10).for_each(|_| {
                    for _ in 0..threads {
                        s1.send(()).unwrap();
                    }
                    for _ in 0..steps * threads {
                        r.recv().unwrap();
                    }
                    for _ in 0..threads {
                        r2.recv().unwrap();
                    }
                });
                drop(s1);
            });
        }

        #[test]
        fn mpmc() {
            let threads = num_cpus::get();
            let steps = TOTAL_STEPS / threads;
            let (s, r) = bounded::<i32>(0);

            let (s1, r1) = bounded(0);
            let (s2, r2) = bounded(0);
            scope(|scope| {
                for _ in 0..threads / 2 {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for i in 0..steps {
                                s.send(i as i32).unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }
                for _ in 0..threads / 2 {
                    scope.spawn(|| {
                        while r1.recv().is_ok() {
                            for _ in 0..steps {
                                r.recv().unwrap();
                            }
                            s2.send(()).unwrap();
                        }
                    });
                }

                (0..10).for_each(|_| {
                    for _ in 0..threads {
                        s1.send(()).unwrap();
                    }
                    for _ in 0..threads {
                        r2.recv().unwrap();
                    }
                });
                drop(s1);
            });
        }
    }
}
