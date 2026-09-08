#![feature(restricted_std)]

// A thread needs a task to run in and pages for its stack.
quark_rt::manifest!([
    quark_rt::manifest::CapReq::task_mgmt(0),
    quark_rt::manifest::CapReq::phys_alloc(64),
]);

thread_local! {
    static SEEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn bump_seen() -> u64 {
    SEEN.with(|c| {
        c.set(c.get() + 1);
        c.get()
    })
}

fn main() {
    // Print program arguments
    let args: Vec<String> = std::env::args().collect();
    println!("argc={}", args.len());
    for (i, arg) in args.iter().enumerate() {
        println!("  argv[{}] = \"{}\"", i, arg);
    }

    println!("Hello from user space!");

    // Test Vec
    let v: Vec<u32> = (0..10).map(|i| i * i).collect();
    println!("Vec: {:?}", v);

    // Test String
    let mut s = String::from("Quark");
    s.push_str(" has a heap!");
    println!("{}", s);

    // Test larger allocation
    let big: Vec<u8> = (0..8192u16).map(|i| (i & 0xFF) as u8).collect();
    println!("Big vec len: {}", big.len());

    // Test deallocation + reuse
    drop(big);
    let reuse: Vec<u64> = (0..100).collect();
    println!("Reuse vec len: {}", reuse.len());

    let handle = std::thread::spawn(|| 40u64 + 2);
    match handle.join() {
        Ok(v) => println!("thread returned {}", v),
        Err(_) => println!("thread: join failed"),
    }

    // A closure that captures moves through the global allocator, while the
    // thread's own control block goes through `System`. Both allocators are
    // exercised at once, which is the case that used to corrupt the heap.
    let owned = String::from("captured");
    let h = std::thread::spawn(move || format!("{} by the thread", owned));
    match h.join() {
        Ok(s) => println!("thread said: {}", s),
        Err(_) => println!("thread: join failed"),
    }

    // Thread-local storage: the same static, read from two threads, must give
    // each its own copy.
    println!("main sees SEEN = {}", bump_seen());
    let t = std::thread::spawn(|| bump_seen());
    let from_thread = t.join().unwrap_or(0);
    println!("thread sees SEEN = {}", from_thread);
    println!("main sees SEEN = {}", bump_seen());

    // Several at once, each with its own stack and thread-locals.
    let mut hs = Vec::new();
    for i in 0..4u64 {
        hs.push(std::thread::spawn(move || {
            for _ in 0..=i {
                bump_seen();
            }
            SEEN.with(|c| c.get()) * 1000 + i
        }));
    }
    let results: Vec<u64> = hs.into_iter().map(|h| h.join().unwrap_or(0)).collect();
    println!("four threads: {:?}", results);

    println!("Heap test passed!");

    // Timed waits. The kernel blocks for these now; it used to have no timed
    // futex, so std's wait_timeout was polling and yielding until the deadline.
    {
        use std::sync::{Arc, Condvar, Mutex};
        let pair = Arc::new((Mutex::new(false), Condvar::new()));

        // Nobody signals: this must come back as a timeout, near the deadline.
        let t0 = std::time::Instant::now();
        let (lock, cv) = &*pair;
        let guard = lock.lock().unwrap();
        let (_g, r) = cv
            .wait_timeout(guard, std::time::Duration::from_millis(300))
            .unwrap();
        println!(
            "condvar timeout: {} after {}ms",
            r.timed_out(),
            t0.elapsed().as_millis()
        );
    }
    {
        use std::sync::{Arc, Condvar, Mutex};
        let pair = Arc::new((Mutex::new(false), Condvar::new()));
        let signaller = Arc::clone(&pair);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let (lock, cv) = &*signaller;
            *lock.lock().unwrap() = true;
            cv.notify_one();
        });

        // Signalled well inside the deadline: this must return early.
        let t0 = std::time::Instant::now();
        let (lock, cv) = &*pair;
        let mut guard = lock.lock().unwrap();
        while !*guard {
            let (g, r) = cv
                .wait_timeout(guard, std::time::Duration::from_millis(2000))
                .unwrap();
            guard = g;
            if r.timed_out() {
                break;
            }
        }
        println!(
            "condvar notify: signalled={} after {}ms",
            *guard,
            t0.elapsed().as_millis()
        );
    }

    // Test sleep
    let t0 = std::time::Instant::now();
    println!("Sleeping 500ms...");
    std::thread::sleep(std::time::Duration::from_millis(500));
    let elapsed = t0.elapsed();
    println!("Woke up ({}ms elapsed)", elapsed.as_millis());
}
