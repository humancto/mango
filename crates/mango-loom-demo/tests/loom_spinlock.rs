#![cfg(loom)]

use loom::sync::Arc;
use mango_loom_demo::Spinlock;

#[test]
fn mutual_exclusion() {
    loom::model(|| {
        let lock = Arc::new(Spinlock::new(0_u32));

        let lock_a = Arc::clone(&lock);
        let t1 = loom::thread::spawn(move || {
            let mut g = lock_a.lock();
            g.with_mut(|v| *v += 1);
        });

        let lock_b = Arc::clone(&lock);
        let t2 = loom::thread::spawn(move || {
            let mut g = lock_b.lock();
            g.with_mut(|v| *v += 1);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        let g = lock.lock();
        g.with(|v| assert_eq!(*v, 2));
    });
}
