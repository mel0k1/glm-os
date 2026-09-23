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

use alloc::sync::Arc;

use crate::console::GLM_GRAY;
use crate::cpu::gdt;
use crate::cpu::idt::Regs;
use crate::cpu::smp;
use crate::klog;
use crate::mem::frames;
use crate::mem::vmm::{self, AddressSpace};
use crate::sync::Spinlock;
use crate::user::signal::{self, NSIG, SIGKILL, SIGTERM, SIGUSR1, SIGUSR2};

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
/// v0.7: exit code for sibling threads terminated by a process exit.
pub const THREAD_KILLED_CODE: i64 = 143; // 128 + SIGKILL, Linux-style

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
    /// Parked on a full/empty IPC channel (ipc.rs parks via mark_blocked_chan).
    BlockedChan,
    /// v0.9: parked on an empty UDP socket queue (net::sock::recvfrom).
    BlockedSock,
    /// Shell waiting for `wait_target` child to exit.
    WaitingChild,
    /// v0.7: a thread parked in sys_join until a sibling thread exits.
    BlockedJoin,
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
            State::BlockedChan => "CHAN",
            State::BlockedSock => "SOCK",
            State::WaitingChild => "WAIT",
            State::BlockedJoin => "JOIN",
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
    /// Owned user address space (None for kernel tasks). v0.7: shared by
    /// reference between the threads of one process (Arc refcount).
    pub user_space: Option<Arc<AddressSpace>>,
    /// Parent pid (for wait()).
    pub parent: u64,
    /// Who this WaitingChild task waits for (0 = any child).
    pub wait_target: u64,
    /// v0.7: thread group id = pid of the main thread. Every task starts
    /// as a single-thread process (tgid == pid); threads created by
    /// sys_clone inherit the creator's tgid. pid == tgid marks the MAIN
    /// thread, whose exit terminates the whole process.
    pub tgid: u64,
    /// v0.7: thread this BlockedJoin task waits for (0 = any sibling).
    pub join_target: u64,
    /// v0.7: set by a process exit on Running sibling threads; the flag
    /// turns them into zombies at their very next interrupt, on their own
    /// CPU and kernel stack.
    pub die_flag: bool,
    /// v0.7: per-task FS base for user TLS (set via SYS_SET_FS).
    pub fs_base: u64,
    pub exit_code: i64,
    pub wake_at_ms: u64,
    pub is_user: bool,
    /// User-space handler VA per signal number (0 = default action).
    pub sig_handlers: [u64; NSIG],
    /// Bitmask of queued-but-undelivered signals (bit N = signal N).
    pub sig_pending: u64,
    /// VA of the SigFrame being handled right now (0 = none).
    pub sig_frame_va: u64,
    /// Nesting depth of currently-running signal handlers.
    pub sig_depth: u8,
    /// v1.4: output redirection target. 0 = the text console; a non-zero
    /// value is a GUI terminal window id — every console write this task
    /// makes (kernel prints and SYS_WRITE alike) lands in that window
    /// instead. Children inherit it at spawn/fork time, so `run` inside a
    /// terminal window prints into the window that launched it.
    pub out_win: u32,
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
            tgid: 0,
            join_target: 0,
            die_flag: false,
            fs_base: 0,
            exit_code: 0,
            wake_at_ms: 0,
            is_user: false,
            sig_handlers: [0; NSIG],
            sig_pending: 0,
            sig_frame_va: 0,
            sig_depth: 0,
            out_win: 0,
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

/// v0.7: exit-code mailbox. Closes a latent race: post_dispatch lazily
/// reaps zombies, so a child can be fully reaped BEFORE its parent gets
/// around to wait()/join() — which then parks forever. Every zombie
/// destruction records (pid, exit_code) here under SCHED_LOCK; wait/join
/// consult the log after the live-table scan. Oldest entries are
/// overwritten when the ring is full (bounded by MAX_TASKS).
static mut REAP_LOG: [(u64, i64); MAX_TASKS] = [(0, 0i64); MAX_TASKS];

fn reap_log_push(pid: u64, code: i64) {
    let log = unsafe { &mut *core::ptr::addr_of_mut!(REAP_LOG) };
    // overwrite the oldest used entry when full (classic ring)
    let slot = log.iter().position(|e| e.0 == 0).unwrap_or(0);
    log[slot] = (pid, code);
}

fn reap_log_take(pid: u64) -> Option<i64> {
    let log = unsafe { &mut *core::ptr::addr_of_mut!(REAP_LOG) };
    let slot = log.iter().position(|e| e.0 == pid)?;
    let code = log[slot].1;
    log[slot] = (0, 0);
    Some(code)
}

/// v0.7: destroy a zombie slot for good: record the exit code in the
/// mailbox, free its kernel stack. Caller holds SCHED_LOCK. The task must
/// not be running anywhere (true for all Zombie tasks).
fn reap_zombie_locked(slot: usize) {
    let (pid, code) = {
        let t = &tasks()[slot];
        (t.pid, t.exit_code)
    };
    reap_log_push(pid, code);
    free_slot(&mut tasks()[slot]);
}

const MSR_FS_BASE: u32 = 0xC000_0100;

/// Write the FS base MSR (user TLS). Called on every context switch.
#[inline]
fn wrmsr_fs_base(v: u64) {
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") MSR_FS_BASE,
            in("eax") v as u32,
            in("edx") (v >> 32) as u32,
            options(nostack, nomem)
        );
    }
}
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

/// v1.4: the calling task's output-redirect target (0 = text console).
/// Lock-free read of the CURRENT task, same safety argument as
/// current_pid(): a running task cannot be reaped underneath itself.
pub fn current_out_win() -> u32 {
    if !online() {
        return 0;
    }
    let idx = current_idx();
    if idx >= MAX_TASKS {
        return 0;
    }
    tasks()[idx].out_win
}

/// v1.4: redirect the CURRENT task's console output into a terminal
/// window (0 = back to the text console). Only ever called by the task
/// itself, in task context.
pub fn set_current_out_win(win: u32) {
    if !online() {
        return;
    }
    let idx = current_idx();
    if idx < MAX_TASKS {
        tasks()[idx].out_win = win;
    }
}

/// Index of the calling task in the task table (syscall context).
pub fn current_slot() -> usize {
    current_idx()
}

/// Ask for a context switch at the next interrupt (timer/IPI).
pub fn request_switch() {
    smp::request_switch(smp::cpu_index());
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
    pub user_space: Option<Arc<AddressSpace>>,
    /// Pin the task to one CPU (CPU_ANY = free migration).
    pub pinned_cpu: u8,
    /// v1.7: (rdi, rsi) for the entry frame — user tasks carry
    /// (argc, argv) so ring-3 programs receive a Unix-style argv.
    pub entry_regs: (u64, u64),
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
fn bootstrap_frame(t: &mut Task, entry: u64, user_rsp: Option<u64>, entry_regs: (u64, u64)) -> u64 {
    let top = t.kstack_top & !0xF;
    let frame = (top - core::mem::size_of::<Regs>() as u64) as *mut Regs;
    unsafe {
        core::ptr::write_bytes(frame as *mut u8, 0, core::mem::size_of::<Regs>());
        let r = &mut *frame;
        r.rip = entry;
        r.rflags = 0x202; // IF=1, reserved bit
        // v1.7: (rdi, rsi) = (argc, argv) for user tasks; (0, 0) for kernel
        r.rdi = entry_regs.0;
        r.rsi = entry_regs.1;
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
        tgid: pid, // every spawned task starts as a single-thread process
        join_target: 0,
        die_flag: false,
        fs_base: 0,
        exit_code: 0,
        wake_at_ms: 0,
        is_user: new.is_user,
        sig_handlers: [0; NSIG],
        sig_pending: 0,
        sig_frame_va: 0,
        sig_depth: 0,
        // v1.4: children inherit the spawner's output redirection, so a
        // program `run` from a terminal window prints into that window
        out_win: current_out_win(),
    };
    set_name(t, new.name);
    t.saved_regs = bootstrap_frame(t, new.entry, new.user_rsp, new.entry_regs);

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

/// v1.7: replace the CURRENT task's user image (the heart of exec).
///
/// Called from the SYS_EXEC handler while the calling task is suspended
/// in its own syscall frame on its own kernel stack. The replacement
/// image (`new_space` with entry/stack already built) was fully prepared
/// by the caller BEFORE we take the lock, so every failure below leaves
/// the old image intact and drops only the new one.
///
/// Steps, all under SCHED_LOCK:
///   1. refuse inside a thread group (POSIX exec would kill the
///      siblings; we refuse instead — returns Err, image dropped);
///   2. swap user_space Arc + pml4, reset signal state and TLS base
///      (the old TLS block dies with the old address space);
///   3. rename the task (ps shows the new image);
///   4. rewrite the saved ring-3 frame IN PLACE: rip = new entry,
///      rsp = new stack, rdi/rsi = (argc, argv) — the iretq that ends
///      this very interrupt lands inside the new program;
///   5. load CR3 NOW: without this the iretq would return into pages
///      that are not mapped under the old table.
/// After the lock is released: CR3 already points at the new table and
/// no other task holds the old Arc (groups refused), so the old image
/// gets a FULL destroy (frames + tables + PML4 — unlike the self-exit
/// path, nothing can still walk it).
///
/// On success the caller must not touch user memory again: its
/// `AddressSpace::from_pml4(cr3())` view is stale the moment CR3 moves.
pub fn exec_replace_image(
    new_space: AddressSpace,
    entry: u64,
    rsp: u64,
    rdi: u64,
    rsi: u64,
    new_name: &str,
    frame: *mut Regs,
) -> Result<(), &'static str> {
    let new_pml4 = new_space.pml4;
    let _g = SCHED_LOCK.lock();
    let me = current_idx();
    let (mypid, is_user) = {
        let t = &tasks()[me];
        (t.pid, t.is_user)
    };
    if !is_user {
        let _ = new_space.destroy();
        return Err("exec from a kernel task makes no sense");
    }
    // POSIX would take the whole thread group down; we refuse instead.
    for other in tasks().iter() {
        if other.pid != mypid && other.tgid == mypid && other.state != State::Dead {
            let _ = new_space.destroy();
            return Err("exec inside a thread group is not supported");
        }
    }

    let old_space = {
        let t = &mut tasks()[me];
        let old = t.user_space.replace(Arc::new(new_space));
        t.pml4 = new_pml4;
        // POSIX exec: caught handlers reset to default, pending dropped
        t.sig_handlers = [0; NSIG];
        t.sig_pending = 0;
        t.sig_depth = 0;
        t.sig_frame_va = 0;
        // the old TLS block belongs to the old address space
        t.fs_base = 0;
        set_name(t, new_name);
        old
    };

    // frame surgery: the syscall "returns" into the new image
    unsafe {
        let r = &mut *frame;
        r.rip = entry;
        r.rsp = rsp;
        r.rdi = rdi; // argc
        r.rsi = rsi; // argv (pointer to argv[0], NULL-terminated)
        r.rax = 0;
    }

    // switch CR3 while still inside the kernel: the kernel half is shared
    // by every table, so this stack and all kernel code stay mapped
    vmm::load_cr3(new_pml4);
    wrmsr_fs_base(0);

    drop(_g);

    // the old image is now unreferenced AND no live CR3 points at it:
    // full teardown, nothing leaks (contrast with destroy_keep_root)
    if let Some(old) = old_space {
        match Arc::try_unwrap(old) {
            Ok(owned) => {
                let freed = owned.destroy();
                klog!("exec: old image destroyed ({} frames reclaimed)", freed);
            }
            // cannot happen (groups refused, pid survives), but keep the
            // Arc dropped regardless
            Err(_) => {}
        }
    }
    Ok(())
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
        entry_regs: (0, 0),
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
        tgid: 0, // set to the shell pid right below
        join_target: 0,
        die_flag: false,
        fs_base: 0,
        exit_code: 0,
        wake_at_ms: 0,
        is_user: false,
        sig_handlers: [0; NSIG],
        sig_pending: 0,
        sig_frame_va: 0,
        sig_depth: 0,
        out_win: 0,
    };
    set_name(t, "glmsh");
    t.tgid = t.pid;
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
        entry_regs: (0, 0),
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
        entry_regs: (0, 0),
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
        // v1.0: stay off the screen while the GUI owns it (klog keeps
        // the stats alive on serial for the duration)
        if crate::console::GUI_ACTIVE.load(core::sync::atomic::Ordering::Relaxed) {
            klog!(
                "[kstat] up {}s | tasks alive (gui active, console deferred)",
                uptime_ms() / 1000
            );
            continue;
        }
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

/// v0.6: fork() — copy-on-write duplicate of the calling user task.
///
/// The child gets:
///   * a COW clone of the parent's address space (fork_cow: every user
///     page shared, writable ones downgraded to read-only until a write
///     faults them into private copies);
///   * a fresh kernel stack holding a COPY of the caller's current
///     interrupt frame, with rax = 0 (fork() returns 0 in the child);
///   * inherited signal handler table; pending signals do NOT carry over.
///
/// The parent keeps running right away and finds the child's pid in rax.
/// Returns the pid (>= 0) or -1 when resources ran out.
pub fn sys_fork(regs: &mut Regs) -> i64 {
    if !online() {
        return -1;
    }

    // only user tasks may fork: kernel tasks share the kernel CR3, there
    // is no user half to clone
    let (my_slot, am_user) = {
        let _g = SCHED_LOCK.lock();
        let me = current_idx();
        (me, tasks()[me].is_user)
    };
    if !am_user {
        return -1;
    }

    // 1) clone the address space (pure VMM work: no scheduler locks held).
    //    Marks pages COW + refcounts frames; invlpgs the parent's pages.
    let parent_space = AddressSpace::from_pml4(vmm::cr3());
    let Some(child_space) = parent_space.fork_cow() else {
        return -1;
    };

    // 2) broadcast a TLB shootdown so no other CPU keeps stale writable
    //    translations of pages fork_cow just downgraded
    crate::mem::tlb::shootdown_all_others();

    // 3) register the child in the task table
    let _g = SCHED_LOCK.lock();
    let Some(slot) = tasks().iter().position(|t| t.state == State::Dead) else {
        drop(_g);
        child_space.destroy();
        return -1;
    };
    let Some((ks_top, ks_bottom)) = alloc_kstack() else {
        drop(_g);
        child_space.destroy();
        return -1;
    };
    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    let child_space = Arc::new(child_space);

    // copy the parent's CURRENT ring-3 frame onto the child's fresh stack
    let child_frame = ((ks_top & !0xF) - core::mem::size_of::<Regs>() as u64) as *mut Regs;
    unsafe {
        core::ptr::copy_nonoverlapping(regs as *const Regs, child_frame, 1);
        (*child_frame).rax = 0; // fork() == 0 in the child
    }

    let (name_bytes, name_len, parent_pid, handlers) = {
        let t = &tasks()[my_slot];
        (t.name, t.name_len, t.pid, t.sig_handlers)
    };
    {
        let t = &mut tasks()[slot];
        *t = Task {
            pid,
            name: name_bytes,
            name_len,
            state: State::Ready,
            pinned_cpu: CPU_ANY,
            on_cpu: CPU_ANY,
            kstack_top: ks_top,
            kstack_bottom: ks_bottom,
            kstack_owned: true,
            saved_regs: child_frame as u64,
            pml4: child_space.pml4,
            user_space: Some(child_space),
            parent: parent_pid,
            wait_target: 0,
            tgid: pid, // a fork() child is always a NEW single-thread process
            join_target: 0,
            die_flag: false,
            fs_base: 0, // TLS is not inherited across fork
            exit_code: 0,
            wake_at_ms: 0,
            is_user: true,
            sig_handlers: handlers,
            sig_pending: 0, // pending signals do not cross fork()
            sig_frame_va: 0,
            sig_depth: 0,
            // v1.4: a fork() child keeps printing where its parent prints
            out_win: current_out_win(),
        };
    }

    klog!(
        "sched: fork: pid {} -> child pid {} (cow clone of pml4 {:#x})",
        parent_pid,
        pid,
        tasks()[slot].pml4
    );
    pid as i64
}

/// wait(target): if a matching child PROCESS already exited, return its
/// code immediately; otherwise park the caller until exit_current wakes it.
/// v0.7: only real processes match here (pid == tgid); thread exits are
/// taken by sys_join. The REAP_LOG mailbox covers zombies that the lazy
/// reaper destroyed before we got here.
pub fn sys_wait(regs: &mut Regs, target: u64) {
    let _g = SCHED_LOCK.lock();
    let me = current_idx();
    let mypid = tasks()[me].pid;
    let mut done: Option<i64> = None;
    for t in tasks().iter() {
        if t.state == State::Zombie && t.parent == mypid && t.pid == t.tgid {
            if target == 0 || target == t.pid {
                done = Some(t.exit_code);
                break;
            }
        }
    }
    if done.is_none() && target != 0 {
        done = reap_log_take(target);
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

/// Turn `slot` into a zombie: drop its reference to the (possibly shared)
/// user space, record the exit code, wake a waiting parent (processes only)
/// or a joining sibling thread with the code injected into its parked
/// frame. Caller holds SCHED_LOCK. Returns reclaimed frame count.
///
/// v0.7 Arc semantics: the address space dies only when the LAST task of
/// the process drops its reference (Arc::try_unwrap succeeds). Threads
/// sharing the space keep it alive for their siblings.
fn finish_zombie_locked(slot: usize, code: i64) -> u64 {
    let (mypid, myparent, am_process) = {
        let t = &tasks()[slot];
        (t.pid, t.parent, t.pid == t.tgid)
    };

    // drop this task's reference; destroy only if it was the last one.
    // destroy_keep_root: this may be the SELF-exit path where the dying
    // task's own CR3 still points at the PML4 — leak the root frame
    // instead of freeing it under its own feet (4 KiB, bounded).
    let reclaimed = {
        let t = &mut tasks()[slot];
        match t.user_space.take() {
            Some(space) => match Arc::try_unwrap(space) {
                Ok(owned) => owned.destroy_keep_root(),
                Err(_) => 0, // siblings still map this space
            },
            None => 0,
        }
    };

    {
        let t = &mut tasks()[slot];
        t.exit_code = code;
        t.state = State::Zombie;
        t.on_cpu = CPU_ANY;
    }

    if am_process {
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
    } else {
        // v0.7: wake a joining sibling thread with the exit code
        for t in tasks().iter_mut() {
            if t.state == State::BlockedJoin
                && t.tgid == tasks()[slot].tgid
                && (t.join_target == mypid || t.join_target == 0)
            {
                if t.saved_regs != 0 {
                    unsafe {
                        (*core::ptr::with_exposed_provenance_mut::<Regs>(t.saved_regs as usize)).rax =
                            code as u64;
                    }
                }
                t.join_target = 0;
                t.state = State::Ready;
                break;
            }
        }
    }
    reclaimed
}

/// Exit the current PROCESS (syscall exit or fatal fault). Never returns
/// to the caller as a running task: v0.7 first terminates every sibling
/// thread of the process (blocked ones directly, running ones via
/// die_flag at their next interrupt), then marks itself a zombie, wakes a
/// waiting parent, and requests a switch.
pub fn exit_current(code: i64) {
    let cpu = smp::cpu_index();
    // v1.6: pids of thread-group members finished during this sweep (their
    // open files are closed AFTER the lock is released -- same lock-order
    // rule as the GUI exit hook)
    let mut swept: alloc::vec::Vec<u64> = alloc::vec![];
    let (mypid, frames_reclaimed) = {
        let _g = SCHED_LOCK.lock();
        let me = current_idx();
        let (my_tgid, is_user) = {
            let t = &tasks()[me];
            (t.tgid, t.is_user)
        };
        // v0.7: a process exit takes its whole thread group with it
        if is_user {
            for i in 0..MAX_TASKS {
                if i == me {
                    continue;
                }
                let t = &mut tasks()[i];
                if t.state == State::Dead
                    || t.state == State::Zombie
                    || !t.is_user
                    || t.tgid != my_tgid
                {
                    continue;
                }
                if t.state == State::Running {
                    // on another CPU: mark for death; its own next
                    // interrupt finishes it on its own kernel stack
                    t.die_flag = true;
                    klog!(
                        "sched: process {} exit: running thread {} marked for termination",
                        my_tgid, t.pid
                    );
                } else {
                    // parked anywhere: safe to finish right now
                    let pid = t.pid;
                    let _ = finish_zombie_locked(i, THREAD_KILLED_CODE);
                    swept.push(pid);
                    klog!(
                        "sched: process {} exit: thread {} terminated",
                        my_tgid, pid
                    );
                }
            }
        }
        let reclaimed = finish_zombie_locked(me, code);
        (tasks()[me].pid, reclaimed)
    };

    klog!(
        "sched: task {} exited with code {} ({} frames reclaimed)",
        mypid,
        code,
        frames_reclaimed
    );
    // v1.2: a dead ring-3 task takes its GUI windows with it (SCHED_LOCK
    // already released -- lock order forbids SCHED_LOCK -> GUI_LOCK)
    crate::gui::on_task_exit(mypid);
    // v1.6: ...and its open files (close + flush; swept threads too)
    for pid in &swept {
        crate::fs::sysfile::on_task_exit(*pid);
    }
    crate::fs::sysfile::on_task_exit(mypid);
    smp::request_switch(cpu);
}

/// v0.7: exit the current THREAD only (pthread_exit-style). The process
/// (shared address space, sibling threads) keeps running. The main thread
/// calling this is redirected to a full process exit.
pub fn sys_texit_current(code: i64) {
    let (me, my_tgid) = {
        let _g = SCHED_LOCK.lock();
        let me = current_idx();
        (me, tasks()[me].tgid)
    };
    if tasks()[me].pid == my_tgid {
        // main thread: exiting it means exiting the process
        exit_current(code);
        return;
    }
    let cpu = smp::cpu_index();
    let (tid, freed) = {
        let _g = SCHED_LOCK.lock();
        let tid = tasks()[me].pid;
        let freed = finish_zombie_locked(me, code);
        (tid, freed)
    };
    klog!(
        "sched: thread {} exited with code {} ({} frames reclaimed, process {} continues)",
        tid,
        code,
        freed,
        my_tgid
    );
    // v1.6: a dying thread takes its own open files with it (SCHED_LOCK
    // released -- same lock-order rule as in exit_current)
    crate::fs::sysfile::on_task_exit(tid);
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
    for i in 0..MAX_TASKS {
        let t = &mut tasks()[i];
        if t.state == State::Dead || t.pid != pid {
            continue;
        }
        match t.state {
            State::Running => return Err("cannot kill a running task"),
            State::Zombie => {
                reap_zombie_locked(i);
                return Ok("zombie reaped");
            }
            _ => {
                // drop this task's space reference (shared spaces survive
                // for their remaining threads); if it was the last
                // reference the whole space is torn down
                let reclaimed = {
                    let t = &mut tasks()[i];
                    match t.user_space.take() {
                        Some(space) => match alloc::sync::Arc::try_unwrap(space) {
                            Ok(owned) => owned.destroy(),
                            Err(_) => 0,
                        },
                        None => 0,
                    }
                };
                if reclaimed > 0 {
                    klog!("sched: kill {}: {} frames reclaimed", pid, reclaimed);
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
// v0.7: threads — clone / thread-exit / join / TLS
// ---------------------------------------------------------------------------

const USER_HALF_LIMIT: u64 = 0x0000_8000_0000_0000;

/// v0.7: clone(entry=rdi, stack=rsi, arg=rdx) — create a THREAD in the
/// caller's process: same address space (shared Arc), same signal
/// handlers, fresh kernel stack, fresh tid; the child starts at `entry`
/// with rdi = arg on the caller-supplied user stack. Returns the child's
/// tid, or -1 when resources ran out.
pub fn sys_clone(regs: &mut Regs) -> i64 {
    if !online() {
        return -1;
    }
    let entry = regs.rdi;
    let stack = regs.rsi;
    let arg = regs.rdx;

    // basic sanity: both must be canonical user-half addresses
    if entry == 0 || stack == 0 || entry >= USER_HALF_LIMIT || stack >= USER_HALF_LIMIT {
        klog!("sched: clone: rejected entry={:#x} stack={:#x}", entry, stack);
        return -1;
    }

    let _g = SCHED_LOCK.lock();
    let me = current_idx();
    let my_tgid = tasks()[me].tgid;
    if !tasks()[me].is_user {
        return -1;
    }
    let Some(slot) = tasks().iter().position(|t| t.state == State::Dead) else {
        return -1;
    };
    let Some((ks_top, ks_bottom)) = alloc_kstack() else {
        return -1;
    };
    let Some(space) = tasks()[me].user_space.clone() else {
        return -1;
    };
    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);

    // fresh ring-3 frame: the child lands at `entry` (rdi = arg) on its
    // own user stack; everything else is zeroed
    let frame = ((ks_top & !0xF) - core::mem::size_of::<Regs>() as u64) as *mut Regs;
    unsafe {
        core::ptr::write_bytes(frame as *mut u8, 0, core::mem::size_of::<Regs>());
        let r = &mut *frame;
        r.rip = entry;
        r.rflags = 0x202;
        r.rsp = stack;
        r.cs = gdt::USER_CODE_RPL3 as u64;
        r.ss = gdt::USER_DATA_RPL3 as u64;
        r.rdi = arg;
    }

    let (name_bytes, name_len, handlers) = {
        let t = &tasks()[me];
        (t.name, t.name_len, t.sig_handlers)
    };
    {
        let t = &mut tasks()[slot];
        *t = Task {
            pid,
            name: name_bytes,
            name_len,
            state: State::Ready,
            pinned_cpu: CPU_ANY,
            on_cpu: CPU_ANY,
            kstack_top: ks_top,
            kstack_bottom: ks_bottom,
            kstack_owned: true,
            saved_regs: frame as u64,
            pml4: space.pml4, // SAME page tables as the creator
            user_space: Some(space),
            parent: my_tgid,
            wait_target: 0,
            tgid: my_tgid,
            join_target: 0,
            die_flag: false,
            fs_base: 0, // each thread installs its own TLS via set_fs
            exit_code: 0,
            wake_at_ms: 0,
            is_user: true,
            sig_handlers: handlers,
            sig_pending: 0,
            sig_frame_va: 0,
            sig_depth: 0,
            out_win: current_out_win(), // threads share the process output
        };
    }

    klog!(
        "sched: clone: thread pid {} of process {} (shared pml4 {:#x}, kstack {:#x})",
        pid,
        my_tgid,
        tasks()[slot].pml4,
        ks_top
    );
    pid as i64
}

/// v0.7: join(tid=rdi) — wait until the sibling thread `tid` exits and
/// return its exit code. target 0 joins ANY sibling. Bad/unknown tids
/// (already joined, not a thread of this process) return -1. Parking is
/// atomic with the scan under SCHED_LOCK, so an exit cannot be missed.
pub fn sys_join(regs: &mut Regs, target: u64) {
    let _g = SCHED_LOCK.lock();
    let me = current_idx();
    if !tasks()[me].is_user {
        regs.rax = (-1i64) as u64;
        return;
    }
    let (my_tgid, my_pid) = {
        let t = &tasks()[me];
        (t.tgid, t.pid)
    };

    // already-exited sibling not yet reaped?
    for i in 0..MAX_TASKS {
        let t = &tasks()[i];
        if t.state == State::Zombie && t.tgid == my_tgid && t.pid != my_pid
            && (target == 0 || target == t.pid)
        {
            let code = t.exit_code;
            reap_zombie_locked(i);
            regs.rax = code as u64;
            return;
        }
    }
    // already reaped (lazy reaper got it first)?
    if target != 0 {
        if let Some(code) = reap_log_take(target) {
            regs.rax = code as u64;
            return;
        }
    }
    // is there a live thread matching the request?
    let mut alive = false;
    for t in tasks().iter() {
        if t.state != State::Dead
            && t.state != State::Zombie
            && t.is_user
            && t.tgid == my_tgid
            && t.pid != my_pid
            && (target == 0 || target == t.pid)
        {
            alive = true;
            break;
        }
    }
    if !alive {
        regs.rax = (-1i64) as u64;
        return;
    }
    let t = &mut tasks()[me];
    t.join_target = target;
    t.state = State::BlockedJoin;
    regs.rax = 0;
    drop(_g);
    smp::request_switch(smp::cpu_index());
}

/// v0.7: set the calling task's FS base (user TLS). Returns 0, or -1 for
/// kernel tasks / non-canonical values.
pub fn sys_set_fs_current(v: u64) -> i64 {
    if v >= USER_HALF_LIMIT {
        return -1;
    }
    let _g = SCHED_LOCK.lock();
    let me = current_idx();
    if !tasks()[me].is_user {
        return -1;
    }
    tasks()[me].fs_base = v;
    wrmsr_fs_base(v);
    0
}

// ---------------------------------------------------------------------------
// Signals + channel parking (v0.5)
// ---------------------------------------------------------------------------

/// Queue a signal for a user task. Delivered when the task is next
/// scheduled in (see deliver_pending_locked). SIGKILL terminates even if
/// a handler is installed.
pub fn send_signal(pid: u64, sig: u64) -> Result<&'static str, &'static str> {
    if !online() {
        return Err("scheduler offline");
    }
    if sig >= NSIG as u64 {
        return Err("bad signal number");
    }
    if pid == current_pid() {
        return Err("cannot signal the calling task");
    }
    let _g = SCHED_LOCK.lock();
    for t in tasks().iter_mut() {
        if t.state == State::Dead || t.pid != pid {
            continue;
        }
        if !t.is_user {
            return Err("not a user task");
        }
        if t.state == State::Zombie {
            return Err("task is a zombie");
        }
        t.sig_pending |= 1 << sig;
        // v0.9: a parked task never resumes on its own, so a queued signal
        // would sit undelivered forever. Wake parked waiters now; delivery
        // happens in post_dispatch on the next schedule (frame surgery for
        // handled signals, default action otherwise). The woken primitive
        // (chan/sock/wait/join/input) re-checks its condition on resume,
        // so a spurious-looking wake is harmless — POSIX EINTR semantics.
        match t.state {
            State::BlockedInput
            | State::BlockedChan
            | State::BlockedSock
            | State::WaitingChild
            | State::BlockedJoin => t.state = State::Ready,
            _ => {}
        }
        return Ok("signal queued (delivers on resume)");
    }
    Err("no such task")
}

/// (state, is_user) of a task by pid — the shell consults this to decide
/// between signal delivery, zombie reaping and the direct kill path.
pub fn task_state(pid: u64) -> Option<(State, bool)> {
    let _g = SCHED_LOCK.lock();
    tasks()
        .iter()
        .find(|t| t.state != State::Dead && t.pid == pid)
        .map(|t| (t.state, t.is_user))
}

/// sigaction syscall body: install a handler for the CURRENT task.
/// Returns the previous handler (0 = was default), or -1 on error.
pub fn sig_set_handler_current(sig: u64, handler: u64) -> i64 {
    if !signal::catchable(sig) {
        return -1;
    }
    let _g = SCHED_LOCK.lock();
    let t = &mut tasks()[current_idx()];
    if !t.is_user {
        return -1;
    }
    let old = t.sig_handlers[sig as usize];
    t.sig_handlers[sig as usize] = handler;
    old as i64
}

/// (pending SigFrame VA, nesting depth) of a task — used by sigreturn.
pub fn sig_frame_of(slot: usize) -> (u64, u8) {
    let _g = SCHED_LOCK.lock();
    let t = &tasks()[slot];
    (t.sig_frame_va, t.sig_depth)
}

/// Consume the current SigFrame (sigreturn succeeded).
pub fn sig_frame_consumed(slot: usize) {
    let _g = SCHED_LOCK.lock();
    let t = &mut tasks()[slot];
    if t.sig_frame_va != 0 {
        t.sig_frame_va = 0;
        t.sig_depth = t.sig_depth.saturating_sub(1);
    }
}

/// Mark the current task as parked on a channel. MUST be called while the
/// caller holds IPC_LOCK (see ipc.rs): the state flip is what makes a peer
/// wakeup un-loseable. The switch itself happens at the next interrupt.
pub fn mark_blocked_chan() {
    let _g = SCHED_LOCK.lock();
    tasks()[current_idx()].state = State::BlockedChan;
}

/// Wake a channel waiter (slot index stored +1). Called from ipc.rs while
/// it holds IPC_LOCK — taking SCHED_LOCK here respects IPC -> SCHED order.
pub fn wake_chan_waiter(slot_plus_one: u16) {
    if slot_plus_one == 0 {
        return;
    }
    let slot = slot_plus_one as usize - 1;
    if slot >= MAX_TASKS {
        return;
    }
    let _g = SCHED_LOCK.lock();
    let t = &mut tasks()[slot];
    if t.state == State::BlockedChan {
        t.state = State::Ready;
    }
}

/// Mark the current task as parked on a UDP socket queue. Same contract
/// as mark_blocked_chan (net::sock parks while holding SOCK_LOCK).
pub fn mark_blocked_sock() {
    let _g = SCHED_LOCK.lock();
    tasks()[current_idx()].state = State::BlockedSock;
}

/// Wake a socket waiter (slot index stored +1). Same contract as
/// wake_chan_waiter; SOCK_LOCK -> SCHED_LOCK mirrors IPC -> SCHED.
pub fn wake_sock_waiter(slot_plus_one: u16) {
    if slot_plus_one == 0 {
        return;
    }
    let slot = slot_plus_one as usize - 1;
    if slot >= MAX_TASKS {
        return;
    }
    let _g = SCHED_LOCK.lock();
    let t = &mut tasks()[slot];
    if t.state == State::BlockedSock {
        t.state = State::Ready;
    }
}

/// Outcome of a delivery attempt at resume time.
enum Delivery {
    Nothing,
    Handled, // frame rewritten: the task enters its handler on resume
    Terminated, // default action / SIGKILL: the task is a zombie now
}

/// Deliver one pending signal to `slot` (must not be running anywhere).
/// Caller holds SCHED_LOCK. Only user tasks are signalled.
fn deliver_pending_locked(slot: usize) -> Delivery {
    let (is_user, pending, has_space) = {
        let t = &tasks()[slot];
        (t.is_user, t.sig_pending, t.user_space.is_some())
    };
    if !is_user || pending == 0 || !has_space {
        return Delivery::Nothing;
    }
    let sig = pending.trailing_zeros() as u64; // lowest pending first
    let handler = tasks()[slot].sig_handlers[sig as usize];

    if sig == SIGKILL || handler == 0 || tasks()[slot].sig_depth >= 4 {
        let code = if sig == SIGKILL { 128 + SIGKILL as i64 } else { 128 + sig as i64 };
        let pid = tasks()[slot].pid;
        let freed = finish_zombie_locked(slot, code);
        klog!(
            "sched: task {} terminated by signal {} ({} frames reclaimed)",
            pid,
            sig,
            freed
        );
        crate::console::print_color("  [ ", GLM_GRAY);
        crate::console::print_color("sig", crate::console::GLM_RED);
        crate::console::print_color(" ] ", GLM_GRAY);
        crate::console::print_args(format_args!(
            "task {} terminated by signal {}\n",
            pid, sig
        ));
        return Delivery::Terminated;
    }

    // handler installed: frame surgery on the parked ring-3 frame
    let t = &mut tasks()[slot];
    match signal::enter_handler(t, sig, handler) {
        Ok(()) => {
            t.sig_pending &= !(1 << sig);
            Delivery::Handled
        }
        Err(()) => {
            // user stack unreachable: fall back to termination (128+11 = SIGSEGV)
            let pid = t.pid;
            let freed = finish_zombie_locked(slot, 139);
            klog!(
                "sched: task {}: signal {} delivery failed, killed ({} frames)",
                pid,
                sig,
                freed
            );
            Delivery::Terminated
        }
    }
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
    let me = smp::current_task_idx(cpu);

    // scheduler duties: LAPIC timer normally; PIT (vec 32) as fallback
    // when no LAPIC is present
    if vec == crate::cpu::apic::TIMER_VECTOR as u64
        || (vec == 32 && !crate::cpu::apic::online())
    {
        timer_duties(cpu);
    }

    // v0.7: a process exit marked this task for death. Finish it right
    // here, on its own CPU and kernel stack, at the first interrupt after
    // the flag was set (covers ring-3 preemption AND mid-syscall tasks).
    if tasks()[me].die_flag && tasks()[me].state == State::Running {
        let pid = tasks()[me].pid;
        let freed = finish_zombie_locked(me, THREAD_KILLED_CODE);
        klog!(
            "sched: thread {} terminated by process exit ({} frames reclaimed)",
            pid, freed
        );
    }

    let want_switch =
        smp::take_switch_request(cpu) || tasks()[smp::current_task_idx(cpu)].state != State::Running;
    if !want_switch {
        return core::ptr::null_mut();
    }

    // park the current context on its own stack
    {
        let t = &mut tasks()[me];
        if t.state == State::Running {
            t.state = State::Ready;
        }
        t.saved_regs = regs as *mut Regs as u64;
    }

    // reap zombies that are not the current context (their stacks are safe)
    for i in 0..MAX_TASKS {
        if tasks()[i].state == State::Zombie && tasks()[i].pid != tasks()[me].pid {
            reap_zombie_locked(i);
        }
    }

    // pick the next task, delivering any queued signals to it first:
    // a task terminated by a default-action signal is skipped (it is a
    // zombie now and gets reaped on the next pass).
    let chosen = loop {
        let cand = pick_next(cpu);
        match cand {
            None => break None,
            Some(n) => match deliver_pending_locked(n) {
                Delivery::Terminated => continue,
                _ => break Some(n),
            },
        }
    };

    match chosen {
        None => {
            // nobody else is ready: keep running the current context.
            // (Under the lock, so no other CPU could have snatched it.)
            // NOTE: a task parked on a channel (BlockedChan) never lands
            // here in practice — its CPU's kidle is always Ready, so the
            // switch below happens and the parked task stays asleep.
            tasks()[me].state = State::Running;
            core::ptr::null_mut()
        }
        Some(next) => {
            // switch bookkeeping: states, this CPU's TSS.RSP0, CR3, TLS
            {
                let t = &mut tasks()[next];
                t.state = State::Running;
                t.on_cpu = cpu as u8;
                gdt::set_rsp0(cpu, t.kstack_top);
                if t.pml4 != vmm::cr3() {
                    vmm::load_cr3(t.pml4);
                }
                // v0.7: FS base always follows the task (user TLS; kernel
                // tasks carry 0). Unconditional: cheaper than tracking.
                wrmsr_fs_base(t.fs_base);
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
