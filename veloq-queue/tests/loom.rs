#[cfg(feature = "loom")]
mod loom_tests {
    use loom::sync::Arc;
    use loom::thread;
    use veloq_queue::MpscQueue;

    /// Test concurrent producers ensuring all data is lost.
    /// Since we join the threads, we guarantee strict happens-before for the pops.
    #[test]
    fn test_concurrent_producers() {
        loom::model(|| {
            let q = Arc::new(MpscQueue::new());
            let q1 = q.clone();
            let q2 = q.clone();

            let t1 = thread::spawn(move || {
                q1.push(1);
                q1.push(2);
            });
            let t2 = thread::spawn(move || {
                q2.push(3);
                q2.push(4);
            });

            t1.join().unwrap();
            t2.join().unwrap();

            let mut sum = 0;
            // We expect exactly 4 items.
            for _ in 0..4 {
                match q.pop() {
                    Some(v) => sum += v,
                    None => panic!("Queue should not be empty after joins have completed"),
                }
            }
            assert_eq!(sum, 1 + 2 + 3 + 4);
            assert!(q.pop().is_none());
        });
    }

    /// Test the race between push and pop.
    /// Even if they race, linearizability ensures that if pop returns None,
    /// the push hasn't "happened" effectively from the consumer's perspective yet.
    /// But once we join, we MUST see the value.
    #[test]
    fn test_push_pop_race() {
        loom::model(|| {
            let q = Arc::new(MpscQueue::new());
            let q1 = q.clone();

            let t = thread::spawn(move || {
                q1.push(10);
            });

            // This pop races with the push
            let v1 = q.pop();

            t.join().unwrap();

            // After join, the push is definitely visible.
            // If v1 was None, we must find 10 now.
            // If v1 was Some(10), the queue must be empty now.
            match v1 {
                Some(v) => {
                    assert_eq!(v, 10);
                    assert!(q.pop().is_none());
                }
                None => {
                    assert_eq!(q.pop(), Some(10));
                }
            }
        });
    }

    #[test]
    fn test_n_producers_1_consumer() {
        loom::model(|| {
            let q = Arc::new(MpscQueue::new());
            let n_producers = 2; // Keep small for loom
            let items_per_producer = 2;

            let mut producers = Vec::new();
            for i in 0..n_producers {
                let q = q.clone();
                producers.push(thread::spawn(move || {
                    for j in 0..items_per_producer {
                        q.push(i * 100 + j);
                    }
                }));
            }

            let q_cons = q.clone();
            let consumer = thread::spawn(move || {
                let mut count = 0;
                let target = n_producers * items_per_producer;
                while count < target {
                    if q_cons.pop().is_some() {
                        count += 1;
                    } else {
                        thread::yield_now();
                    }
                }
                count
            });

            for p in producers {
                p.join().unwrap();
            }

            let consumed = consumer.join().unwrap();
            assert_eq!(consumed, n_producers * items_per_producer);
            assert!(q.pop().is_none());
        });
    }
}
