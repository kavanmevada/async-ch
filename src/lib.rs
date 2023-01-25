use core::fmt::Debug;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
    task::{Poll, Waker},
};

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Error {
    LockPoisoned,
    Disconnected,
}

#[derive(Debug)]
pub enum SendState<T> {
    Pending(Option<T>),
    Done,
    Cancelled,
}

#[derive(Debug)]
pub enum RecvState<T> {
    Pending,
    Done(Option<T>),
    Cancelled,
}

#[derive(Debug)]
pub struct Channel<T> {
    sending: Vec<(Arc<Waker>, SendState<T>)>,
    waiting: Vec<(Arc<Waker>, RecvState<T>)>,
    queue: VecDeque<Option<T>>,
    length: usize,
}

#[derive(Debug)]
pub struct Shared<T> {
    channel: Mutex<Channel<T>>,
    sender_count: AtomicUsize,
    receiver_count: AtomicUsize,
}

impl<T> Channel<T> {
    fn with_capacity(length: usize) -> Self {
        if length == usize::MAX {
            Self {
                sending: Vec::default(),
                waiting: Vec::default(),
                queue: VecDeque::default(),
                length,
            }
        } else {
            Self {
                sending: Vec::with_capacity(length),
                waiting: Vec::with_capacity(length),
                queue: VecDeque::with_capacity(length),
                length,
            }
        }
    }
}

pub fn bounded<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        channel: Mutex::new(Channel::with_capacity(cap)),
        sender_count: AtomicUsize::new(1),
        receiver_count: AtomicUsize::new(1),
    });

    (Sender(Arc::clone(&shared)), Receiver(Arc::clone(&shared)))
}

pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        channel: Mutex::new(Channel::with_capacity(usize::MAX)),
        sender_count: AtomicUsize::new(1),
        receiver_count: AtomicUsize::new(1),
    });

    (Sender(Arc::clone(&shared)), Receiver(Arc::clone(&shared)))
}

#[derive(Debug)]
pub struct SenderGuard<'r, T>(&'r Arc<Shared<T>>, Weak<Waker>);

impl<'r, T> Drop for SenderGuard<'r, T> {
    fn drop(&mut self) {
        self.0.channel.lock().map_or((), |mut channel| {
            if let Some((w, state)) = channel
                .sending
                .iter_mut()
                .find(|(w, _)| Arc::downgrade(w).ptr_eq(&self.1))
            {
                *state = SendState::Cancelled;
                w.wake_by_ref();
            }
        });
    }
}

#[derive(Debug)]
pub struct Sender<T>(Arc<Shared<T>>);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct SendError<T>(pub T);

impl<T: Debug> Sender<T> {
    pub fn send(&self, msg: T) -> Result<(), SendError<T>> {
        futures::executor::block_on(self.send_async(msg))
    }

    pub async fn send_async(&self, msg: T) -> Result<(), SendError<T>> {
        let mut marker: Weak<Waker> = Weak::new();
        let mut msg_op = Some(msg);

        let mut guard = SenderGuard(&self.0, Weak::new());

        core::future::poll_fn(|cx| {
            let mut channel = self.0.channel.lock().expect("poison error");

            let waker = Arc::new(cx.waker().clone());

            // println!(
            //     "{:?}: send(before) {:?}",
            //     std::thread::current().id(),
            //     channel
            // );

            if let Some((pos, w, state)) =
                channel
                    .sending
                    .iter_mut()
                    .enumerate()
                    .find_map(|(p, (w, s))| {
                        if Arc::downgrade(w).ptr_eq(&marker) {
                            Some((p, w, s))
                        } else {
                            None
                        }
                    })
            {
                // Second Poll

                match state {
                    SendState::Pending(msg_op) => {
                        if self.0.receiver_count.load(Ordering::SeqCst) == 0 {
                            Poll::Ready(Err(SendError(msg_op.take().unwrap())))
                        } else {
                            marker = Arc::downgrade(&waker);
                            guard.1 = Arc::downgrade(&waker);
                            *w = waker;

                            // println!("snd: waker updated");

                            Poll::Pending
                        }
                    }
                    SendState::Done => {
                        channel.sending.remove(pos);

                        // println!(
                        //     "{:?}: send(after) SendState::Done so removed {:?}",
                        //     std::thread::current().id(),
                        //     &channel
                        // );

                        Poll::Ready(Ok(()))
                    }
                    SendState::Cancelled => {
                        channel.sending.remove(pos);

                        // println!(
                        //     "{:?}: send(after) SendState::Done so removed {:?}",
                        //     std::thread::current().id(),
                        //     &channel
                        // );

                        Poll::Ready(Ok(()))
                    }
                }
            } else {
                // First Poll

                for (w, state) in channel.waiting.iter_mut() {
                    if let RecvState::Pending = state {
                        *state = RecvState::Done(msg_op.take());
                        w.wake_by_ref();

                        // println!(
                        //     "{:?} state replaced from waiting by sending {:?}",
                        //     std::thread::current().id(),
                        //     &channel
                        // );

                        return Poll::Ready(Ok(()));
                    }
                }

                if self.0.receiver_count.load(Ordering::SeqCst) == 0 {
                    return Poll::Ready(Err(SendError(msg_op.take().unwrap())));
                }

                if channel.queue.len() < channel.length {
                    channel.queue.push_back(msg_op.take());
                    return Poll::Ready(Ok(()));
                }

                marker = Arc::downgrade(&waker);
                guard.1 = Arc::downgrade(&waker);
                channel
                    .sending
                    .push((waker, SendState::Pending(msg_op.take())));

                // println!(
                //     "{:?}: send(after) pending => {:?}",
                //     std::thread::current().id(),
                //     &channel
                // );

                Poll::Pending
            }
        })
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
        if self.0.sender_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            if let Ok(channel) = self.0.channel.lock() {
                channel.waiting.iter().for_each(|(w, _)| w.wake_by_ref());
            }
        }
    }
}

#[derive(Debug)]
pub struct ReceiverGuard<'r, T>(&'r Arc<Shared<T>>, Weak<Waker>);

impl<'r, T> Drop for ReceiverGuard<'r, T> {
    fn drop(&mut self) {
        self.0.channel.lock().map_or((), |mut channel| {
            if let Some((w, state)) = channel
                .waiting
                .iter_mut()
                .find(|(w, _)| Arc::downgrade(w).ptr_eq(&self.1))
            {
                *state = RecvState::Cancelled;
                w.wake_by_ref();
            }
        });
    }
}

#[derive(Debug)]
pub struct Receiver<T>(Arc<Shared<T>>);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RecvError {
    /// All senders were dropped and no messages are waiting in the channel, so no further messages can be received.
    Disconnected,
    Cancelled,
}

impl<T: Debug> Receiver<T> {
    pub fn recv(&self) -> Result<T, RecvError> {
        futures::executor::block_on(self.recv_async())
    }

    pub async fn recv_async(&self) -> Result<T, RecvError> {
        let mut marker: Weak<Waker> = Weak::new();
        let mut guard = ReceiverGuard(&self.0, Weak::new());

        core::future::poll_fn(|cx| {
            let mut channel = self.0.channel.lock().or(Err(RecvError::Disconnected))?;

            let waker = Arc::new(cx.waker().clone());

            // println!(
            //     "{:?}: recv(before) {:?}",
            //     std::thread::current().id(),
            //     channel
            // );

            if let Some((pos, w, state)) =
                channel
                    .waiting
                    .iter_mut()
                    .enumerate()
                    .find_map(|(p, (w, s))| {
                        if Arc::downgrade(w).ptr_eq(&marker) {
                            Some((p, w, s))
                        } else {
                            None
                        }
                    })
            {
                // Second Poll

                match state {
                    RecvState::Pending => {
                        if self.0.sender_count.load(Ordering::SeqCst) == 0 {
                            Poll::Ready(Err(RecvError::Disconnected))
                        } else {
                            marker = Arc::downgrade(&waker);
                            guard.1 = Arc::downgrade(&waker);
                            *w = waker;

                            // println!(
                            //     "{:?} waker updated in recv_async {:?}",
                            //     std::thread::current().id(),
                            //     &channel
                            // );

                            Poll::Pending
                        }
                    }
                    RecvState::Done(msg_op) => {
                        let msg_op = msg_op.take();
                        channel.waiting.remove(pos);
                        // println!(
                        //     "{:?}: recv(after) removed from waiting by receiver {:?}",
                        //     std::thread::current().id(),
                        //     &channel
                        // );

                        Poll::Ready(Ok(msg_op.unwrap()))
                    }
                    RecvState::Cancelled => {
                        channel.waiting.remove(pos);
                        // println!(
                        //     "{:?}: recv(after) removed from waiting by receiver {:?}",
                        //     std::thread::current().id(),
                        //     &channel
                        // );

                        Poll::Ready(Err(RecvError::Cancelled))
                    }
                }
            } else {
                // First Poll

                for (w, state) in channel.sending.iter_mut() {
                    if let SendState::Pending(msg_op) = state {
                        let msg = msg_op.take().unwrap();
                        *state = SendState::Done;
                        w.wake_by_ref();

                        // println!(
                        //     "{:?}: recv(after) replaced state from sending by receiver {:?}",
                        //     std::thread::current().id(),
                        //     &channel
                        // );

                        return Poll::Ready(Ok(msg));
                    }
                }

                if self.0.sender_count.load(Ordering::SeqCst) == 0 {
                    return Poll::Ready(Err(RecvError::Disconnected));
                }

                if !channel.queue.is_empty() {
                    if let Some(Some(msg)) = channel.queue.pop_front() {
                        return Poll::Ready(Ok(msg));
                    }
                }

                marker = Arc::downgrade(&waker);
                guard.1 = Arc::downgrade(&waker);
                channel.waiting.push((waker, RecvState::Pending));

                // println!(
                //     "{:?}: recv(after) pending {:?}",
                //     std::thread::current().id(),
                //     &channel
                // );

                Poll::Pending
            }
        })
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
        if self.0.receiver_count.fetch_sub(1, Ordering::AcqRel) == 1 {
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
