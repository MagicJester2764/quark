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

    // Test sleep
    let t0 = std::time::Instant::now();
    println!("Sleeping 500ms...");
    std::thread::sleep(std::time::Duration::from_millis(500));
    let elapsed = t0.elapsed();
    println!("Woke up ({}ms elapsed)", elapsed.as_millis());
}
