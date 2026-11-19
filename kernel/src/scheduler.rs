//! Timer-driven round-robin scheduler.
//!
//! This is a **preemptive** scheduler with **real context switches**:
//! every task has its own 8 KiB stack, and switching saves the
//! callee-saved registers and stack pointer of the outgoing task and
//! restores them for the incoming one (see `context_switch` below).
//! A task runs for at most one quantum (`QUANTUM_TICKS` timer ticks)
//! before the timer interrupt handler preempts it — switching to the
//! next ready task right out of the interrupt handler — so a task
//! that never yields (see `hog_task`) can no longer starve the rest.
//! Tasks that do yield still cooperate: yielding returns to the
//! scheduler immediately, without waiting for the quantum.
//!
//! How the preemption stays safe on a single CPU with spin locks:
//! * The timer handler sends the PIC end-of-interrupt *before*
//!   switching, so the next timer IRQ is not blocked behind the
//!   switch. When the preempted task is later resumed, it returns
//!   into the handler, which `iret`s back to the interrupted code.
//! * Preemption is skipped while the running task holds the serial
//!   or framebuffer lock (checked with `is_locked`), because the
//!   incoming task might print and spin on that lock with interrupts
//!   still disabled from the interrupt entry — a deadlock.
//! * The task table is taken with `try_lock`, never a blocking lock:
//!   if the interrupted task was in the middle of updating it, this
//!   tick's preemption is simply skipped.
//! * The task-table lock is dropped before the actual switch, so the
//!   incoming task (or the scheduler, when it yields) can take it.
//! * Interrupts are disabled on interrupt entry and stay that way
//!   in the task switched to from the handler, so it cannot itself
//!   be preempted mid-step; it yields back to the scheduler, which
//!   re-enables interrupts while waiting for the next tick.
//!
//! Tasks:
//! * `counter` / `printer` — demo tasks, 5 steps each, run at boot
//!   to prove tasks interleave.
//! * `hog` — a demo task that spins for ~2 s and *never yields*,
//!   to prove preemption: without it, `counter` and `printer`
//!   would not run until `hog` finished.
//! * `heartbeat` — a background task that just counts steps; it runs
//!   for one step every ~2 s while the shell is idle, and the `tasks`
//!   shell command shows how far it has got.

use crate::interrupts::TIMER_TICKS;
use alloc::boxed::Box;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use spin::Mutex;

pub const MAX_TASKS: usize = 4;
const STACK_SIZE: usize = 8192;
/// Ticks between background (heartbeat) task steps (~2 s at 18.2 Hz).
const BACKGROUND_TICK_INTERVAL: u64 = 36;
/// Preemption quantum: a task runs at most this many timer ticks
/// (~165 ms at 18.2 Hz) before the timer handler switches away.
pub const QUANTUM_TICKS: u64 = 3;
/// How many times the timer handler has preempted a task.
pub static PREEMPTIONS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskState {
    Ready,
    Running,
    Finished,
}

pub struct Task {
    pub id: usize,
    pub name: &'static str,
    pub state: TaskState,
    /// How many times the scheduler has run this task (one step each).
    /// Counted in `yield_now`, so a step counts for the task that ran
    /// it even when it was preempted into rather than switched to
    /// from the scheduler (see `maybe_preempt`).
    pub steps: u64,
    pub entry: fn(),
}

static TASKS: Mutex<[Option<Task>; MAX_TASKS]> = Mutex::new([None, None, None, None]);
/// Saved stack pointers, one per task. Written by `context_switch`.
static TASK_RSPS: [AtomicUsize; MAX_TASKS] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];
/// Saved stack pointer of the main (scheduler/shell) context.
static SCHEDULER_RSP: AtomicUsize = AtomicUsize::new(0);
/// Id of the task currently running, or `usize::MAX` for the main context.
static CURRENT_TASK: AtomicUsize = AtomicUsize::new(usize::MAX);
static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
static HEARTBEAT_ID: AtomicUsize = AtomicUsize::new(usize::MAX);
static LAST_BACKGROUND_TICK: AtomicU64 = AtomicU64::new(0);
/// Timer tick at which the currently running task started its quantum.
static TASK_START_TICK: AtomicU64 = AtomicU64::new(0);

// The context switch itself, written in assembly so there is no Rust
// prologue/epilogue to trip over: push the callee-saved registers,
// store the old stack pointer, load the new one, pop the registers
// of the incoming context and `ret` into it.
core::arch::global_asm!(
    ".global context_switch",
    "context_switch:",
    "push rbx",
    "push rbp",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov [rdi], rsp",
    "mov rsp, rsi",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbp",
    "pop rbx",
    "ret",
);

unsafe extern "C" {
    fn context_switch(old_rsp: *mut usize, new_rsp: usize);
}

/// The first thing a new task runs (entered via `ret` from
/// `context_switch`): call the task's entry function, then mark the
/// task finished and switch back to the scheduler, never returning.
extern "C" fn task_trampoline() -> ! {
    let id = CURRENT_TASK.load(Ordering::SeqCst);
    let entry = {
        let tasks = TASKS.lock();
        tasks[id].as_ref().map(|t| t.entry)
    };
    if let Some(entry) = entry {
        entry();
    }
    {
        let mut tasks = TASKS.lock();
        if let Some(task) = tasks[id].as_mut() {
            task.state = TaskState::Finished;
        }
    }
    CURRENT_TASK.store(usize::MAX, Ordering::SeqCst);
    let sched_rsp = SCHEDULER_RSP.load(Ordering::SeqCst);
    unsafe { context_switch(TASK_RSPS[id].as_ptr(), sched_rsp) };
    loop {
        x86_64::instructions::hlt();
    }
}

fn create_task(name: &'static str, entry: fn()) -> usize {
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    // Heap-allocated stack, leaked so it lives for the life of the kernel.
    let stack = Box::leak(Box::new([0u8; STACK_SIZE]));
    let stack_top = stack.as_mut_ptr() as usize + STACK_SIZE;
    // Lay out the initial saved context so the first switch to this
    // task pops the registers and `ret`s into the trampoline:
    // [rsp] = r15, r14, r13, r12, rbp, rbx, trampoline address.
    let mut rsp = stack_top & !15usize;
    unsafe {
        (rsp as *mut usize)
            .sub(1)
            .write(task_trampoline as *const () as usize);
        for i in 0..6 {
            (rsp as *mut usize).sub(2 + i).write(0);
        }
        rsp -= 7 * 8;
    }
    TASK_RSPS[id].store(rsp, Ordering::SeqCst);
    {
        let mut tasks = TASKS.lock();
        tasks[id] = Some(Task {
            id,
            name,
            state: TaskState::Ready,
            steps: 0,
            entry,
        });
    }
    id
}

/// Yield the CPU back to the scheduler. Called from inside a task.
/// Counts one step for this task first: counting here (rather than
/// in `switch_to`) keeps the count correct when a task was entered
/// by preemption, since yielding then returns to the scheduler
/// through a different task's `switch_to`.
pub fn yield_now() {
    let id = CURRENT_TASK.load(Ordering::SeqCst);
    if id == usize::MAX {
        return;
    }
    {
        let mut tasks = TASKS.lock();
        if let Some(task) = tasks[id].as_mut() {
            task.steps += 1;
        }
    }
    let sched_rsp = SCHEDULER_RSP.load(Ordering::SeqCst);
    unsafe { context_switch(TASK_RSPS[id].as_ptr(), sched_rsp) };
}

/// Switch from the main context to task `id` for one step, then return
/// here when the task yields or finishes — or when a task it was
/// preempted to yields (see `maybe_preempt`).
fn switch_to(id: usize) {
    let new_rsp = TASK_RSPS[id].load(Ordering::SeqCst);
    CURRENT_TASK.store(id, Ordering::SeqCst);
    TASK_START_TICK.store(TIMER_TICKS.load(Ordering::SeqCst), Ordering::SeqCst);
    {
        let mut tasks = TASKS.lock();
        if let Some(task) = tasks[id].as_mut() {
            task.state = TaskState::Running;
        }
    }
    unsafe { context_switch(SCHEDULER_RSP.as_ptr(), new_rsp) };
    // Back in the main context: the task yielded or finished — or a
    // task it was preempted to yielded (see `maybe_preempt`). Steps
    // are counted in `yield_now`, not here, so just restore the
    // state.
    CURRENT_TASK.store(usize::MAX, Ordering::SeqCst);
    {
        let mut tasks = TASKS.lock();
        if let Some(task) = tasks[id].as_mut() {
            if task.state == TaskState::Running {
                task.state = TaskState::Ready;
            }
        }
    }
}

/// Called from the timer interrupt handler, *after* the PIC
/// end-of-interrupt, to preempt the running task if its quantum has
/// expired. Switches directly to the next ready task (skipping the
/// background heartbeat task). If there is no other ready task, or
/// the quantum has not expired, or a lock check fails, it returns
/// and the handler `iret`s back to the interrupted task.
///
/// Safety notes (see the module docs): the task-table lock is taken
/// with `try_lock` and dropped before the switch, and preemption is
/// skipped while the serial or framebuffer lock is held.
pub fn maybe_preempt() {
    let current = CURRENT_TASK.load(Ordering::SeqCst);
    if current == usize::MAX {
        return; // main (scheduler/shell) context: nothing to preempt
    }
    let now = TIMER_TICKS.load(Ordering::SeqCst);
    if now.wrapping_sub(TASK_START_TICK.load(Ordering::SeqCst)) < QUANTUM_TICKS {
        return;
    }
    // The incoming task may print; never switch to it while the
    // running task holds a print lock (see module docs).
    if crate::SERIAL.is_locked() || crate::framebuffer::FRAMEBUFFER.is_locked() {
        return;
    }
    let next = {
        let mut tasks = match TASKS.try_lock() {
            Some(guard) => guard,
            None => return,
        };
        let mut next = None;
        for offset in 1..=MAX_TASKS {
            let id = (current + offset) % MAX_TASKS;
            if let Some(task) = tasks[id].as_ref() {
                if task.state == TaskState::Ready && task.name != "heartbeat" {
                    next = Some(id);
                    break;
                }
            }
        }
        let next = match next {
            Some(id) => id,
            None => return,
        };
        if let Some(task) = tasks[next].as_mut() {
            task.state = TaskState::Running;
        }
        if let Some(task) = tasks[current].as_mut() {
            if task.state == TaskState::Running {
                task.state = TaskState::Ready;
            }
        }
        next
        // `tasks` guard is dropped here, before the switch below.
    };
    CURRENT_TASK.store(next, Ordering::SeqCst);
    TASK_START_TICK.store(now, Ordering::SeqCst);
    PREEMPTIONS.fetch_add(1, Ordering::SeqCst);
    let new_rsp = TASK_RSPS[next].load(Ordering::SeqCst);
    unsafe { context_switch(TASK_RSPS[current].as_ptr(), new_rsp) };
    // When this task is switched back to, execution resumes here and
    // returns into the timer handler, which `iret`s to the code that
    // was interrupted.
}

/// Snapshot of the task table for the `tasks` shell command.
pub fn list_tasks() -> alloc::vec::Vec<(usize, &'static str, TaskState, u64)> {
    let tasks = TASKS.lock();
    tasks
        .iter()
        .flatten()
        .map(|t| (t.id, t.name, t.state, t.steps))
        .collect()
}

// --- Demo tasks -----------------------------------------------------------

fn counter_task() {
    for i in 0..5 {
        crate::sprintln!("task counter: step {} (value {})", i, i);
        crate::fb_println!("task counter: step {} (value {})", i, i);
        yield_now();
    }
}

fn printer_task() {
    for i in 0..5 {
        crate::sprintln!("task printer: step {}", i);
        crate::fb_println!("task printer: step {}", i);
        yield_now();
    }
}

fn heartbeat_task() {
    loop {
        yield_now();
    }
}

/// A task that never yields: it spins for ~2 s (36 timer ticks),
/// printing a progress line every ~0.5 s. Cooperatively, it would
/// starve `counter` and `printer` for the whole 2 s; preemptively,
/// they run between its progress lines. Its length is measured in
/// timer ticks, not iterations, so it takes ~2 s on any CPU speed.
fn hog_task() {
    let start = TIMER_TICKS.load(Ordering::SeqCst);
    let mut spins: u64 = 0;
    let mut printed: u64 = 0;
    loop {
        spins = spins.wrapping_add(1);
        core::hint::black_box(spins);
        let elapsed = TIMER_TICKS.load(Ordering::SeqCst).wrapping_sub(start);
        if elapsed >= 36 {
            break;
        }
        if elapsed >= (printed + 1) * 9 {
            printed += 1;
            crate::sprintln!(
                "task hog: still running ({} ticks in, {} spins)",
                elapsed,
                spins
            );
            crate::fb_println!(
                "task hog: still running ({} ticks in)",
                elapsed
            );
        }
    }
    crate::sprintln!("task hog: done after {} ticks, {} spins", 36, spins);
    crate::fb_println!("task hog: done");
}

/// Create the demo + background tasks. Call once, after interrupts.
pub fn init() {
    create_task("counter", counter_task);
    create_task("printer", printer_task);
    let heartbeat = create_task("heartbeat", heartbeat_task);
    HEARTBEAT_ID.store(heartbeat, Ordering::SeqCst);
    create_task("hog", hog_task);
}

/// Run the demo tasks to completion, switching only on timer-tick
/// boundaries so the round-robin is driven by the timer. The `hog`
/// task never yields, so it only makes progress in quanta — the
/// timer handler preempts it to the other tasks and back.
pub fn run_demo() {
    crate::sprintln!("scheduler: starting demo — 3 tasks, round-robin on timer ticks");
    crate::fb_println!("scheduler: starting demo");
    loop {
        let ready: alloc::vec::Vec<usize> = {
            let tasks = TASKS.lock();
            tasks
                .iter()
                .flatten()
                .filter(|t| {
                    t.state != TaskState::Finished && t.name != "heartbeat"
                })
                .map(|t| t.id)
                .collect()
        };
        if ready.is_empty() {
            break;
        }
        // Wait for the next timer tick before switching.
        let tick = TIMER_TICKS.load(Ordering::SeqCst);
        while TIMER_TICKS.load(Ordering::SeqCst) == tick {
            x86_64::instructions::interrupts::enable_and_hlt();
        }
        for id in ready {
            let finished = {
                let tasks = TASKS.lock();
                tasks[id].as_ref().map(|t| t.state == TaskState::Finished).unwrap_or(true)
            };
            if !finished {
                switch_to(id);
            }
        }
    }
    crate::sprintln!(
        "scheduler: demo done ({} preemptions)",
        PREEMPTIONS.load(Ordering::SeqCst)
    );
    crate::fb_println!("scheduler: demo done");
}

/// Called from the shell's idle loop: if ~2 s have passed since the
/// heartbeat task last ran, give it one step.
pub fn background_tick() {
    let id = HEARTBEAT_ID.load(Ordering::SeqCst);
    if id == usize::MAX {
        return;
    }
    let now = TIMER_TICKS.load(Ordering::SeqCst);
    let last = LAST_BACKGROUND_TICK.load(Ordering::SeqCst);
    if now.wrapping_sub(last) < BACKGROUND_TICK_INTERVAL {
        return;
    }
    LAST_BACKGROUND_TICK.store(now, Ordering::SeqCst);
    let ready = {
        let tasks = TASKS.lock();
        tasks[id]
            .as_ref()
            .map(|t| t.state == TaskState::Ready)
            .unwrap_or(false)
    };
    if ready {
        switch_to(id);
    }
}
