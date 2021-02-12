/// This function is where the kernel sets up IRQ handlers
/// It is increcibly unsafe, and should be minimal in nature
/// It must create the IDT with the correct entries, those entries are
/// defined in other files inside of the `arch` module

use core::slice;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::allocator;
#[cfg(feature = "acpi")]
use crate::acpi;
#[cfg(feature = "graphical_debug")]
use crate::arch::x86_64::graphical_debug;
use crate::arch::x86_64::pti;
use crate::arch::x86_64::flags::*;
use crate::device;
use crate::gdt;
use crate::idt;
use crate::interrupt;
use crate::log::{self, info};
use crate::paging;

/// Test of zero values in BSS.
static BSS_TEST_ZERO: usize = 0;
/// Test of non-zero values in data.
static DATA_TEST_NONZERO: usize = 0xFFFF_FFFF_FFFF_FFFF;
/// Test of zero values in thread BSS
#[thread_local]
static mut TBSS_TEST_ZERO: usize = 0;
/// Test of non-zero values in thread data.
#[thread_local]
static mut TDATA_TEST_NONZERO: usize = 0xFFFF_FFFF_FFFF_FFFF;

pub static KERNEL_BASE: AtomicUsize = AtomicUsize::new(0);
pub static KERNEL_SIZE: AtomicUsize = AtomicUsize::new(0);
pub static CPU_COUNT: AtomicUsize = AtomicUsize::new(0);
pub static AP_READY: AtomicBool = AtomicBool::new(false);
static BSP_READY: AtomicBool = AtomicBool::new(false);

#[repr(packed)]
pub struct KernelArgs {
    kernel_base: u64,
    kernel_size: u64,
    stack_base: u64,
    stack_size: u64,
    env_base: u64,
    env_size: u64,

    /// The base 64-bit pointer to an array of saved RSDPs. It's up to the kernel (and possibly
    /// userspace), to decide which RSDP to use. The buffer will be a linked list containing a
    /// 32-bit relative (to this field) next, and the actual struct afterwards.
    ///
    /// This field can be NULL, and if so, the system has not booted with UEFI or in some other way
    /// retrieved the RSDPs. The kernel or a userspace driver will thus try searching the BIOS
    /// memory instead. On UEFI systems, searching is not guaranteed to actually work though.
    acpi_rsdps_base: u64,
    /// The size of the RSDPs region.
    acpi_rsdps_size: u64,
}

/// The entry to Rust, all things must be initialized
#[no_mangle]
pub unsafe extern fn kstart(args_ptr: *const KernelArgs) -> ! {
    let env = {
        let args = &*args_ptr;

        let kernel_base = args.kernel_base as usize;
        let kernel_size = args.kernel_size as usize;
        let stack_base = args.stack_base as usize;
        let stack_size = args.stack_size as usize;
        let env_base = args.env_base as usize;
        let env_size = args.env_size as usize;
        let acpi_rsdps_base = args.acpi_rsdps_base;
        let acpi_rsdps_size = args.acpi_rsdps_size;

        // BSS should already be zero
        {
            assert_eq!(BSS_TEST_ZERO, 0);
            assert_eq!(DATA_TEST_NONZERO, 0xFFFF_FFFF_FFFF_FFFF);
        }

        KERNEL_BASE.store(kernel_base, Ordering::SeqCst);
        KERNEL_SIZE.store(kernel_size, Ordering::SeqCst);

        // Initialize logger
        log::init_logger(|r| {
            use core::fmt::Write;
            let _ = write!(
                crate::arch::x86_64::debug::Writer::new(),
                "{}:{} -- {}\n",
                r.target(),
                r.level(),
                r.args()
            );
        });

        info!("Redox OS starting...");
        info!("Kernel: {:X}:{:X}", kernel_base, kernel_base + kernel_size);
        info!("Stack: {:X}:{:X}", stack_base, stack_base + stack_size);
        info!("Env: {:X}:{:X}", env_base, env_base + env_size);
        info!("RSDPs: {:X}:{:X}", acpi_rsdps_base, acpi_rsdps_base + acpi_rsdps_size);

        // Set up GDT before paging
        gdt::init();

        // Set up IDT before paging
        idt::init();

        // Initialize RMM
        crate::arch::rmm::init(kernel_base, kernel_size);

        // Initialize paging
        let (mut active_table, tcb_offset) = paging::init(0);

        // Set up GDT after paging with TLS
        gdt::init_paging(tcb_offset, stack_base + stack_size);

        // Set up IDT
        idt::init_paging_bsp();

        // Set up syscall instruction
        interrupt::syscall::init();

        // Test tdata and tbss
        {
            assert_eq!(TBSS_TEST_ZERO, 0);
            TBSS_TEST_ZERO += 1;
            assert_eq!(TBSS_TEST_ZERO, 1);
            assert_eq!(TDATA_TEST_NONZERO, 0xFFFF_FFFF_FFFF_FFFF);
            TDATA_TEST_NONZERO -= 1;
            assert_eq!(TDATA_TEST_NONZERO, 0xFFFF_FFFF_FFFF_FFFE);
        }

        // Reset AP variables
        CPU_COUNT.store(1, Ordering::SeqCst);
        AP_READY.store(false, Ordering::SeqCst);
        BSP_READY.store(false, Ordering::SeqCst);

        // Setup kernel heap
        allocator::init(&mut active_table);

        idt::init_paging_post_heap(true, 0);

        // Activate memory logging
        log::init();

        // Use graphical debug
        #[cfg(feature="graphical_debug")]
        graphical_debug::init(&mut active_table);

        #[cfg(feature = "system76_ec_debug")]
        device::system76_ec::init();

        // Initialize devices
        device::init(&mut active_table);

        // Read ACPI tables, starts APs
        #[cfg(feature = "acpi")]
        {
            acpi::init(&mut active_table, if acpi_rsdps_base != 0 && acpi_rsdps_size > 0 { Some((acpi_rsdps_base, acpi_rsdps_size)) } else { None });
            device::init_after_acpi(&mut active_table);
        }

        // Initialize all of the non-core devices not otherwise needed to complete initialization
        device::init_noncore();

        // Stop graphical debug
        #[cfg(feature="graphical_debug")]
        graphical_debug::fini(&mut active_table);

        BSP_READY.store(true, Ordering::SeqCst);

        slice::from_raw_parts(env_base as *const u8, env_size)
    };

    crate::kmain(CPU_COUNT.load(Ordering::SeqCst), env);
}

#[repr(packed)]
pub struct KernelArgsAp {
    cpu_id: u64,
    page_table: u64,
    stack_start: u64,
    stack_end: u64,
}

/// Entry to rust for an AP
pub unsafe extern fn kstart_ap(args_ptr: *const KernelArgsAp) -> ! {
    let cpu_id = {
        let args = &*args_ptr;
        let cpu_id = args.cpu_id as usize;
        let bsp_table = args.page_table as usize;
        let _stack_start = args.stack_start as usize;
        let stack_end = args.stack_end as usize;

        assert_eq!(BSS_TEST_ZERO, 0);
        assert_eq!(DATA_TEST_NONZERO, 0xFFFF_FFFF_FFFF_FFFF);

        // Set up GDT before paging
        gdt::init();

        // Set up IDT before paging
        idt::init();

        // Initialize paging
        let tcb_offset = paging::init_ap(cpu_id, bsp_table);

        // Set up GDT with TLS
        gdt::init_paging(tcb_offset, stack_end);

        // Set up IDT for AP
        idt::init_paging_post_heap(false, cpu_id);

        // Set up syscall instruction
        interrupt::syscall::init();

        // Test tdata and tbss
        {
            assert_eq!(TBSS_TEST_ZERO, 0);
            TBSS_TEST_ZERO += 1;
            assert_eq!(TBSS_TEST_ZERO, 1);
            assert_eq!(TDATA_TEST_NONZERO, 0xFFFF_FFFF_FFFF_FFFF);
            TDATA_TEST_NONZERO -= 1;
            assert_eq!(TDATA_TEST_NONZERO, 0xFFFF_FFFF_FFFF_FFFE);
        }

        // Initialize devices (for AP)
        device::init_ap();

        AP_READY.store(true, Ordering::SeqCst);

        cpu_id
    };

    while ! BSP_READY.load(Ordering::SeqCst) {
        interrupt::pause();
    }

    crate::kmain_ap(cpu_id);
}

#[naked]
#[inline(never)]
// TODO: AbiCompatBool
pub unsafe extern "C" fn usermode(_ip: usize, _sp: usize, _arg: usize, _singlestep: u32) -> ! {
    // rdi, rsi, rdx, rcx
    asm!(
        "
            mov rbx, {flag_interrupts}
            test ecx, ecx
            jz .after_singlestep_branch
            or rbx, {flag_singlestep}

            .after_singlestep_branch:

            // save `ip` (rdi), `sp` (rsi), and `arg` (rdx) in callee-preserved registers, so that
            // they are not modified by `pti_unmap`

            mov r13, rdi
            mov r14, rsi
            mov r15, rdx
            call {pti_unmap}

            // Go to usermode
            swapgs
            mov r8, {user_data_seg_selector}
            mov r9, {user_tls_seg_selector}
            mov ds, r8d
            mov es, r8d
            mov fs, r9d
            mov gs, r8d

            // Target RFLAGS
            mov r11, rbx
            // Target instruction pointer
            mov rcx, r13
            // Target stack pointer
            mov rsp, r14
            // Target argument
            mov rdi, r15

            xor rax, rax
            xor rbx, rbx
            // Don't zero rcx; it's used for `ip`.
            xor rdx, rdx
            // Don't zero rdi; it's used for `arg`.
            xor rsi, rsi
            xor rbp, rbp
            // Don't zero rsp, obviously.
            xor r8, r8
            xor r9, r9
            xor r10, r10
            // Don't zero r11; it's used for `rflags`.
            xor r12, r12
            xor r13, r13
            xor r14, r14
            xor r15, r15

            fninit
            sysretq
        ",

        flag_interrupts = const(FLAG_INTERRUPTS),
        flag_singlestep = const(FLAG_SINGLESTEP),
        pti_unmap = sym pti::unmap,
        user_data_seg_selector = const(gdt::GDT_USER_DATA << 3 | 3),
        user_tls_seg_selector = const(gdt::GDT_USER_TLS << 3 | 3),
        options(noreturn),
    );
}
