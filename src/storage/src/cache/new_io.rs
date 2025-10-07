use std::{ops::Range, os::fd::RawFd, pin::Pin, sync::{atomic::{AtomicBool, Ordering}, Arc, LazyLock, Mutex}, task::{Context, Poll, Waker}, thread};

use io_uring::{cqueue, opcode, squeue, IoUring};

pub trait IoTask: Send + Sync {
    #[inline]
    fn set_waker(self: &Self, waker: Waker) {
        let mut guard = self.waker().lock().unwrap();
        *guard = Some(waker);
    }

    fn waker(self: &Self) -> &Mutex<Option<Waker>>;

    fn get_sqe(&self, user_data: u64) -> squeue::Entry;

    fn completed(&self) -> &AtomicBool;

    fn notify_waker(&self) {
        self.completed().store(true, Ordering::Relaxed);
        let mut guard = self.waker().lock().unwrap();
        if let Some(waker) = guard.take() {
            waker.wake();
        }
    }
}

#[allow(unused)]
pub struct FileReadTask {
    base_ptr: *mut u8,
    fd: RawFd,
    completed: AtomicBool,
    waker: Mutex<Option<Waker>>,
    range: Range<u64>,
}

impl FileReadTask {
    #[allow(unused)]
    pub fn new(base_ptr: *mut u8, range: Range<u64>, fd: RawFd) -> FileReadTask {
        return FileReadTask {base_ptr, fd, completed: AtomicBool::new(false), waker: Mutex::<Option<Waker>>::new(None), range: range}
    }

    #[inline]
    pub fn ptr(&self) -> *const u8 {
        self.base_ptr as *const u8
    }
}

impl IoTask for FileReadTask {
    #[inline]
    fn waker(self: &Self) -> &Mutex<Option<Waker>> {
        return &self.waker;
    }
    
    #[inline]
    fn get_sqe(&self, user_data: u64) -> squeue::Entry {
        let read_op = opcode::Read::new(
            io_uring::types::Fd(self.fd),
            self.base_ptr,
            (self.range.end - self.range.start) as u32, // Logically, this should be the remaining number of bytes, but that fails...
        );
        let sqe = read_op
            .offset(self.range.start)
            .build()
            .user_data(user_data as u64);
        sqe
    }
    
    #[inline]
    fn completed(&self) -> &AtomicBool {
        &self.completed
    }
}

unsafe impl Send for FileReadTask {}
unsafe impl Sync for FileReadTask {}

pub struct FileWriteTask {
    base_ptr: *const u8,
    num_bytes: usize,
    fd: RawFd,
    completed: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl FileWriteTask {
    pub(crate) fn new(base_ptr: *const u8, num_bytes: usize, fd: RawFd) -> FileWriteTask {
        return FileWriteTask {base_ptr, num_bytes, fd, completed: AtomicBool::new(false), waker: Mutex::<Option<Waker>>::new(None)}
    }
}

impl IoTask for FileWriteTask {
    #[inline]
    fn waker(self: &Self) -> &Mutex<Option<Waker>> {
        return &self.waker;
    }
    
    #[inline]
    fn get_sqe(&self, user_data: u64) -> squeue::Entry {
        let padding = if self.num_bytes % 4096 == 0 {
            0
        } else {
            4096 - self.num_bytes % 4096
        };
        let num_bytes_aligned = self.num_bytes + padding;
        let write_op = opcode::Write::new(
            io_uring::types::Fd(self.fd),
            self.base_ptr,
            num_bytes_aligned as u32,
        );

        let sqe = write_op
            .offset(0u64)
            .build()
            .user_data(user_data);
        sqe
    }

    #[inline]
    fn completed(&self) -> &AtomicBool {
        &self.completed
    }
}

unsafe impl Send for FileWriteTask {}
unsafe impl Sync for FileWriteTask {}

static ENABLED: AtomicBool = AtomicBool::new(true);

pub struct IoUringThreadpool {
    sender: crossbeam_channel::Sender<Arc<dyn IoTask>>,
    worker: Option<thread::JoinHandle<()>>,
}

unsafe impl Sync for IoUringThreadpool {}

pub(crate) static IO_URING_THREAD_POOL_INST: LazyLock<IoUringThreadpool> = LazyLock::new(|| IoUringThreadpool::new());

impl IoUringThreadpool {
    const NUM_ENTRIES: u32 = 64;

    fn new() -> IoUringThreadpool {        
        let (sender, receiver) = crossbeam_channel::unbounded::<Arc<dyn IoTask>>();

        let mut builder = IoUring::<squeue::Entry, cqueue::Entry>::builder();
        builder.setup_iopoll();
        // Add a similar argument to the worker thread as well to sleep when not busy
        builder.setup_sqpoll(50000);
        let ring = builder
            .build(Self::NUM_ENTRIES)
            .expect("Failed to build IoUring instance");

        let receiver_clone = receiver.clone();
        let worker = thread::spawn(move || {
            let mut uring_worker = UringWorker::new(receiver_clone, ring);
            uring_worker.thread_loop();
        });

        IoUringThreadpool {
            sender: sender,
            worker: Some(worker),
        }
    }

    pub(crate) fn submit_task(self: &Self, task: Arc<dyn IoTask>) {
        self.sender.send(task.clone()).expect("Failed to submit task through channel");
    }

}

impl Drop for IoUringThreadpool {
    fn drop(&mut self) {
        ENABLED.store(false, Ordering::Relaxed);
        let worker = self.worker.take();
        if worker.is_some() {
            let _ = worker.unwrap().join();
        }
    }
}

struct UringWorker {
    channel: crossbeam_channel::Receiver<Arc<dyn IoTask>>,
    ring: io_uring::IoUring,
    // Assumption: There wont be more than 2^16 tasks on-the-fly at once
    op_counter: u16,
    completions_array: Vec<usize>,
    submitted_tasks: Vec<Option<Arc<dyn IoTask>>>,
    inflight_requests: u32,
}

impl UringWorker {
    fn new(channel: crossbeam_channel::Receiver<Arc<dyn IoTask>>, ring: io_uring::IoUring) -> UringWorker {
        let mut completions_array = Vec::<usize>::new();
        completions_array.resize(1<<16, 0);

        let mut tasks = Vec::<Option<Arc<dyn IoTask>>>::new();
        tasks.resize(IoUringThreadpool::NUM_ENTRIES as usize, None);
        UringWorker { channel: channel, ring: ring, op_counter: 0, completions_array: completions_array, submitted_tasks: tasks, inflight_requests: 0 }
    }

    fn thread_loop(self: &mut Self) {
        loop {
            if !ENABLED.load(Ordering::Relaxed) {
                break;
            }
            
            while self.inflight_requests < IoUringThreadpool::NUM_ENTRIES {
                let res = self.channel.try_recv();
                if res.is_err() {break;}
                let task = res.unwrap();
                // Consume tasks from channel and submit them to the ring
                {
                    let sq = &mut (self.ring.submission());
                    let sqe = task.get_sqe((self.op_counter as u64)<<48);
                    
                    unsafe {
                        sq.push(&sqe)
                            .expect("Failed to push to submission queue");
                    }
                    sq.sync();
                    self.submitted_tasks[self.op_counter as usize] = Some(task);
                    self.completions_array[self.op_counter as usize] = 1;
                    self.op_counter = (self.op_counter + 1) & 63;
                }
                self.ring.submit().expect("Failed to submit");
                
                self.inflight_requests += 1;
            }

            self.poll_completions();
        }
    }

    fn poll_completions(self: &mut Self) {
        let cq = &mut self.ring.completion();
        loop {
            cq.sync();
            match cq.next() {
                Some(cqe) => {
                    self.inflight_requests -= 1;
                    let errno = -cqe.result();
                    let err = std::io::Error::from_raw_os_error(errno);
                    assert!(
                        cqe.result() > 0,
                        "Read cqe result error: {err}"
                    );
                    let opcode = (cqe.user_data()>>48) as usize;
                    let remaining = &mut self.completions_array[opcode];
                    *remaining -= 1;
                    if *remaining == 0 {
                        self.submitted_tasks[opcode].as_ref().unwrap().notify_waker();
                    }
                },
                None => {
                    break;
                }
            }
        }
    }
}

enum UringState {
    Initialized,
    Submitted,
}

pub struct UringFuture<T>
    where T: IoTask {
    task: Arc<T>,
    state: UringState,
}

impl<T> UringFuture<T>
        where T: IoTask {
    pub fn new(task: Arc<T>) -> UringFuture<T> {
        return UringFuture { task: task, state: UringState::Initialized }
    }
}

impl<T> Future for UringFuture<T> 
        where T: IoTask + 'static {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match self.state {
            UringState::Initialized => {
                // Measure the io latency
                IO_URING_THREAD_POOL_INST.submit_task(self.task.clone());
                self.task.set_waker(cx.waker().clone());
                self.state = UringState::Submitted;
                return Poll::Pending;
            },
            UringState::Submitted => {
                match self.task.completed().load(Ordering::Relaxed) {
                    false => {
                        self.task.set_waker(cx.waker().clone());
                        return Poll::Pending;
                    },
                    true => {
                        return Poll::Ready(());
                    }
                }
            }
        }
    }
}