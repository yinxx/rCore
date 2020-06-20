use super::{
    abi::{self, ProcInitInfo},
    add_to_process_table, Pid, Process, PROCESSORS,
};
use crate::arch::interrupt::TrapFrame;
use crate::arch::{
    cpu,
    memory::{get_page_fault_addr, set_page_table},
    paging::*,
};
use crate::fs::{FileHandle, FileLike, OpenOptions, FOLLOW_MAX_DEPTH};
use crate::ipc::SemProc;
use crate::memory::{
    phys_to_virt, ByFrame, Delay, File, GlobalFrameAlloc, KernelStack, MemoryAttr, MemorySet, Read,
};
use crate::process::structs::ElfExt;
use crate::sync::{Condvar, EventBus, SpinLock, SpinNoIrqLock as Mutex};
use crate::{
    signal::{Siginfo, Signal, SignalAction, SignalStack, Sigset},
    syscall::handle_syscall,
};
use alloc::{
    boxed::Box, collections::BTreeMap, collections::VecDeque, string::String, sync::Arc,
    sync::Weak, vec::Vec,
};
use apic::{LocalApic, XApic, LAPIC_ADDR};
use bitflags::_core::cell::Ref;
use core::fmt;
use core::str;
use core::{
    future::Future,
    mem::MaybeUninit,
    pin::Pin,
    task::{Context, Poll},
};
use log::*;
use pc_keyboard::KeyCode::BackTick;
use rcore_fs::vfs::INode;
use rcore_memory::{Page, PAGE_SIZE};
use spin::RwLock;
use trapframe::UserContext;
use xmas_elf::{
    header,
    program::{Flags, SegmentData, Type},
    ElfFile,
};

/// Tid type
pub type Tid = usize;

/// Mutable part of a thread struct
#[derive(Default)]
pub struct ThreadInner {
    /// user context
    /// None when thread is running in user
    context: Option<Box<UserContext>>,
    /// Kernel performs futex wake when thread exits.
    /// Ref: [http://man7.org/linux/man-pages/man2/set_tid_address.2.html]
    pub clear_child_tid: usize,
}

#[allow(dead_code)]
pub struct Thread {
    /// Mutable part
    pub inner: Mutex<ThreadInner>,
    /// This is same as `proc.vm`, avoid extra locking
    pub vm: Arc<Mutex<MemorySet>>,
    /// The process that this thread belongs to
    pub proc: Arc<Mutex<Process>>,
    /// Thread id
    pub tid: Tid,
    /// Signal mask
    pub sig_mask: Sigset,
}

lazy_static! {
    /// Records the mapping between pid and Process struct.
    pub static ref THREADS: RwLock<BTreeMap<usize, Arc<Thread>>> =
        RwLock::new(BTreeMap::new());
}

impl Thread {
    /// Assign a tid and put itself to global thread table.
    pub fn add_to_table(mut self) -> Arc<Self> {
        let mut thread_table = THREADS.write();

        // assign tid, do not start from 0
        let tid = (Pid::INIT..)
            .find(|i| thread_table.get(i).is_none())
            .unwrap();
        self.tid = tid;

        // put to thread table
        let self_ref = Arc::new(self);
        thread_table.insert(tid, self_ref.clone());

        self_ref
    }

    /// Construct virtual memory of a new user process from ELF at `inode`.
    /// Return `(MemorySet, entry_point, ustack_top)`
    pub fn new_user_vm(
        inode: &Arc<dyn INode>,
        args: Vec<String>,
        envs: Vec<String>,
        vm: &mut MemorySet,
    ) -> Result<(usize, usize), &'static str> {
        // Read ELF header
        // 0x3c0: magic number from ld-musl.so
        let mut data = [0u8; 0x3c0];
        inode
            .read_at(0, &mut data)
            .map_err(|_| "failed to read from INode")?;

        // Parse ELF
        let elf = ElfFile::new(&data)?;

        // Check ELF type
        match elf.header.pt2.type_().as_type() {
            header::Type::Executable => {}
            header::Type::SharedObject => {}
            _ => return Err("ELF is not executable or shared object"),
        }

        // Check ELF arch
        match elf.header.pt2.machine().as_machine() {
            #[cfg(target_arch = "x86_64")]
            header::Machine::X86_64 => {}
            #[cfg(target_arch = "aarch64")]
            header::Machine::AArch64 => {}
            #[cfg(any(target_arch = "riscv32", target_arch = "riscv64"))]
            header::Machine::Other(243) => {}
            #[cfg(target_arch = "mips")]
            header::Machine::Mips => {}
            _ => return Err("invalid ELF arch"),
        }

        // auxiliary vector
        let mut auxv = {
            let mut map = BTreeMap::new();
            if let Some(phdr_vaddr) = elf.get_phdr_vaddr() {
                map.insert(abi::AT_PHDR, phdr_vaddr as usize);
            }
            map.insert(abi::AT_PHENT, elf.header.pt2.ph_entry_size() as usize);
            map.insert(abi::AT_PHNUM, elf.header.pt2.ph_count() as usize);
            map.insert(abi::AT_PAGESZ, PAGE_SIZE);
            map
        };

        // entry point
        let mut entry_addr = elf.header.pt2.entry_point() as usize;
        // Make page table
        vm.clear();
        let bias = elf.make_memory_set(vm, inode);

        // Check interpreter (for dynamic link)
        // When interpreter is used, map both dynamic linker and executable
        if let Ok(loader_path) = elf.get_interpreter() {
            info!("Handling interpreter... offset={:x}", bias);
            // assuming absolute path
            let interp_inode = crate::fs::ROOT_INODE
                .lookup_follow(loader_path, FOLLOW_MAX_DEPTH)
                .map_err(|_| "interpreter not found")?;
            // load loader by bias and set aux vector.
            let mut interp_data: [u8; 0x3c0] = unsafe { MaybeUninit::zeroed().assume_init() };
            interp_inode
                .read_at(0, &mut interp_data)
                .map_err(|_| "failed to read from INode")?;
            let elf_interp = ElfFile::new(&interp_data)?;
            elf_interp.append_as_interpreter(&interp_inode, vm, bias);

            // update auxiliary vector
            auxv.insert(abi::AT_ENTRY, elf.header.pt2.entry_point() as usize);
            auxv.insert(abi::AT_BASE, bias);

            // use interpreter as actual entry point
            debug!("entry point: {:x}", elf.header.pt2.entry_point() as usize);
            entry_addr = elf_interp.header.pt2.entry_point() as usize + bias;
        }

        // User stack
        use crate::consts::{USER_STACK_OFFSET, USER_STACK_SIZE};
        let mut ustack_top = {
            let ustack_buttom = USER_STACK_OFFSET;
            let ustack_top = USER_STACK_OFFSET + USER_STACK_SIZE;

            // user stack except top 4 pages
            vm.push(
                ustack_buttom,
                ustack_top - PAGE_SIZE * 4,
                MemoryAttr::default().user().execute(),
                Delay::new(GlobalFrameAlloc),
                "user_stack_delay",
            );

            // We are going to write init info now. So map the last 4 pages eagerly.
            vm.push(
                ustack_top - PAGE_SIZE * 4,
                ustack_top,
                MemoryAttr::default().user().execute(), // feature
                ByFrame::new(GlobalFrameAlloc),
                "user_stack",
            );
            ustack_top
        };

        // Make init info
        let init_info = ProcInitInfo { args, envs, auxv };
        unsafe {
            vm.with(|| ustack_top = init_info.push_at(ustack_top));
        }

        Ok((entry_addr, ustack_top))
    }

    /// Make a new user process from ELF `data`
    pub fn new_user(
        inode: &Arc<dyn INode>,
        exec_path: &str,
        args: Vec<String>,
        envs: Vec<String>,
    ) -> Arc<Thread> {
        /// get virtual memory info
        let mut vm = MemorySet::new();
        let (entry_addr, ustack_top) = Self::new_user_vm(inode, args, envs, &mut vm).unwrap();

        let vm_token = vm.token();
        let vm = Arc::new(Mutex::new(vm));

        // initial fds
        let mut files = BTreeMap::new();
        files.insert(
            0,
            FileLike::File(FileHandle::new(
                crate::fs::TTY.clone(),
                OpenOptions {
                    read: true,
                    write: false,
                    append: false,
                    nonblock: false,
                },
                String::from("/dev/tty"),
                false,
                false,
            )),
        );
        files.insert(
            1,
            FileLike::File(FileHandle::new(
                crate::fs::TTY.clone(),
                OpenOptions {
                    read: false,
                    write: true,
                    append: false,
                    nonblock: false,
                },
                String::from("/dev/tty"),
                false,
                false,
            )),
        );
        files.insert(
            2,
            FileLike::File(FileHandle::new(
                crate::fs::TTY.clone(),
                OpenOptions {
                    read: false,
                    write: true,
                    append: false,
                    nonblock: false,
                },
                String::from("/dev/tty"),
                false,
                false,
            )),
        );

        // user context
        let mut context = UserContext::default();
        context.general.set_ip(entry_addr);
        context.general.set_sp(ustack_top);
        context.general.rflags = 0x3202;

        let thread = Thread {
            tid: 0, // allocated below
            inner: Mutex::new(ThreadInner {
                context: Some(Box::from(context)),
                clear_child_tid: 0,
            }),
            vm: vm.clone(),
            proc: Arc::new(Mutex::new(Process {
                vm,
                files,
                cwd: String::from("/"),
                exec_path: String::from(exec_path),
                futexes: BTreeMap::default(),
                semaphores: SemProc::default(),
                pid: Pid::new(), // allocated later
                pgid: 0,
                parent: (Pid::new(), Weak::new()),
                children: Vec::new(),
                threads: Vec::new(),
                exit_code: 0,
                pending_sigset: Sigset::empty(),
                sig_queue: VecDeque::new(),
                dispositions: [SignalAction::default(); Signal::RTMAX + 1],
                sigaltstack: SignalStack::default(),
                eventbus: EventBus::new(),
            })),
            sig_mask: Sigset::default(),
        };

        let res = thread.add_to_table();

        // set pid to tid
        add_to_process_table(res.proc.clone(), Pid(res.tid));

        res
    }

    /// Fork a new process from current one
    /// Only current process is persisted
    pub fn fork(&self, tf: &UserContext) -> Arc<Thread> {
        /// clone virtual memory
        let vm = self.vm.lock().clone();
        let vm_token = vm.token();
        let vm = Arc::new(Mutex::new(vm));

        /// context of new thread
        let mut context = tf.clone();
        context.general.set_syscall_ret(0);

        let mut proc = self.proc.lock();

        let new_proc = Arc::new(Mutex::new(Process {
            vm: vm.clone(),
            files: proc.files.clone(), // share open file descriptions
            cwd: proc.cwd.clone(),
            exec_path: proc.exec_path.clone(),
            futexes: BTreeMap::default(),
            semaphores: proc.semaphores.clone(),
            pid: Pid::new(), // assigned later
            pgid: proc.pgid,
            parent: (proc.pid.clone(), Arc::downgrade(&self.proc)),
            children: Vec::new(),
            threads: Vec::new(),
            exit_code: 0,
            pending_sigset: Sigset::empty(),
            sig_queue: VecDeque::new(),
            dispositions: proc.dispositions.clone(),
            sigaltstack: Default::default(),
            eventbus: EventBus::new(),
        }));

        // new thread
        // this part in linux manpage seems ambiguous:
        // Each of the threads in a process has its own signal mask.
        // A child created via fork(2) inherits a copy of its parent's signal
        // mask; the signal mask is preserved across execve(2).
        let new_thread = Thread {
            tid: 0, // allocated below
            inner: Mutex::new(ThreadInner {
                context: Some(Box::new(context)),
                clear_child_tid: 0,
            }),
            vm,
            proc: new_proc,
            sig_mask: self.sig_mask,
        }
        .add_to_table();

        // link thread and process
        let child_pid = Pid(new_thread.tid);
        add_to_process_table(new_thread.proc.clone(), Pid(new_thread.tid));
        new_thread.proc.lock().threads.push(new_thread.tid);

        // link to parent
        proc.children
            .push((child_pid, Arc::downgrade(&new_thread.proc)));

        new_thread
    }

    /// Create a new thread in the same process.
    pub fn new_clone(
        &self,
        context: &UserContext,
        stack_top: usize,
        tls: usize,
        clear_child_tid: usize,
    ) -> Arc<Thread> {
        let vm_token = self.vm.lock().token();
        let mut new_context = context.clone();
        new_context.general.set_syscall_ret(0);
        new_context.general.set_sp(stack_top);
        new_context.general.set_tls(tls);

        let thread = Thread {
            tid: 0,
            inner: Mutex::new(ThreadInner {
                clear_child_tid,
                context: Some(Box::new(new_context)),
            }),
            vm: self.vm.clone(),
            proc: self.proc.clone(),
            sig_mask: self.sig_mask,
        };
        let res = thread.add_to_table();
        res.proc.lock().threads.push(res.tid);
        res
    }

    pub fn begin_running(&self) -> Box<UserContext> {
        self.inner.lock().context.take().unwrap()
    }

    pub fn end_running(&self, cx: Box<UserContext>) {
        self.inner.lock().context = Some(cx);
    }
}

pub fn spawn(thread: Arc<Thread>) {
    let vmtoken = thread.vm.lock().token();
    let temp = thread.clone();
    let future = async move {
        loop {
            let mut cx = thread.begin_running();
            trace!("go to user: {:#x?}", cx);
            cx.run();
            trace!("back from user: {:#x?}", cx);

            let mut exit = false;
            match cx.trap_num {
                0x100 => exit = handle_syscall(&thread, &mut cx).await,
                0x20..=0x3f => {
                    let mut lapic = unsafe { XApic::new(phys_to_virt(LAPIC_ADDR)) };
                    lapic.eoi();
                    trace!("handle irq {}", cx.trap_num);
                    if cx.trap_num == 0x20 {
                        crate::trap::timer();
                    }
                    if cx.trap_num == 0x20 + 4 {
                        use crate::arch::driver::serial::*;
                        info!("\nInterupt: COM1");
                        crate::trap::serial(COM1.lock().receive());
                    }
                }
                0xe => {
                    // page fault
                    let addr = get_page_fault_addr();
                    debug!("page fault from user @ {:#x}", addr);

                    thread.vm.lock().handle_page_fault(addr as usize);
                }
                _ => {}
            }
            thread.end_running(cx);
            if exit {
                break;
            }
        }
    };

    spawn_thread(Box::pin(future), vmtoken, temp);
}

fn spawn_thread(
    future: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    vmtoken: usize,
    thread: Arc<Thread>,
) {
    executor::spawn(PageTableSwitchWrapper {
        inner: Mutex::new(future),
        vmtoken,
        thread,
    });
}

#[must_use = "future does nothing unless polled/`await`-ed"]
struct PageTableSwitchWrapper {
    inner: Mutex<Pin<Box<dyn Future<Output = ()> + Send>>>,
    vmtoken: usize,
    thread: Arc<Thread>,
}

impl Future for PageTableSwitchWrapper {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // set cpu local thread
        // TODO: task local?
        let cpu_id = cpu::id();
        unsafe {
            PROCESSORS[cpu_id] = Some(self.thread.clone());
        }
        // vmtoken won't change
        set_page_table(self.vmtoken);
        let res = self.inner.lock().as_mut().poll(cx);
        unsafe {
            PROCESSORS[cpu_id] = None;
        }
        res
    }
}
