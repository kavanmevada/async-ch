#![feature(test)]

extern crate test;

use test::Bencher;

const TOTAL_STEPS: usize = 40_000;

#[macro_use]
extern crate criterion;

use criterion::black_box;
use criterion::*;
use std::time::Instant;

use async_std::prelude::FutureExt;
use async_std::stream::StreamExt;
use futures::stream::FuturesUnordered;
use futures::Future;
use futures::TryFutureExt;
use std::task::Context;
use std::time::Duration;

// The function to benchmark
fn parallel_async_receivers_flume(c: &mut Criterion) {
    let size: usize = TOTAL_STEPS;

    c.bench_with_input(
        BenchmarkId::new("async_parallel_receive_flume", size),
        &size,
        |b, &s| {
            // Insert a call to `to_async` to convert the bencher to async mode.
            // The timing loops are the same as with the normal bencher.
            b.to_async(criterion::async_executor::FuturesExecutor)
                .iter(|| async {
                    let (tx, rx) = flume::unbounded();

                    let send_fut = async move {
                        for _ in 0..s {
                            rx.recv_async().await.unwrap();
                        }
                    };

                    async_std::task::spawn(send_fut);

                    let mut futures_unordered = (0..250)
                        .map(|_| async {
                            while let Ok(()) = tx.send_async(()).await
                            /* rx.recv() is OK */
                            {}
                        })
                        .collect::<FuturesUnordered<_>>();

                    while futures_unordered.next().await.is_some() {}
                });
        },
    );
}

fn parallel_async_receivers_async_ch(c: &mut Criterion) {
    let size: usize = TOTAL_STEPS;

    c.bench_with_input(
        BenchmarkId::new("BM_parallel_receive_async_ch", size),
        &size,
        |b, &s| {
            // Insert a call to `to_async` to convert the bencher to async mode.
            // The timing loops are the same as with the normal bencher.
            b.to_async(criterion::async_executor::FuturesExecutor)
                .iter(|| async {
                    let (tx, rx) = async_ch::unbounded();

                    let send_fut = async move {
                        for _ in 0..s {
                            rx.recv_async().await.unwrap();
                        }
                    };

                    async_std::task::spawn(send_fut);

                    let mut futures_unordered = (0..250)
                        .map(|_| async {
                            while let Ok(()) = tx.send_async(()).await
                            /* rx.recv() is OK */
                            {}
                        })
                        .collect::<FuturesUnordered<_>>();

                    while futures_unordered.next().await.is_some() {}
                });
        },
    );
}

criterion_group!(
    benches,
    parallel_async_receivers_async_ch,
    parallel_async_receivers_flume
);
criterion_main!(benches);
