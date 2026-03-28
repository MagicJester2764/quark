#![feature(restricted_std)]

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

    println!("Heap test passed!");

    // Test sleep
    let t0 = std::time::Instant::now();
    println!("Sleeping 500ms...");
    std::thread::sleep(std::time::Duration::from_millis(500));
    let elapsed = t0.elapsed();
    println!("Woke up ({}ms elapsed)", elapsed.as_millis());
}
