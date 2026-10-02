// SPDX-License-Identifier: MIT OR Apache-2.0
use std::{
    cell::Cell,
    sync::atomic::{AtomicBool, Ordering},
};
static INITIALIZED: AtomicBool = AtomicBool::new(false);
extern "C" fn initialize() {
    INITIALIZED.store(true, Ordering::SeqCst);
}
#[used]
#[unsafe(link_section = ".init_array")]
static INITIALIZER: extern "C" fn() = initialize;
extern "C" fn finalize() {
    println!("rust-fini");
}
#[used]
#[unsafe(link_section = ".fini_array")]
static FINALIZER: extern "C" fn() = finalize;
thread_local! { static VALUE: Cell<usize> = const { Cell::new(13) }; }
fn main() {
    assert!(INITIALIZED.load(Ordering::SeqCst));
    assert_eq!(std::env::args().skip(1).collect::<Vec<_>>(), ["one", "two"]);
    assert_eq!(
        std::env::var("PUFFINBOX_STARTUP_PROBE").unwrap(),
        "original-entry"
    );
    let worker = std::thread::spawn(|| {
        VALUE.with(|v| {
            assert_eq!(v.get(), 13);
            v.set(47);
            v.get()
        })
    });
    assert_eq!(worker.join().unwrap(), 47);
    VALUE.with(|v| assert_eq!(v.get(), 13));
    assert!(std::panic::catch_unwind(|| panic!("intentional startup probe")).is_err());
    println!("rust-init-args-env-thread-tls-unwind");
}
