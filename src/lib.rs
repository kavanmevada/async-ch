use core::fmt::Debug;
use std::{
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
}

#[derive(Debug)]
pub enum RecvState<T> {
    Pending,
    Done(Option<T>),
}

#[derive(Debug)]
pub struct Channel<T> {
    sending: Vec<(Arc<Waker>, SendState<T>)>,
    waiting: Vec<(Arc<Waker>, RecvState<T>)>,
    queue: Vec<Option<T>>,
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
                queue: Vec::default(),
                length,
            }
        } else {
            Self {
                sending: Vec::with_capacity(length),
                waiting: Vec::with_capacity(length),
                queue: Vec::with_capacity(length),
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
                .retain(|(w, _)| !Arc::downgrade(w).ptr_eq(&self.1))
        });
    }
}

#[derive(Debug)]
pub struct Sender<T>(Arc<Shared<T>>);

impl<T: Debug> Sender<T> {
    pub fn send(&self, msg: T) -> Result<(), Error> {
        futures::executor::block_on(self.send_async(msg))
    }

    pub async fn send_async(&self, msg: T) -> Result<(), Error> {
        let mut marker: Weak<Waker> = Weak::new();
        let mut msg_op = Some(msg);

        let mut guard = SenderGuard(&self.0, Weak::new());

        core::future::poll_fn(|cx| {
            let mut channel = self.0.channel.lock().or(Err(Error::LockPoisoned))?;

            if self.0.receiver_count.load(Ordering::SeqCst) == 0 {
                return Poll::Ready(Err(Error::Disconnected));
            }

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
                    SendState::Pending(_) => {
                        *w = waker;

                        // println!("snd: waker updated");

                        Poll::Pending
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

                if channel.queue.len() < channel.length {
                    channel.queue.push(msg_op.take());
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
            channel
                .waiting
                .retain(|(w, _)| !Arc::downgrade(w).ptr_eq(&self.1))
        });
    }
}

#[derive(Debug)]
pub struct Receiver<T>(Arc<Shared<T>>);

impl<T: Debug> Receiver<T> {
    pub fn recv(&self) -> Result<T, Error> {
        futures::executor::block_on(self.recv_async())
    }

    pub async fn recv_async(&self) -> Result<T, Error> {
        let mut marker: Weak<Waker> = Weak::new();
        let mut guard = ReceiverGuard(&self.0, Weak::new());

        core::future::poll_fn(|cx| {
            let mut channel = self.0.channel.lock().or(Err(Error::LockPoisoned))?;

            if self.0.sender_count.load(Ordering::SeqCst) == 0 {
                return Poll::Ready(Err(Error::Disconnected));
            }

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
                        *w = waker;

                        // println!(
                        //     "{:?} waker updated in recv_async {:?}",
                        //     std::thread::current().id(),
                        //     &channel
                        // );

                        Poll::Pending
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

                if !channel.queue.is_empty() {
                    if let Some(msg) = channel.queue.remove(0) {
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
    use super::*;

    const TOTAL_STEPS: usize = 40_000;

    mod unbounded {
        use std::thread::scope;

        use super::*;

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

    mod bounded_n {
        use std::thread::scope;

        use super::*;

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

    mod bounded_1 {
        use std::thread::scope;

        use super::*;

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

    mod bounded_0 {
        use std::thread::scope;

        use super::*;

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
