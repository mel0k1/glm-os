//! The v0.4 scheduler: preemptive round-robin multitasking on SMP.
//!
//! Core idea — "every task is always suspended inside an interrupt frame":
//!
//!   - when a task stops running (preempted, blocked, exited), its full
//!     `Regs` frame lives at the top of its own kernel stack, and
//!     `Task.saved_regs` points at it;
//!   - the asm ISR stub calls `common_handler`, which ends in
//!     `post_dispatch()`; that may return a DIFFERENT task's Regs pointer,
//!     and the stub does `mov rsp, <that>; pop; iretq` — a complete
//!     context switch with one stack pointer move;
//!   - CR3 and the CPU's TSS.RSP0 are reloaded per switch, so user tasks
//!     each get their own address space and their own ring-0 trap stack.
//!
//! v0.4 SMP model:
//!   - one global task table guarded by SCHED_LOCK (IRQ-safe spinlock);
//!     the park -> pick -> run sequence inside post_dispatch is atomic
//!     under the lock, so two CPUs can never grab the same task;
//!   - `current` is per-CPU (see cpu::smp);
//!   - preemption is still ring-3-only via the per-CPU LAPIC timer
//!     (kernel structures stay race-free by design);
//!   - tasks may be pinned to a CPU (`pinned_cpu`) — the resident kidle
//!     of every AP is pinned so each CPU always has an idle home;
//!   - `on_cpu` records which CPU a Running task occupies (ps output).
//!
//! Lock ordering: SCHED -> { FRAMES, KEYBOARD, SERIAL } (leaves never
//! take other locks; CONSOLE/HEAP/VMM are standalone leaves).

use core::arch::asm;
use core::sync::atomic::{AtomicU64, AtomicBool, Ordering};

use crate::console::GLM_GRAY;
use crate::cpu::gdt;
use crate::cpu::idt::Regs;
use crate::cpu::smp;
use crate::klog;
use crate::mem::frames;
use crate::mem::vmm::{self, AddressSpace};
use crate::sync::Spinlock;

/// Maximum simultaneous tasks (including kidles, shell and kstat).
pub const MAX_TASKS: usize = 16;
/// Per-task kernel stack (4 KiB pages).
const KSTACK_PAGES: usize = 16;
const KSTACK_SIZE: usize = KSTACK_PAGES * 4096;

/// "Not pinned to any CPU" sentinel (also used for on_cpu = nowhere).
pub const CPU_ANY: u8 = 0xFF;

// ---------------------------------------------------------------------------
// Syscall numbers shared with user::syscall (kernel threads use them too)
// ---------------------------------------------------------------------------
pub const SYS_YIELD: u64 = 5;
pub const SYS_SLEEP: u64 = 6;
pub const SYS_WAIT: u64 = 7;

/// The one lock that guards the task table and all scheduling decisions.
static SCHED_LOCK: Spinlock<()> = Spinlock::new(());

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Currently on the CPU (on_cpu says which).
    Running,
    /// In the run queue.
    Ready,
    /// Sleeping until `wake_at_ms`.
    Sleeping,
    /// Waiting for a keypress.
    BlockedInput,
    /// Shell waiting for `wait_target` child to exit.
    WaitingChild,
    /// Exited, code in `exit_code`; kernel stack not yet reclaimed.
    Zombie,
    /// Free slot.
    Dead,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Running => "RUN",
            State::Ready => "READY",
            State::Sleeping => "SLEEP",
            State::BlockedInput => "KEYWAIT",
            State::WaitingChild => "WAIT",
            State::Zombie => "ZOMBIE",
            State::Dead => "-",
        }
    }
}

pub struct Task {
    pub pid: u64,
    pub name: [u8; 12],
    pub name_len: usize,
    pub state: State,
    /// CPU this task is pinned to (CPU_ANY = scheduler may place it anywhere).
    pub pinned_cpu: u8,
    /// CPU the task is Running on (CPU_ANY = not running).
    pub on_cpu: u8,
    /// Kernel stack top (virtual, higher-half).
    pub kstack_top: u64,
    /// Kernel stack bottom (virtual) — for reclamation.
    pub kstack_bottom: u64,
    /// Does this task own its kernel stack (shell runs on the boot stack).
    pub kstack_owned: bool,
    /// Where the suspended Regs frame lives (0 = never suspended yet).
    pub saved_regs: u64,
    /// Physical PML4 for this task (kernel tasks share the kernel CR3).
    pub pml4: u64,
    /// Owned user address space (None for kernel tasks).
    pub user_space: Option<AddressSpace>,
    /// Parent pid (for wait()).
    pub parent: u64,
    /// Who this WaitingChild task waits for (0 = any child).
    pub wait_target: u64,
    pub exit_code: i64,
    pub wake_at_ms: u64,
    pub is_user: bool,
}

impl Task {
    const fn dead() -> Self {
        Self {
            pid: 0,
            name: [0; 12],
            name_len: 0,
            state: State::Dead,
            pinned_cpu: CPU_ANY,
            on_cpu: CPU_ANY,
            kstack_top: 0,
            kstack_bottom: 0,
            kstack_owned: false,
            saved_regs: 0,
            pml4: 0,
            user_space: None,
            parent: 0,
            wait_target: 0,
            exit_code: 0,
            wake_at_ms: 0,
            is_user: false,
        }
    }

    pub fn name_str(&self) -> &str {
        // SAFETY: name bytes are always ASCII written by the kernel
        unsafe {
            core::str::from_utf8_unchecked(&self.name[..self.name_len.min(12)])
        }
    }
}

static mut TASKS: [Task; MAX_TASKS] = [const { Task::dead() }; MAX_TASKS];
static NEXT_PID: AtomicU64 = AtomicU64::new(1);
static SWITCHES: AtomicU64 = AtomicU64::new(0);
/// Set true once sched::init() ran (guards the timer epilogue).
static ON_FLAG: AtomicBool = AtomicBool::new(false);

pub fn online() -> bool {
    ON_FLAG.load(Ordering::Relaxed)
}

pub fn switches() -> u64 {
    SWITCHES.load(Ordering::Relaxed)
}

fn tasks() -> &'static mut [Task; MAX_TASKS] {
    unsafe { &mut *core::ptr::addr_of_mut!(TASKS) }
}

fn current_idx() -> usize {
    smp::current_task_idx(smp::cpu_index())
}

pub fn current_pid() -> u64 {
    if !online() {
        return 0;
    }
    let idx = current_idx();
    if idx >= MAX_TASKS {
        return 0;
    }
    tasks()[idx].pid
}

fn uptime_ms() -> u64 {
    crate::cpu::pit::uptime_ms()
}

// ---------------------------------------------------------------------------
// Task creation
// ---------------------------------------------------------------------------

pub struct NewTask<'a> {
    pub name: &'a str,
    /// Entry RIP.
    pub entry: u64,
    /// Initial RSP (user tasks: user stack top; kernel: managed internally).
    pub user_rsp: Option<u64>,
    /// Physical PML4 (kernel CR3 for kernel tasks).
    pub pml4: u64,
    pub is_user: bool,
    pub user_space: Option<AddressSpace>,
    /// Pin the task to one CPU (CPU_ANY = free migration).
    pub pinned_cpu: u8,
}

fn alloc_kstack() -> Option<(u64, u64)> {
    let phys = frames::alloc_contig(KSTACK_PAGES)?;
    let bottom = crate::mem::paging::phys_to_virt(phys);
    Some((bottom + KSTACK_SIZE as u64, bottom))
}

fn set_name(t: &mut Task, name: &str) {
    let bytes = name.as_bytes();
    let n = bytes.len().min(12);
    t.name[..n].copy_from_slice(&bytes[..n]);
    t.name_len = n;
}

/// Build the fake "suspended in an interrupt" frame that starts a task.
/// Returns the address of the Regs frame on the task's kernel stack.
fn bootstrap_frame(t: &mut Task, entry: u64, user_rsp: Option<u64>) -> u64 {
    let top = t.kstack_top & !0xF;
    let frame = (top - core::mem::size_of::<Regs>() as u64) as *mut Regs;
    unsafe {
        core::ptr::write_bytes(frame as *mut u8, 0, core::mem::size_of::<Regs>());
        let r = &mut *frame;
        r.rip = entry;
        r.rflags = 0x202; // IF=1, reserved bit
        if let Some(ursp) = user_rsp {
            // ring 3 entry
            r.cs = gdt::USER_CODE_RPL3 as u64;
            r.ss = gdt::USER_DATA_RPL3 as u64;
            r.rsp = ursp;
        } else {
            // kernel thread entry: mimic a called function (rsp ≡ 8 mod 16)
            r.cs = gdt::KERNEL_CODE as u64;
            r.ss = gdt::KERNEL_DATA as u64;
            r.rsp = top - 8;
        }
        frame as u64
    }
}

/// Create a task and put it in the run queue. Returns its pid.
pub fn spawn(new: NewTask) -> Option<u64> {
    let _g = SCHED_LOCK.lock();
    let slot = tasks().iter().position(|t| t.state == State::Dead)?;
    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);

    let (ks_top, ks_bottom) = alloc_kstack()?;

    let t = &mut tasks()[slot];
    *t = Task {
        pid,
        name: [0; 12],
        name_len: 0,
        state: State::Ready,
        pinned_cpu: new.pinned_cpu,
        on_cpu: CPU_ANY,
        kstack_top: ks_top,
        kstack_bottom: ks_bottom,
        kstack_owned: true,
        saved_regs: 0,
        pml4: new.pml4,
        user_space: new.user_space,
        parent: current_pid(),
        wait_target: 0,
        exit_code: 0,
        wake_at_ms: 0,
        is_user: new.is_user,
    };
    set_name(t, new.name);
    t.saved_regs = bootstrap_frame(t, new.entry, new.user_rsp);

    klog!(
        "sched: spawned '{}' pid {} ({}) pml4={:#x} kstack={:#x} pin={}",
        t.name_str(),
        pid,
        if new.is_user { "user" } else { "kernel" },
        t.pml4,
        t.kstack_top,
        if new.pinned_cpu == CPU_ANY {
            "any".into()
        } else {
            alloc::format!("{}", new.pinned_cpu)
        }
    );
    Some(pid)
}

/// Public read-only accessor used by the SMP boot path (kstack to switch to).
pub fn task_kstack_top(slot: usize) -> u64 {
    tasks()[slot].kstack_top
}

/// Snapshot (pid, name) of the task in `slot` — introspection only.
pub fn task_brief(slot: usize) -> Option<(u64, &'static str)> {
    let _g = SCHED_LOCK.lock();
    let t = &tasks()[slot];
    if t.state == State::Dead {
        None
    } else {
        Some((t.pid, t.name_str()))
    }
}

/// Spawn the resident idle task for CPU `cpu` (pinned); returns the slot.
/// Called from the BSP before the AP is released.
pub fn spawn_ap_kidle(cpu: usize) -> Option<usize> {
    const KIDLE_NAMES: [&str; gdt::MAX_CPUS] =
        ["kidle", "kidle1", "kidle2", "kidle3", "kidle4", "kidle5", "kidle6", "kidle7"];
    let kernel_cr3 = vmm::kernel_cr3();
    let _ = spawn(NewTask {
        name: KIDLE_NAMES[cpu.min(gdt::MAX_CPUS - 1)],
        entry: kidle_main as *const () as u64,
        user_rsp: None,
        pml4: kernel_cr3,
        is_user: false,
        user_space: None,
        pinned_cpu: cpu as u8,
    })?;
    // find the slot we just filled (kidle names are unique per cpu)
    tasks().iter().position(|t| {
        t.state != State::Dead && t.name_str().starts_with("kidle") && t.pinned_cpu == cpu as u8
    })
}

/// Hand a freshly-started AP its initial task: mark the pinned kidle as
/// Running on that CPU and make it the CPU's current.
pub fn ap_go(cpu: usize, slot: usize) {
    let _g = SCHED_LOCK.lock();
    let t = &mut tasks()[slot];
    t.state = State::Running;
    t.on_cpu = cpu as u8;
    smp::set_current_task(cpu, slot);
}

// ---------------------------------------------------------------------------
// init: adopt the boot context as task 1 (the shell), start kidle + kstat
// ---------------------------------------------------------------------------

extern "C" {
    /// Boot kernel stack top from main.rs `_start`.
    static kstack_top: u8;
}

pub fn init() {
    let kernel_cr3 = vmm::kernel_cr3();

    // Shell = the context that is running RIGHT NOW (boot stack).
    let shell_slot = tasks().iter().position(|t| t.state == State::Dead).unwrap();
    let t = &mut tasks()[shell_slot];
    *t = Task {
        pid: NEXT_PID.fetch_add(1, Ordering::Relaxed), // pid 1
        name: [0; 12],
        name_len: 0,
        state: State::Running,
        pinned_cpu: 0,
        on_cpu: 0,
        kstack_top: unsafe { &kstack_top as *const u8 as u64 },
        kstack_bottom: 0,
        kstack_owned: false,
        saved_regs: 0, // never suspended yet
        pml4: kernel_cr3,
        user_space: None,
        parent: 0,
        wait_target: 0,
        exit_code: 0,
        wake_at_ms: 0,
        is_user: false,
    };
    set_name(t, "glmsh");
    smp::set_current_task(0, shell_slot);

    // kidle: always ready, hlt between yields (BSP resident idle)
    let _ = spawn(NewTask {
        name: "kidle",
        entry: kidle_main as *const () as u64,
        user_rsp: None,
        pml4: kernel_cr3,
        is_user: false,
        user_space: None,
        pinned_cpu: 0,
    });
    // kstat: periodic stats reporter (free to migrate)
    let _ = spawn(NewTask {
        name: "kstat",
        entry: kstat_main as *const () as u64,
        user_rsp: None,
        pml4: kernel_cr3,
        is_user: false,
        user_space: None,
        pinned_cpu: CPU_ANY,
    });

    ON_FLAG.store(true, Ordering::Relaxed);
    klog!(
        "sched: online (shell=pid {} adopted on boot stack, kidle+kstat ready, preempt=ring3 @ {} Hz)",
        tasks()[shell_slot].pid,
        crate::cpu::apic::SCHED_HZ
    );
}

// ---------------------------------------------------------------------------
// Kernel thread bodies
// ---------------------------------------------------------------------------

/// int 0x80 from ring 0 — the uniform way kernel tasks yield/sleep/wait.
#[inline]
pub fn ksyscall(n: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret;
    unsafe {
        asm!(
            "int 0x80",
            inlateout("rax") n => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            options(nostack)
        );
    }
    ret
}

fn kidle_main() -> ! {
    loop {
        // never busy: hlt between yields; if someone else is ready the
        // yield hands the CPU over immediately
        ksyscall(SYS_YIELD, 0, 0, 0);
        unsafe { asm!("hlt", options(nomem, nostack)) };
    }
}

/// Extern-C wrapper: the entry point an AP jumps to on its kidle stack
/// (called from cpu::smp::ap_entry through inline asm).
pub extern "C" fn ap_idle_main() -> ! {
    kidle_main()
}

fn kstat_main() -> ! {
    loop {
        ksyscall(SYS_SLEEP, 5000, 0, 0);
        let alive = tasks().iter().filter(|t| t.state != State::Dead).count();
        let cpus = crate::cpu::smp::online_mask().count_ones();
        crate::console::print_color("[kstat] ", GLM_GRAY);
        crate::console::print_args(format_args!(
            "up {}s | cpus {} | tasks {} | switches {} | frames free {}",
            uptime_ms() / 1000,
            cpus,
            alive,
            switches(),
            {
                let s = frames::stats();
                s.total - s.used
            }
        ));
        crate::console::newline();
    }
}

// ---------------------------------------------------------------------------
// Syscall-side operations (run in the context of the calling task)
// ---------------------------------------------------------------------------

pub fn sys_yield_now() {
    smp::request_switch(smp::cpu_index());
}

pub fn sys_sleep(ms: u64) {
    let _g = SCHED_LOCK.lock();
    let t = &mut tasks()[current_idx()];
    t.wake_at_ms = uptime_ms() + ms;
    t.state = State::Sleeping;
    drop(_g);
    smp::request_switch(smp::cpu_index());
}

/// wait(target): if a matching child already exited, return its code
/// immediately; otherwise park the caller until exit_current wakes it.
pub fn sys_wait(regs: &mut Regs, target: u64) {
    let _g = SCHED_LOCK.lock();
    let me = current_idx();
    let mypid = tasks()[me].pid;
    let mut done: Option<i64> = None;
    for t in tasks().iter() {
        if t.state == State::Zombie && t.parent == mypid {
            if target == 0 || target == t.pid {
                done = Some(t.exit_code);
                break;
            }
        }
    }
    if let Some(code) = done {
        regs.rax = code as u64;
        return;
    }
    let t = &mut tasks()[me];
    t.wait_target = target;
    t.state = State::WaitingChild;
    regs.rax = 0;
    drop(_g);
    smp::request_switch(smp::cpu_index());
}

/// Readchar support: block until a key arrives.
pub fn sys_block_on_input() {
    let _g = SCHED_LOCK.lock();
    let t = &mut tasks()[current_idx()];
    t.state = State::BlockedInput;
    drop(_g);
    smp::request_switch(smp::cpu_index());
}

/// Exit the current task (syscall exit or fatal fault). Never returns
/// to the caller as a running task: marks zombie, wakes a waiting parent,
/// and requests a switch.
pub fn exit_current(code: i64) {
    let cpu = smp::cpu_index();
    let (mypid, frames_reclaimed) = {
        let _g = SCHED_LOCK.lock();
        let me = current_idx();
        let (mypid, myparent) = {
            let t = &tasks()[me];
            (t.pid, t.parent)
        };

        // tear down the user address space while still on it (kernel half is
        // shared, so HHDM access works regardless of CR3)
        let reclaimed = {
            let t = &mut tasks()[me];
            match t.user_space.take() {
                Some(space) => space.destroy(),
                None => 0,
            }
        };

        {
            let t = &mut tasks()[me];
            t.exit_code = code;
            t.state = State::Zombie;
            t.on_cpu = CPU_ANY;
        }

        // wake a waiting parent, deliver the exit code through its frame
        for t in tasks().iter_mut() {
            if t.state == State::WaitingChild
                && t.pid == myparent
                && (t.wait_target == 0 || t.wait_target == mypid)
            {
                if t.saved_regs != 0 {
                    unsafe {
                        (*core::ptr::with_exposed_provenance_mut::<Regs>(t.saved_regs as usize)).rax =
                            code as u64;
                    }
                }
                t.wait_target = 0;
                t.state = State::Ready;
                break;
            }
        }
        (mypid, reclaimed)
    };

    klog!(
        "sched: task {} exited with code {} ({} frames reclaimed)",
        mypid,
        code,
        frames_reclaimed
    );
    smp::request_switch(cpu);
}

/// Kill a task by pid. Refuses running tasks (they live on some CPU).
pub fn kill(pid: u64) -> Result<&'static str, &'static str> {
    if !online() {
        return Err("scheduler offline");
    }
    let me = current_pid();
    if pid == me {
        return Err("cannot kill the running task");
    }
    let _g = SCHED_LOCK.lock();
    for t in tasks().iter_mut() {
        if t.state == State::Dead || t.pid != pid {
            continue;
        }
        match t.state {
            State::Running => return Err("cannot kill a running task"),
            State::Zombie => {
                free_slot(t);
                return Ok("zombie reaped");
            }
            _ => {
                if let Some(space) = t.user_space.take() {
                    let n = space.destroy();
                    klog!("sched: kill {}: {} frames reclaimed", pid, n);
                }
                free_slot(t);
                klog!("sched: task {} killed", pid);
                return Ok("terminated");
            }
        }
    }
    Err("no such task")
}

fn free_slot(t: &mut Task) {
    if t.kstack_owned && t.kstack_bottom != 0 {
        for i in 0..KSTACK_PAGES as u64 {
            frames::free(t.kstack_bottom + i * 4096);
        }
    }
    klog!("sched: reaped task {} ({})", t.pid, t.name_str());
    *t = Task::dead();
}

// ---------------------------------------------------------------------------
// The switching core
// ---------------------------------------------------------------------------

/// Wake duties + preemption decision on each LAPIC timer tick.
/// Caller holds SCHED_LOCK.
fn timer_duties(cpu: usize) {
    let now = uptime_ms();

    // wake sleepers
    for t in tasks().iter_mut() {
        if t.state == State::Sleeping && t.wake_at_ms <= now {
            t.state = State::Ready;
        }
    }

    // deliver a keypress to a blocked task (frame surgery: it is not running)
    for t in tasks().iter_mut() {
        if t.state == State::BlockedInput {
            if let Some(c) = crate::cpu::keyboard::pop() {
                if t.saved_regs != 0 {
                    unsafe {
                        (*core::ptr::with_exposed_provenance_mut::<Regs>(t.saved_regs as usize))
                            .rax = c as u64;
                    }
                }
                t.state = State::Ready;
            }
            break; // one key, one waiter per tick
        }
    }

    // preempt ring-3 tasks (never kernel tasks: no kernel data races)
    let cur = smp::current_task_idx(cpu);
    if cur >= MAX_TASKS {
        return;
    }
    let (from_user, running) = {
        let t = &tasks()[cur];
        (t.is_user, t.state == State::Running)
    };
    if from_user && running {
        tasks()[cur].state = State::Ready;
        smp::request_switch(cpu);
    }
}

/// Pick the next task among Ready ones, EXCLUDING this CPU's current:
/// prefer any non-kidle Ready task; this CPU's pinned kidle is the
/// always-ready fallback. Round-robin by pid relative to current.
/// Tasks pinned to another CPU are invisible here. None = nobody else:
/// the caller keeps running the current task.
fn pick_next(cpu: usize) -> Option<usize> {
    let me = smp::current_task_idx(cpu);
    let cur_pid = tasks()[me].pid;
    let mut best: Option<usize> = None;
    let mut best_idle: Option<usize> = None;

    for (i, t) in tasks().iter().enumerate() {
        if t.state != State::Ready || i == me {
            continue;
        }
        // a task pinned elsewhere can never be picked by this CPU
        if t.pinned_cpu != CPU_ANY && t.pinned_cpu != cpu as u8 {
            continue;
        }
        let is_idle = t.name_str().starts_with("kidle");
        if is_idle {
            best_idle = Some(i);
            continue;
        }
        // round-robin: prefer pid > current pid (lowest such), else lowest
        let better = match best {
            None => true,
            Some(b) => {
                let bp = tasks()[b].pid;
                let (a_after, b_after) = (t.pid > cur_pid, bp > cur_pid);
                if a_after != b_after {
                    a_after
                } else {
                    t.pid < bp
                }
            }
        };
        if better {
            best = Some(i);
        }
    }
    best.or(best_idle)
}

/// Called at the tail of the asm ISR stub path (via common_handler).
/// Returns the Regs frame of the task to resume, or null to resume the
/// interrupted context unchanged. The whole park -> pick -> run sequence
/// is atomic under SCHED_LOCK — two CPUs can never pick the same task.
pub fn post_dispatch(vec: u64, regs: &mut Regs) -> *mut Regs {
    if !online() {
        return core::ptr::null_mut();
    }
    let cpu = smp::cpu_index();
    let _g = SCHED_LOCK.lock();

    // scheduler duties: LAPIC timer normally; PIT (vec 32) as fallback
    // when no LAPIC is present
    if vec == crate::cpu::apic::TIMER_VECTOR as u64
        || (vec == 32 && !crate::cpu::apic::online())
    {
        timer_duties(cpu);
    }

    let want_switch =
        smp::take_switch_request(cpu) || tasks()[smp::current_task_idx(cpu)].state != State::Running;
    if !want_switch {
        return core::ptr::null_mut();
    }

    // park the current context on its own stack
    let me = smp::current_task_idx(cpu);
    {
        let t = &mut tasks()[me];
        if t.state == State::Running {
            t.state = State::Ready;
        }
        t.saved_regs = regs as *mut Regs as u64;
    }

    // reap zombies that are not the current context (their stacks are safe)
    for t in tasks().iter_mut() {
        if t.state == State::Zombie && t.pid != tasks()[me].pid {
            free_slot(t);
        }
    }

    match pick_next(cpu) {
        None => {
            // nobody else is ready: keep running the current context.
            // (Under the lock, so no other CPU could have snatched it.)
            tasks()[me].state = State::Running;
            core::ptr::null_mut()
        }
        Some(next) => {
            // switch bookkeeping: states, this CPU's TSS.RSP0, CR3
            {
                let t = &mut tasks()[next];
                t.state = State::Running;
                t.on_cpu = cpu as u8;
                gdt::set_rsp0(cpu, t.kstack_top);
                if t.pml4 != vmm::cr3() {
                    vmm::load_cr3(t.pml4);
                }
            }
            SWITCHES.fetch_add(1, Ordering::Relaxed);
            smp::count_switch(cpu);

            let frame = tasks()[next].saved_regs as *mut Regs;
            smp::set_current_task(cpu, next);
            frame
        }
    }
}

// ---------------------------------------------------------------------------
// ps / introspection
// ---------------------------------------------------------------------------

pub fn for_each_task(f: impl FnMut(&Task)) {
    let _g = SCHED_LOCK.lock();
    tasks().iter().filter(|t| t.state != State::Dead).for_each(f)
}
