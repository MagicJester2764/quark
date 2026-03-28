use crate::ffi::CStr;
use crate::io;
use crate::num::NonZero;
use crate::time::Duration;

pub struct Thread(!);

impl Thread {
    pub unsafe fn new(_stack: usize, _p: Box<dyn FnOnce()>) -> io::Result<Thread> {
        Err(io::Error::UNSUPPORTED_PLATFORM)
    }

    pub fn yield_now() {
        quark_rt::syscall::sys_yield();
    }

    pub fn set_name(_name: &CStr) {}

    pub fn sleep(dur: Duration) {
        let ms = dur.as_millis() as u64;
        quark_rt::rt::sleep_ms(ms);
    }

    pub fn join(self) {
        self.0
    }
}

pub fn available_parallelism() -> io::Result<NonZero<usize>> {
    Ok(NonZero::new(1).unwrap())
}
