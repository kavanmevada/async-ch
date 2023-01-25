# Async-Ch

A multi-producer, multi-consumer channel focused towards async usage.

Note: Currently at Dev stage.

```rust
use std::thread;

fn main() {
    println!("Hello, world!");

    let (tx, rx) = async_ch::unbounded();

    thread::spawn(move || {
        (0..10).for_each(|i| {
            tx.send(i).unwrap();
        })
    });

    let received: u32 = rx.recv();

}
```