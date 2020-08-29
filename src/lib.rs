//! Async executors.
//!
//! # Examples
//!
//! ```
//! use async_executor::Executor;
//! use futures_lite::future;
//!
//! // Create a new executor.
//! let ex = Executor::new();
//!
//! // Spawn a task.
//! let task = ex.spawn(async {
//!     println!("Hello world");
//! });
//!
//! // Run the executor until the task complets.
//! future::block_on(ex.run(task));
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs, missing_debug_implementations, rust_2018_idioms)]

use std::cell::Cell;
use std::future::Future;
use std::marker::PhantomData;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll, Waker};

use concurrent_queue::ConcurrentQueue;
use futures_lite::{future, ready, FutureExt};

/// A runnable future, ready for execution.
///
/// When a future is internally spawned using `async_task::spawn()` or `async_task::spawn_local()`,
/// we get back two values:
///
/// 1. an `async_task::Task<()>`, which we refer to as a `Runnable`
/// 2. an `async_task::JoinHandle<T, ()>`, which is wrapped inside a `Task<T>`
///
/// Once a `Runnable` is run, it "vanishes" and only reappears when its future is woken. When it's
/// woken up, its schedule function is called, which means the `Runnable` gets pushed into a task
/// queue in an executor.
type Runnable = async_task::Task<()>;

/// A spawned future.
///
/// Tasks are also futures themselves and yield the output of the spawned future.
///
/// When a task is dropped, its gets canceled and won't be polled again. To cancel a task a bit
/// more gracefully and wait until it stops running, use the [`cancel()`][Task::cancel()] method.
///
/// Tasks that panic get immediately canceled. Awaiting a canceled task also causes a panic.
///
/// If a task panics, the panic will be thrown into the [`Executor::run()`] or
/// [`LocalExecutor::run()`] invocation that polled it.
///
/// # Examples
///
/// ```
/// use async_executor::Executor;
/// use futures_lite::future;
/// use std::thread;
///
/// let ex = Executor::new();
///
/// // Spawn a future onto the executor.
/// let task = ex.spawn(async {
///     println!("Hello from a task!");
///     1 + 2
/// });
///
/// // Run an executor thread.
/// thread::spawn(move || future::block_on(ex.run(future::pending::<()>())));
///
/// // Wait for the result.
/// assert_eq!(future::block_on(task), 3);
/// ```
#[must_use = "tasks get canceled when dropped, use `.detach()` to run them in the background"]
#[derive(Debug)]
pub struct Task<T>(Option<async_task::JoinHandle<T, ()>>);

impl<T> UnwindSafe for Task<T> {}
impl<T> RefUnwindSafe for Task<T> {}

impl<T> Task<T> {
    /// Detaches the task to let it keep running in the background.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::Executor;
    /// use async_io::Timer;
    /// use std::time::Duration;
    ///
    /// let ex = Executor::new();
    ///
    /// // Spawn a deamon future.
    /// ex.spawn(async {
    ///     loop {
    ///         println!("I'm a daemon task looping forever.");
    ///         Timer::after(Duration::from_secs(1)).await;
    ///     }
    /// })
    /// .detach();
    /// ```
    pub fn detach(mut self) {
        self.0.take().unwrap();
    }

    /// Cancels the task and waits for it to stop running.
    ///
    /// Returns the task's output if it was completed just before it got canceled, or [`None`] if
    /// it didn't complete.
    ///
    /// While it's possible to simply drop the [`Task`] to cancel it, this is a cleaner way of
    /// canceling because it also waits for the task to stop running.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::Executor;
    /// use async_io::Timer;
    /// use futures_lite::future;
    /// use std::thread;
    /// use std::time::Duration;
    ///
    /// let ex = Executor::new();
    ///
    /// // Spawn a deamon future.
    /// let task = ex.spawn(async {
    ///     loop {
    ///         println!("Even though I'm in an infinite loop, you can still cancel me!");
    ///         Timer::after(Duration::from_secs(1)).await;
    ///     }
    /// });
    ///
    /// // Run an executor thread.
    /// thread::spawn(move || future::block_on(ex.run(future::pending::<()>())));
    ///
    /// future::block_on(async {
    ///     Timer::after(Duration::from_secs(3)).await;
    ///     task.cancel().await;
    /// });
    /// ```
    pub async fn cancel(self) -> Option<T> {
        let mut task = self;
        let handle = task.0.take().unwrap();
        handle.cancel();
        handle.await
    }
}

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        if let Some(handle) = &self.0 {
            handle.cancel();
        }
    }
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.0.as_mut().unwrap()).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(output) => Poll::Ready(output.expect("task has failed")),
        }
    }
}

/// The state of a executor.
#[derive(Debug)]
struct State {
    /// The global queue.
    queue: ConcurrentQueue<Runnable>,

    /// Shards of the global queue created by tickers.
    shards: RwLock<Vec<Arc<ConcurrentQueue<Runnable>>>>,

    /// Set to `true` when a sleeping ticker is notified or no tickers are sleeping.
    notified: AtomicBool,

    /// A list of sleeping tickers.
    sleepers: Mutex<Sleepers>,
}

impl State {
    /// Creates state for a new executor.
    fn new() -> State {
        State {
            queue: ConcurrentQueue::unbounded(),
            shards: RwLock::new(Vec::new()),
            notified: AtomicBool::new(true),
            sleepers: Mutex::new(Sleepers {
                count: 0,
                wakers: Vec::new(),
                id_gen: 0,
            }),
        }
    }

    /// Notifies a sleeping ticker.
    #[inline]
    fn notify(&self) {
        if !self
            .notified
            .compare_and_swap(false, true, Ordering::SeqCst)
        {
            let waker = self.sleepers.lock().unwrap().notify();
            if let Some(w) = waker {
                w.wake();
            }
        }
    }
}

/// A list of sleeping tickers.
#[derive(Debug)]
struct Sleepers {
    /// Number of sleeping tickers (both notified and unnotified).
    count: usize,

    /// IDs and wakers of sleeping unnotified tickers.
    ///
    /// A sleeping ticker is notified when its waker is missing from this list.
    wakers: Vec<(u64, Waker)>,

    /// ID generator for sleepers.
    id_gen: u64,
}

impl Sleepers {
    /// Inserts a new sleeping ticker.
    fn insert(&mut self, waker: &Waker) -> u64 {
        let id = self.id_gen;
        self.id_gen += 1;
        self.count += 1;
        self.wakers.push((id, waker.clone()));
        id
    }

    /// Re-inserts a sleeping ticker's waker if it was notified.
    ///
    /// Returns `true` if the ticker was notified.
    fn update(&mut self, id: u64, waker: &Waker) -> bool {
        for item in &mut self.wakers {
            if item.0 == id {
                if !item.1.will_wake(waker) {
                    item.1 = waker.clone();
                }
                return false;
            }
        }

        self.wakers.push((id, waker.clone()));
        true
    }

    /// Removes a previously inserted sleeping ticker.
    ///
    /// Returns `true` if the ticker was notified.
    fn remove(&mut self, id: u64) -> bool {
        self.count -= 1;
        for i in (0..self.wakers.len()).rev() {
            if self.wakers[i].0 == id {
                self.wakers.remove(i);
                return false;
            }
        }
        true
    }

    /// Returns `true` if a sleeping ticker is notified or no tickers are sleeping.
    fn is_notified(&self) -> bool {
        self.count == 0 || self.count > self.wakers.len()
    }

    /// Returns notification waker for a sleeping ticker.
    ///
    /// If a ticker was notified already or there are no tickers, `None` will be returned.
    fn notify(&mut self) -> Option<Waker> {
        if self.wakers.len() == self.count {
            self.wakers.pop().map(|item| item.1)
        } else {
            None
        }
    }
}

/// An async executor.
///
/// # Examples
///
/// A multi-threaded executor:
///
/// ```
/// use async_channel::unbounded;
/// use async_executor::Executor;
/// use easy_parallel::Parallel;
/// use futures_lite::future;
///
/// let ex = Executor::new();
/// let (signal, shutdown) = unbounded::<()>();
///
/// Parallel::new()
///     // Run four executor threads.
///     .each(0..4, |_| future::block_on(ex.run(shutdown.recv())))
///     // Run the main future on the current thread.
///     .finish(|| future::block_on(async {
///         println!("Hello world!");
///         drop(signal);
///     }));
/// ```
#[derive(Debug)]
pub struct Executor {
    state: once_cell::sync::OnceCell<Arc<State>>,
}

impl UnwindSafe for Executor {}
impl RefUnwindSafe for Executor {}

impl Executor {
    /// Creates a new executor.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::Executor;
    ///
    /// let ex = Executor::new();
    /// ```
    pub const fn new() -> Executor {
        Executor {
            state: once_cell::sync::OnceCell::new(),
        }
    }

    /// Spawns a task onto the executor.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::Executor;
    ///
    /// let ex = Executor::new();
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// ```
    pub fn spawn<T: Send + 'static>(
        &self,
        future: impl Future<Output = T> + Send + 'static,
    ) -> Task<T> {
        // Create a task, push it into the queue by scheduling it, and return its `Task` handle.
        let (runnable, handle) = async_task::spawn(future, self.schedule(), ());
        runnable.schedule();
        Task(Some(handle))
    }

    /// Attempts to run a task if at least one is scheduled.
    ///
    /// Running a scheduled task means simply polling its future once.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::Executor;
    ///
    /// let ex = Executor::new();
    /// assert!(!ex.try_tick()); // no tasks to run
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// assert!(ex.try_tick()); // a task was found
    /// ```
    pub fn try_tick(&self) -> bool {
        match self.state().queue.pop() {
            Err(_) => false,
            Ok(r) => {
                // Notify another ticker now to pick up where this ticker left off, just in case
                // running the task takes a long time.
                self.state().notify();

                // Run the task.
                r.run();
                true
            }
        }
    }

    /// Run a single task.
    ///
    /// Running a task means simply polling its future once.
    ///
    /// If no tasks are scheduled when this method is called, it will wait until one is scheduled.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::Executor;
    /// use futures_lite::future;
    ///
    /// let ex = Executor::new();
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// future::block_on(ex.tick()); // runs the task
    /// ```
    pub async fn tick(&self) {
        // Create a ticker that doesn't use sharding.
        let ticker = Ticker::new(self.state());

        // Keep trying until a single `poll_tick()` is successful.
        future::poll_fn(|cx| ticker.poll_tick(cx)).await
    }

    /// Runs the executor until the given future completes.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::Executor;
    /// use futures_lite::future;
    ///
    /// let ex = Executor::new();
    ///
    /// let task = ex.spawn(async { 1 + 2 });
    /// let res = future::block_on(ex.run(async { task.await * 2 }));
    ///
    /// assert_eq!(res, 6);
    /// ```
    pub async fn run<T>(&self, future: impl Future<Output = T>) -> T {
        // Create a ticker that uses sharding.
        let runner = Runner::new(self.state());

        // A future that ticks the executor forever.
        let tick_forever = future::poll_fn(|cx| {
            // Run a batch of tasks.
            for _ in 0..200 {
                ready!(runner.poll_tick(cx));
            }

            // If there are more tasks, yield.
            cx.waker().wake_by_ref();
            Poll::Pending
        });

        // Run `future` and `tick_forever` concurrently until `future` completes.
        future.or(tick_forever).await
    }

    /// Returns a function that schedules a runnable task when it gets woken up.
    fn schedule(&self) -> impl Fn(Runnable) + Send + Sync + 'static {
        let state = self.state().clone();

        // TODO(stjepang): If possible, push into the current shard and notify the ticker.
        move |runnable| {
            state.queue.push(runnable).unwrap();
            state.notify();
        }
    }

    /// Returns a reference to the inner state.
    fn state(&self) -> &Arc<State> {
        self.state.get_or_init(|| Arc::new(State::new()))
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        // TODO(stjepang): Cancel all remaining tasks.
    }
}

impl Default for Executor {
    fn default() -> Executor {
        Executor::new()
    }
}

/// Executes tasks in a work-stealing executor.
///
/// A ticker represents a "worker" in a work-stealing executor.
#[derive(Debug)]
struct Ticker<'a> {
    /// The executor state.
    state: &'a State,

    /// Set to `true` when in sleeping state.
    ///
    /// States a ticker can be in:
    /// 1) Woken.
    /// 2a) Sleeping and unnotified.
    /// 2b) Sleeping and notified.
    sleeping: Cell<Option<u64>>,
}

impl Ticker<'_> {
    /// Creates a ticker and registers it in the executor state.
    fn new(state: &State) -> Ticker<'_> {
        Ticker {
            state,
            sleeping: Cell::new(None),
        }
    }

    /// Moves the ticker into sleeping and unnotified state.
    ///
    /// Returns `false` if the ticker was already sleeping and unnotified.
    fn sleep(&self, waker: &Waker) -> bool {
        let mut sleepers = self.state.sleepers.lock().unwrap();

        match self.sleeping.get() {
            // Move to sleeping state.
            None => self.sleeping.set(Some(sleepers.insert(waker))),

            // Already sleeping, check if notified.
            Some(id) => {
                if !sleepers.update(id, waker) {
                    return false;
                }
            }
        }

        self.state
            .notified
            .swap(sleepers.is_notified(), Ordering::SeqCst);

        true
    }

    /// Moves the ticker into woken state.
    fn wake(&self) {
        if let Some(id) = self.sleeping.take() {
            let mut sleepers = self.state.sleepers.lock().unwrap();
            sleepers.remove(id);

            self.state
                .notified
                .swap(sleepers.is_notified(), Ordering::SeqCst);
        }
    }

    /// Attempts to execute a single task.
    ///
    /// This method takes a scheduled task and polls its future.
    fn poll_tick(&self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            match self.state.queue.pop() {
                Err(_) => {
                    // Move to sleeping and unnotified state.
                    if !self.sleep(cx.waker()) {
                        // If already sleeping and unnotified, return.
                        return Poll::Pending;
                    }
                }
                Ok(r) => {
                    // Wake up.
                    self.wake();

                    // Notify another ticker now to pick up where this ticker left off, just in
                    // case running the task takes a long time.
                    self.state.notify();

                    // Run the task.
                    r.run();
                    return Poll::Ready(());
                }
            }
        }
    }
}

impl Drop for Ticker<'_> {
    fn drop(&mut self) {
        // If this ticker is in sleeping state, it must be removed from the sleepers list.
        if let Some(id) = self.sleeping.take() {
            let mut sleepers = self.state.sleepers.lock().unwrap();
            let notified = sleepers.remove(id);

            self.state
                .notified
                .swap(sleepers.is_notified(), Ordering::SeqCst);

            // If this ticker was notified, then notify another ticker.
            if notified {
                drop(sleepers);
                self.state.notify();
            }
        }
    }
}

/// Executes tasks in a work-stealing executor.
///
/// A ticker represents a "worker" in a work-stealing executor.
#[derive(Debug)]
struct Runner<'a> {
    state: &'a State,

    ticker: Ticker<'a>,

    /// A shard of the global queue.
    shard: Arc<ConcurrentQueue<Runnable>>,

    /// Bumped every time a task is run.
    ticks: Cell<usize>,
}

impl Runner<'_> {
    /// Creates a runner and registers it in the executor state.
    fn new(state: &State) -> Runner<'_> {
        let runner = Runner {
            state,
            ticker: Ticker::new(state),
            shard: Arc::new(ConcurrentQueue::bounded(512)),
            ticks: Cell::new(0),
        };
        state.shards.write().unwrap().push(runner.shard.clone());
        runner
    }

    /// Attempts to execute a single task.
    ///
    /// This method takes a scheduled task and polls its future.
    fn poll_tick(&self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            match self.search() {
                None => {
                    // Move to sleeping and unnotified state.
                    if !self.ticker.sleep(cx.waker()) {
                        // If already sleeping and unnotified, return.
                        return Poll::Pending;
                    }
                }
                Some(r) => {
                    // Wake up.
                    self.ticker.wake();

                    // Notify another ticker now to pick up where this ticker left off, just in
                    // case running the task takes a long time.
                    self.state.notify();

                    // Bump the ticker.
                    let ticks = self.ticks.get();
                    self.ticks.set(ticks.wrapping_add(1));

                    // Steal tasks from the global queue to ensure fair task scheduling.
                    if ticks % 64 == 0 {
                        steal(&self.state.queue, &self.shard);
                    }

                    // Run the task.
                    r.run();
                    return Poll::Ready(());
                }
            }
        }
    }

    /// Finds the next task to run.
    fn search(&self) -> Option<Runnable> {
        // Try the shard.
        if let Ok(r) = self.shard.pop() {
            return Some(r);
        }

        // Try stealing from the global queue.
        if let Ok(r) = self.state.queue.pop() {
            steal(&self.state.queue, &self.shard);
            return Some(r);
        }

        // Try stealing from other shards.
        let shards = self.state.shards.read().unwrap();

        // Pick a random starting point in the iterator list and rotate the list.
        let n = shards.len();
        let start = fastrand::usize(..n);
        let iter = shards.iter().chain(shards.iter()).skip(start).take(n);

        // Remove this ticker's shard.
        let iter = iter.filter(|shard| !Arc::ptr_eq(shard, &self.shard));

        // Try stealing from each shard in the list.
        for shard in iter {
            steal(shard, &self.shard);
            if let Ok(r) = self.shard.pop() {
                return Some(r);
            }
        }

        None
    }
}

impl Drop for Runner<'_> {
    fn drop(&mut self) {
        // Remove the shard.
        self.state
            .shards
            .write()
            .unwrap()
            .retain(|shard| !Arc::ptr_eq(shard, &self.shard));

        // Re-schedule remaining tasks in the shard.
        while let Ok(r) = self.shard.pop() {
            r.schedule();
        }
    }
}

/// Steals some items from one queue into another.
fn steal<T>(src: &ConcurrentQueue<T>, dest: &ConcurrentQueue<T>) {
    // Half of `src`'s length rounded up.
    let mut count = (src.len() + 1) / 2;

    if count > 0 {
        // Don't steal more than fits into the queue.
        if let Some(cap) = dest.capacity() {
            count = count.min(cap - dest.len());
        }

        // Steal tasks.
        for _ in 0..count {
            if let Ok(t) = src.pop() {
                assert!(dest.push(t).is_ok());
            } else {
                break;
            }
        }
    }
}

/// A thread-local executor.
///
/// The executor can only be run on the thread that created it.
///
/// # Examples
///
/// ```
/// use async_executor::LocalExecutor;
/// use futures_lite::future;
///
/// let local_ex = LocalExecutor::new();
///
/// future::block_on(local_ex.run(async {
///     println!("Hello world!");
/// }));
/// ```
#[derive(Debug)]
pub struct LocalExecutor {
    /// The inner executor.
    inner: once_cell::unsync::OnceCell<Executor>,

    /// Make sure the type is `!Send` and `!Sync`.
    _marker: PhantomData<Rc<()>>,
}

impl UnwindSafe for LocalExecutor {}
impl RefUnwindSafe for LocalExecutor {}

impl LocalExecutor {
    /// Creates a single-threaded executor.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::LocalExecutor;
    ///
    /// let local_ex = LocalExecutor::new();
    /// ```
    pub const fn new() -> LocalExecutor {
        LocalExecutor {
            inner: once_cell::unsync::OnceCell::new(),
            _marker: PhantomData,
        }
    }

    /// Spawns a task onto the executor.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::LocalExecutor;
    ///
    /// let local_ex = LocalExecutor::new();
    ///
    /// let task = local_ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// ```
    pub fn spawn<T: 'static>(&self, future: impl Future<Output = T> + 'static) -> Task<T> {
        // Create a task, push it into the queue by scheduling it, and return its `Task` handle.
        let (runnable, handle) = async_task::spawn_local(future, self.schedule(), ());
        runnable.schedule();
        Task(Some(handle))
    }

    /// Attempts to run a task if at least one is scheduled.
    ///
    /// Running a scheduled task means simply polling its future once.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::LocalExecutor;
    ///
    /// let ex = LocalExecutor::new();
    /// assert!(!ex.try_tick()); // no tasks to run
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// assert!(ex.try_tick()); // a task was found
    /// ```
    pub fn try_tick(&self) -> bool {
        self.inner().try_tick()
    }

    /// Run a single task.
    ///
    /// Running a task means simply polling its future once.
    ///
    /// If no tasks are scheduled when this method is called, it will wait until one is scheduled.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::LocalExecutor;
    /// use futures_lite::future;
    ///
    /// let ex = LocalExecutor::new();
    ///
    /// let task = ex.spawn(async {
    ///     println!("Hello world");
    /// });
    /// future::block_on(ex.tick()); // runs the task
    /// ```
    pub async fn tick(&self) {
        self.inner().tick().await
    }

    /// Runs the executor until the given future completes.
    ///
    /// # Examples
    ///
    /// ```
    /// use async_executor::LocalExecutor;
    /// use futures_lite::future;
    ///
    /// let local_ex = LocalExecutor::new();
    ///
    /// let task = local_ex.spawn(async { 1 + 2 });
    /// let res = future::block_on(local_ex.run(async { task.await * 2 }));
    ///
    /// assert_eq!(res, 6);
    /// ```
    pub async fn run<T>(&self, future: impl Future<Output = T>) -> T {
        self.inner().run(future).await
    }

    /// Returns a function that schedules a runnable task when it gets woken up.
    fn schedule(&self) -> impl Fn(Runnable) + Send + Sync + 'static {
        let state = self.inner().state().clone();

        move |runnable| {
            state.queue.push(runnable).unwrap();
            state.notify();
        }
    }

    /// Returns a reference to the inner executor.
    fn inner(&self) -> &Executor {
        self.inner.get_or_init(|| Executor::new())
    }
}

impl Default for LocalExecutor {
    fn default() -> LocalExecutor {
        LocalExecutor::new()
    }
}
