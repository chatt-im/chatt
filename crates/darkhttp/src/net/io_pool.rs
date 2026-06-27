use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

type Task = Box<dyn FnOnce() + Send + 'static>;

struct Queue {
    tasks: Mutex<VecDeque<Task>>,
    has_tasks: Condvar,
    stopping: AtomicBool,
}

pub(crate) struct IoPool {
    queue: Arc<Queue>,
    workers: Vec<thread::JoinHandle<()>>,
}

impl IoPool {
    pub(crate) fn new(threads: usize) -> Self {
        let queue = Arc::new(Queue {
            tasks: Mutex::new(VecDeque::new()),
            has_tasks: Condvar::new(),
            stopping: AtomicBool::new(false),
        });
        let mut workers = Vec::with_capacity(threads);
        for _ in 0..threads.max(1) {
            let queue = Arc::clone(&queue);
            workers.push(thread::spawn(move || worker_loop(queue)));
        }
        Self { queue, workers }
    }

    pub(crate) fn execute<F>(&self, task: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let mut tasks = self.queue.tasks.lock().unwrap();
        tasks.push_back(Box::new(task));
        self.queue.has_tasks.notify_one();
    }
}

impl Drop for IoPool {
    fn drop(&mut self) {
        self.queue.stopping.store(true, Ordering::SeqCst);
        self.queue.has_tasks.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn worker_loop(queue: Arc<Queue>) {
    loop {
        let task = {
            let mut tasks = queue.tasks.lock().unwrap();
            loop {
                if let Some(task) = tasks.pop_front() {
                    break task;
                }
                if queue.stopping.load(Ordering::SeqCst) {
                    return;
                }
                tasks = queue.has_tasks.wait(tasks).unwrap();
            }
        };
        task();
    }
}
