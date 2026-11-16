use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Condvar, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

pub const INPUT_WAKE_DELAY: Duration = Duration::from_micros(1500);

pub struct Wake {
    flag: AtomicBool,
    lock: Mutex<()>,
    cv: Condvar,
    delayed: Mutex<mpsc::Sender<Instant>>,
}

impl Wake {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Instant>();
        std::thread::spawn(move || {
            while let Ok(mut deadline) = rx.recv() {
                while let Ok(next) = rx.try_recv() {
                    deadline = deadline.max(next);
                }
                let now = Instant::now();
                if deadline > now {
                    std::thread::sleep(deadline - now);
                }
                global().signal();
            }
        });
        Self {
            flag: AtomicBool::new(false),
            lock: Mutex::new(()),
            cv: Condvar::new(),
            delayed: Mutex::new(tx),
        }
    }

    pub fn signal(&self) {
        self.flag.store(true, Ordering::Release);
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.cv.notify_all();
    }

    pub fn notify_after(&self, delay: Duration) {
        let sender = self.delayed.lock().unwrap_or_else(|e| e.into_inner());
        let _ = sender.send(Instant::now() + delay);
    }

    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        if self.flag.swap(false, Ordering::Acquire) {
            return true;
        }
        let guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self
            .cv
            .wait_timeout_while(guard, timeout, |_| !self.flag.load(Ordering::Acquire));
        self.flag.swap(false, Ordering::Acquire)
    }
}

pub fn global() -> &'static Wake {
    static WAKE: OnceLock<Wake> = OnceLock::new();
    WAKE.get_or_init(Wake::new)
}
