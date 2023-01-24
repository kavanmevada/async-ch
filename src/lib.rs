use core::fmt::Debug;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
    task::{Poll, Waker},
};

type Queue<T> = VecDeque<T>;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Error {
    LockPoisoned,
    Disconnected,
}

#[derive(Debug)]
pub struct Channel<T> {
    sending: Queue<(Option<T>, Arc<Waker>)>,
    waiting: Queue<(Option<T>, Arc<Waker>)>,
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
                sending: VecDeque::default(),
                waiting: VecDeque::default(),
                queue: VecDeque::default(),
                length,
            }
        } else {
            Self {
                sending: VecDeque::with_capacity(length),
                waiting: VecDeque::with_capacity(length),
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
            channel
                .sending
                .retain(|(_, w)| !Arc::downgrade(w).ptr_eq(&self.1))
        });
    }
}

#[derive(Debug)]
pub struct Sender<T>(Arc<Shared<T>>);

impl<T: Debug> Sender<T> {
    pub fn send(&self, msg: T) -> Result<(), Error> {
        futures_lite::future::block_on(self.send_async(msg))
    }

    pub async fn send_async(&self, msg: T) -> Result<(), Error> {
        let mut marker: Weak<Waker> = Weak::new();
        let mut msg_op = Some(msg);

        let mut guard = SenderGuard(&self.0, Weak::new());

        core::future::poll_fn(|cx| {
            let mut channel = self.0.channel.lock().or(Err(Error::LockPoisoned))?;

            if channel.sending.front().map_or(false, |s| s.0.is_none()) {
                let _ = channel.sending.pop_front();
                return Poll::Ready(Ok(()));
            }

            if let Some(item) = channel.waiting.front_mut().filter(|w| w.0.is_none()) {
                let _ = core::mem::replace(&mut item.0, msg_op.take());
                item.1.wake_by_ref();
                return Poll::Ready(Ok(()));
            }

            if self.0.receiver_count.load(Ordering::SeqCst) == 0 {
                return Poll::Ready(Err(Error::Disconnected));
            }

            if channel.queue.len() < channel.length {
                channel.queue.push_back(msg_op.take());
                return Poll::Ready(Ok(()));
            }

            let waker = Arc::new(cx.waker().clone());

            if let Some((_, w)) = channel
                .sending
                .iter_mut()
                .find(|(_, w)| Arc::downgrade(w).ptr_eq(&marker))
            {
                let _ = core::mem::replace(w, waker.clone());
            } else {
                channel.sending.push_back((msg_op.take(), waker.clone()));
            }

            drop(channel);

            let _ = core::mem::replace(&mut marker, Arc::downgrade(&waker));
            let _ = core::mem::replace(&mut guard.1, Arc::downgrade(&waker));

            Poll::Pending
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
                channel.waiting.iter().for_each(|(_, w)| w.wake_by_ref());
            }
        }
    }
}

#[derive(Debug)]
pub struct ReceiverGuard<'r, T>(&'r Arc<Shared<T>>, Weak<Waker>);

impl<'r, T> Drop for ReceiverGuard<'r, T> {
    fn drop(&mut self) {
        self.0.channel.lock().map_or((), |mut channel| {
            channel
                .waiting
                .retain(|(_, w)| !Arc::downgrade(w).ptr_eq(&self.1))
        });
    }
}

#[derive(Debug)]
pub struct Receiver<T>(Arc<Shared<T>>);

impl<T: Debug> Receiver<T> {
    pub fn recv(&self) -> Result<T, Error> {
        futures_lite::future::block_on(self.recv_async())
    }

    pub async fn recv_async(&self) -> Result<T, Error> {
        let mut marker: Weak<Waker> = Weak::new();
        let mut guard = ReceiverGuard(&self.0, Weak::new());

        core::future::poll_fn(|cx| {
            let mut channel = self.0.channel.lock().or(Err(Error::LockPoisoned))?;

            if channel.waiting.front().map_or(false, |s| s.0.is_some()) {
                if let Some((msg_op, _)) = channel.waiting.pop_front() {
                    return Poll::Ready(Ok(msg_op.unwrap()));
                }
            }

            if let Some((msg_op, w)) = channel
                .sending
                .front_mut()
                .filter(|(msg_op, _)| msg_op.is_some())
            {
                let msg_op = msg_op.take().unwrap();
                w.wake_by_ref();
                return Poll::Ready(Ok(msg_op));
            }

            if channel.queue.len() > 0 {
                if let Some(Some(msg)) = channel.queue.pop_front() {
                    return Poll::Ready(Ok(msg));
                }
            }

            if self.0.sender_count.load(Ordering::SeqCst) == 0 {
                return Poll::Ready(Err(Error::Disconnected));
            }

            let waker = Arc::new(cx.waker().clone());

            if let Some((_, w)) = channel
                .waiting
                .iter_mut()
                .find(|(_, w)| Arc::downgrade(w).ptr_eq(&marker))
            {
                let _ = core::mem::replace(w, waker.clone());
            } else {
                channel.waiting.push_back((None, waker.clone()));
            }

            drop(channel);

            let _ = core::mem::replace(&mut marker, Arc::downgrade(&waker));
            let _ = core::mem::replace(&mut guard.1, Arc::downgrade(&waker));

            Poll::Pending
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
                .map_or((), |c| c.sending.iter().for_each(|(_, w)| w.wake_by_ref()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_std::prelude::FutureExt;
    use async_std::stream::StreamExt;
    use futures::stream::FuturesUnordered;
    use futures::Future;
    use futures::TryFutureExt;
    use std::task::Context;
    use std::time::Duration;

    #[test]
    fn r#async_recv() {
        let (tx, rx) = unbounded::<u32>();

        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            async_std::task::block_on(tx.send_async(42u32)).unwrap();
            // println!("send end");
        });

        async_std::task::block_on(async {
            assert_eq!(rx.recv_async().await.unwrap(), 42);
            // println!("recv end");
        });

        t.join().unwrap();
    }

    #[test]
    fn r#async_send() {
        let (tx, rx) = bounded(0);

        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            assert_eq!(async_std::task::block_on(rx.recv_async()), Ok(42));
            // println!("recv end");
        });

        async_std::task::block_on(async {
            tx.send_async(42u32).await.unwrap();
            // println!("send end");
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
            assert!(rx.recv_async().await.is_err());
        });

        t.join().unwrap();
    }

    #[test]
    fn r#async_send_disconnect() {
        let (tx, rx) = bounded(0);

        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            drop(rx)
        });

        async_std::task::block_on(async {
            assert!(tx.send_async(42u32).await.is_err());
        });

        t.join().unwrap();
    }

    #[test]
    fn r#async_recv_drop_recv() {
        let (tx, rx) = bounded::<i32>(10);

        let recv_fut = rx.recv_async();

        async_std::task::block_on(async {
            let res =
                async_std::future::timeout(std::time::Duration::from_millis(500), rx.recv_async())
                    .await;
            assert!(res.is_err());
        });

        let rx2 = rx.clone();
        let t =
            std::thread::spawn(move || async_std::task::block_on(async { rx2.recv_async().await }));

        std::thread::sleep(std::time::Duration::from_millis(500));

        async_std::task::block_on(tx.send_async(42)).unwrap();

        drop(recv_fut);

        assert_eq!(t.join().unwrap(), Ok(42))
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
            .timeout(Duration::from_secs(25))
            .map_err(|_| panic!("Receive timed out!"))
            .await
            .unwrap();

        // println!("recv end");
    }

    #[async_std::test]
    async fn parallel_async_senders() {
        let (tx, rx) = unbounded::<()>();

        let send_fut = async move {
            let n_sends: usize = 2;
            for _ in 0..n_sends {
                rx.recv_async().await.unwrap();
            }
        };

        async_std::task::spawn(
            send_fut
                .timeout(Duration::from_secs(5))
                .map_err(|_| panic!("Send timed out!")),
        );

        let mut futures_unordered = (0..2)
            .map(|_| async {
                while let Ok(()) = tx.send_async(()).await
                /* rx.recv() is OK */
                {}
            })
            .collect::<FuturesUnordered<_>>();

        let recv_fut = async { while futures_unordered.next().await.is_some() {} };

        recv_fut
            .timeout(Duration::from_secs(25))
            .map_err(|_| panic!("Receive timed out!"))
            .await
            .unwrap();

        // println!("recv end");
    }

    #[test]
    fn change_waker() {
        let (tx, rx) = bounded(1);
        async_std::task::block_on(tx.send_async(())).unwrap();

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
            assert_eq!(send_fut.as_mut().poll(&mut waker2.ctx()), Poll::Pending);

            // Wake the future
            async_std::task::block_on(rx.recv_async()).unwrap();

            // Check that waker2 was woken and waker1 was not
            assert_eq!(waker1.woken(), 0);
            assert_eq!(waker2.woken(), 1);

            // Check future ended correctly
            assert_eq!(send_fut.poll(&mut waker2.ctx()), Poll::Ready(Ok(())));
        }

        // Check that the waker is correctly updated when receiving tasks change their wakers
        {
            async_std::task::block_on(rx.recv_async()).unwrap();
            let recv_fut = rx.recv_async();
            futures::pin_mut!(recv_fut);

            let (waker1, waker2) = (DebugWaker::new(), DebugWaker::new());

            // Set the waker to waker1
            assert_eq!(recv_fut.as_mut().poll(&mut waker1.ctx()), Poll::Pending);

            // Change the waker to waker2
            assert_eq!(recv_fut.as_mut().poll(&mut waker2.ctx()), Poll::Pending);

            // Wake the future
            async_std::task::block_on(tx.send_async(())).unwrap();

            // Check that waker2 was woken and waker1 was not
            assert_eq!(waker1.woken(), 0);
            assert_eq!(waker2.woken(), 1);

            // Check future ended correctly
            assert_eq!(recv_fut.poll(&mut waker2.ctx()), Poll::Ready(Ok(())));
        }
    }

    #[test]
    fn _mpmc() {
        const TOTAL_STEPS: usize = 40_000;

        let threads = num_cpus::get();
        let steps = TOTAL_STEPS / threads;
        let (s, r) = unbounded::<i32>();

        let (s1, r1) = bounded::<()>(0);
        let (s2, r2) = bounded::<()>(0);
        for _ in 0..threads / 2 {
            let r1 = r1.clone();
            let s2 = s2.clone();
            let s = s.clone();

            std::thread::spawn(move || {
                while r1.recv().is_ok() {
                    for _i in 0..steps {
                        s.send(_i as i32).unwrap();
                    }
                    s2.send(()).unwrap();
                }
            });
        }
        for _ in 0..threads / 2 {
            let r1 = r1.clone();
            let s2 = s2.clone();
            let r = r.clone();

            std::thread::spawn(move || {
                while r1.recv().is_ok() {
                    for _i in 0..steps {
                        r.recv().unwrap();
                    }
                    s2.send(()).unwrap();
                }
            });
        }

        for _ in 0..threads / 2 {
            for _ in 0..2 {
                s1.send(()).unwrap();
                r2.recv().unwrap();
            }
        }

        // drop(s1);
    }

    #[test]
    fn _spmc() {
        const TOTAL_STEPS: usize = 40_000;

        let threads = num_cpus::get() - 1;
        let steps = TOTAL_STEPS / threads;
        let (s, r) = unbounded::<i32>();

        let (s1, r1) = bounded(0);
        let (s2, r2) = bounded(0);
        for _ in 0..threads {
            let r1 = r1.clone();
            let r = r.clone();
            let s2 = s2.clone();

            std::thread::spawn(move || {
                while r1.recv().is_ok() {
                    for _ in 0..steps {
                        r.recv().unwrap();
                    }
                    s2.send(()).unwrap();
                }
            });
        }

        for _ in 0..threads {
            s1.send(()).unwrap();
        }
        for i in 0..steps * threads {
            s.send(i as i32).unwrap();
        }
        for _ in 0..threads {
            r2.recv().unwrap();
        }
        // drop(s1);
    }

    #[test]
    fn _mpsc() {
        const TOTAL_STEPS: usize = 40_000;

        let threads = num_cpus::get() - 1;
        let steps = TOTAL_STEPS / threads;
        let (s, r) = unbounded::<i32>();

        let (s1, r1) = bounded(0);
        let (s2, r2) = bounded(0);
        for _ in 0..threads {
            let r1 = r1.clone();
            let s = s.clone();
            let s2 = s2.clone();

            std::thread::spawn(move || {
                while r1.recv().is_ok() {
                    for i in 0..steps {
                        s.send(i as i32).unwrap();
                    }
                    s2.send(()).unwrap();
                }
            });
        }

        for _ in 0..threads {
            s1.send(()).unwrap();
        }
        for _ in 0..steps * threads {
            r.recv().unwrap();
        }
        for _ in 0..threads {
            r2.recv().unwrap();
        }
        drop(s1);
    }
}
