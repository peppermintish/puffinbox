use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake},
    time::Duration,
};

use futures_channel::mpsc;
use futures_util::{Sink, Stream, StreamExt};

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn concurrent_producers_deliver_each_value_once_in_producer_order() {
    const PRODUCERS: usize = 4;
    const PER_PRODUCER: usize = 2_000;
    let (sender, mut receiver) = mpsc::unbounded();
    let threads = (0..PRODUCERS)
        .map(|producer| {
            let sender = sender.clone();
            std::thread::spawn(move || {
                for sequence in 0..PER_PRODUCER {
                    sender.unbounded_send((producer, sequence)).unwrap();
                }
            })
        })
        .collect::<Vec<_>>();
    drop(sender);
    let mut next = [0; PRODUCERS];
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some((producer, sequence)) = receiver.next().await {
            assert_eq!(sequence, next[producer]);
            next[producer] += 1;
        }
    })
    .await
    .expect("all producers must wake and drain the receiver");
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(next, [PER_PRODUCER; PRODUCERS]);
}

#[test]
fn bounded_capacity_wakes_the_parked_sender_after_receive() {
    let (mut sender, mut receiver) = mpsc::channel(0);
    sender.try_send(10).unwrap();
    let wake = Arc::new(WakeCount::default());
    let waker = wake.clone().into();
    let mut context = Context::from_waker(&waker);
    assert!(Pin::new(&mut sender).poll_ready(&mut context).is_pending());
    assert_eq!(
        Pin::new(&mut receiver).poll_next(&mut context),
        Poll::Ready(Some(10))
    );
    assert!(wake.0.load(Ordering::SeqCst) > 0);
    assert!(matches!(
        Pin::new(&mut sender).poll_ready(&mut context),
        Poll::Ready(Ok(()))
    ));
    sender.try_send(11).unwrap();
    drop(sender);
    assert_eq!(
        Pin::new(&mut receiver).poll_next(&mut context),
        Poll::Ready(Some(11))
    );
    assert_eq!(
        Pin::new(&mut receiver).poll_next(&mut context),
        Poll::Ready(None)
    );
}

#[test]
fn closing_the_receiver_wakes_a_sender_and_rejects_new_values() {
    let (mut sender, mut receiver) = mpsc::channel(0);
    sender.try_send(7).unwrap();
    let wake = Arc::new(WakeCount::default());
    let waker = wake.clone().into();
    let mut context = Context::from_waker(&waker);
    assert!(Pin::new(&mut sender).poll_ready(&mut context).is_pending());
    receiver.close();
    assert!(wake.0.load(Ordering::SeqCst) > 0);
    assert!(matches!(
        Pin::new(&mut sender).poll_ready(&mut context),
        Poll::Ready(Err(_))
    ));
    assert_eq!(
        Pin::new(&mut receiver).poll_next(&mut context),
        Poll::Ready(Some(7))
    );
    assert_eq!(
        Pin::new(&mut receiver).poll_next(&mut context),
        Poll::Ready(None)
    );
    assert!(sender.try_send(8).unwrap_err().is_disconnected());
}

#[test]
fn send_and_final_sender_drop_wake_the_pending_receiver() {
    let (sender, mut receiver) = mpsc::unbounded();
    let wake = Arc::new(WakeCount::default());
    let waker = wake.clone().into();
    let mut context = Context::from_waker(&waker);
    assert!(Pin::new(&mut receiver).poll_next(&mut context).is_pending());
    sender.unbounded_send(3).unwrap();
    assert!(wake.0.load(Ordering::SeqCst) > 0);
    assert_eq!(
        Pin::new(&mut receiver).poll_next(&mut context),
        Poll::Ready(Some(3))
    );
    assert!(Pin::new(&mut receiver).poll_next(&mut context).is_pending());
    let previous = wake.0.load(Ordering::SeqCst);
    drop(sender);
    assert!(wake.0.load(Ordering::SeqCst) > previous);
    assert_eq!(
        Pin::new(&mut receiver).poll_next(&mut context),
        Poll::Ready(None)
    );
}

#[test]
fn receiver_drop_releases_each_queued_value_and_rejected_send_once() {
    #[derive(Debug)]
    struct Value(Arc<AtomicUsize>);
    impl Drop for Value {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = mpsc::unbounded();
    for _ in 0..100 {
        sender.unbounded_send(Value(dropped.clone())).unwrap();
    }
    drop(receiver);
    assert_eq!(dropped.load(Ordering::SeqCst), 100);
    let error = sender.unbounded_send(Value(dropped.clone())).unwrap_err();
    assert!(error.is_disconnected());
    drop(error);
    drop(sender);
    assert_eq!(dropped.load(Ordering::SeqCst), 101);
}
