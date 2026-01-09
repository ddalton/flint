#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

use std::ffi::CString;
use std::mem;
use std::ptr;
use std::time::Instant;

const BLOCK_SIZE: u64 = 4096; // 4KB blocks
const NUM_BLOCKS: u64 = 262144; // 1GB of 4KB blocks
const QUEUE_DEPTH: u32 = 128;

struct NvmeController {
    ctrlr: *mut spdk_nvme_ctrlr,
    ns: *mut spdk_nvme_ns,
    qpair: *mut spdk_nvme_qpair,
}

struct IoContext {
    buffer: *mut u8,
    completed: bool,
    success: bool,
}

// Helper function to check if completion has error (replaces missing spdk_nvme_cpl_is_error macro)
unsafe fn is_cpl_error(cpl: *const spdk_nvme_cpl) -> bool {
    // Check status field - 0 means success
    // The completion structure has a status field that indicates errors
    let status = (*cpl).status;
    (status & 0xFFFE) != 0  // Check status bits (bit 0 is phase, bits 1+ are status)
}

extern "C" fn io_complete_cb(
    _arg: *mut ::std::os::raw::c_void,
    cpl: *const spdk_nvme_cpl,
) {
    let ctx = _arg as *mut IoContext;
    unsafe {
        (*ctx).completed = true;
        (*ctx).success = !is_cpl_error(cpl);
    }
}

extern "C" fn probe_cb(
    _cb_ctx: *mut ::std::os::raw::c_void,
    trid: *const spdk_nvme_transport_id,
    opts: *mut spdk_nvme_ctrlr_opts,
) -> bool {
    unsafe {
        let addr = (*trid).traddr.as_ptr();
        println!("Found NVMe controller: {}", std::ffi::CStr::from_ptr(addr).to_string_lossy());
    }
    true
}

extern "C" fn attach_cb(
    cb_ctx: *mut ::std::os::raw::c_void,
    trid: *const spdk_nvme_transport_id,
    ctrlr: *mut spdk_nvme_ctrlr,
    opts: *const spdk_nvme_ctrlr_opts,
) {
    let nvme_ctx = cb_ctx as *mut *mut NvmeController;

    unsafe {
        // Get the first namespace
        let ns_id = spdk_nvme_ctrlr_get_first_active_ns(ctrlr);
        if ns_id == 0 {
            println!("No active namespaces found");
            return;
        }

        let ns = spdk_nvme_ctrlr_get_ns(ctrlr, ns_id);
        if ns.is_null() {
            println!("Failed to get namespace");
            return;
        }

        // Allocate I/O queue pair
        let qpair = spdk_nvme_ctrlr_alloc_io_qpair(ctrlr, ptr::null(), 0);
        if qpair.is_null() {
            println!("Failed to allocate I/O queue pair");
            return;
        }

        let ctx = Box::new(NvmeController {
            ctrlr,
            ns,
            qpair,
        });

        *nvme_ctx = Box::into_raw(ctx);

        let num_sectors = spdk_nvme_ns_get_num_sectors(ns);
        let sector_size = spdk_nvme_ns_get_sector_size(ns);
        let size_gb = (num_sectors * sector_size as u64) / (1024 * 1024 * 1024);

        println!("✓ Attached NVMe controller");
        println!("  Namespace ID: {}", ns_id);
        println!("  Capacity: {} GB", size_gb);
        println!("  Sector size: {} bytes", sector_size);
        println!("  Queue depth: {}", QUEUE_DEPTH);
    }
}

fn run_sequential_read_test(nvme: &NvmeController) -> Result<(f64, f64), String> {
    println!("\n═══════════════════════════════════════════════════════");
    println!("SEQUENTIAL READ TEST (SPDK Native Polling Mode)");
    println!("═══════════════════════════════════════════════════════");

    unsafe {
        let sector_size = spdk_nvme_ns_get_sector_size(nvme.ns) as u64;
        let blocks_per_io = BLOCK_SIZE / sector_size;

        // Allocate DMA buffer
        let buffer_size = (BLOCK_SIZE * QUEUE_DEPTH as u64) as usize;
        let buffer = spdk_zmalloc(
            buffer_size,
            0x1000, // 4KB alignment
            ptr::null_mut(),
            0,
            0,
        ) as *mut u8;

        if buffer.is_null() {
            return Err("Failed to allocate DMA buffer".to_string());
        }

        let mut contexts: Vec<Box<IoContext>> = Vec::new();
        for _ in 0..QUEUE_DEPTH {
            contexts.push(Box::new(IoContext {
                buffer,
                completed: false,
                success: false,
            }));
        }

        let start = Instant::now();
        let mut lba = 0u64;
        let mut submitted = 0u64;
        let mut completed = 0u64;
        let mut in_flight = 0u32;

        while completed < NUM_BLOCKS {
            // Submit I/Os up to queue depth
            while in_flight < QUEUE_DEPTH && submitted < NUM_BLOCKS {
                let ctx_idx = (submitted % QUEUE_DEPTH as u64) as usize;
                let ctx_ptr = contexts[ctx_idx].as_mut() as *mut IoContext;

                (*ctx_ptr).completed = false;
                (*ctx_ptr).success = false;

                let rc = spdk_nvme_ns_cmd_read(
                    nvme.ns,
                    nvme.qpair,
                    buffer.add((ctx_idx * BLOCK_SIZE as usize) as usize) as *mut _,
                    lba,
                    blocks_per_io as u32,
                    Some(io_complete_cb),
                    ctx_ptr as *mut _,
                    0,
                );

                if rc != 0 {
                    spdk_free(buffer as *mut _);
                    return Err(format!("Failed to submit read command: {}", rc));
                }

                lba += blocks_per_io;
                submitted += 1;
                in_flight += 1;
            }

            // Poll for completions
            spdk_nvme_qpair_process_completions(nvme.qpair, 0);

            // Check for completed I/Os
            for ctx in contexts.iter_mut() {
                if ctx.completed {
                    if !ctx.success {
                        spdk_free(buffer as *mut _);
                        return Err("I/O failed".to_string());
                    }
                    ctx.completed = false;
                    completed += 1;
                    in_flight -= 1;
                }
            }
        }

        let elapsed = start.elapsed();
        let bytes_read = NUM_BLOCKS * BLOCK_SIZE;
        let throughput_gbps = (bytes_read as f64 / elapsed.as_secs_f64()) / (1024.0 * 1024.0 * 1024.0);
        let iops = NUM_BLOCKS as f64 / elapsed.as_secs_f64();

        spdk_free(buffer as *mut _);

        println!("Completed: {} blocks in {:.2}s", completed, elapsed.as_secs_f64());
        println!("Throughput: {:.2} GB/s", throughput_gbps);
        println!("IOPS: {:.0}", iops);

        Ok((throughput_gbps, iops))
    }
}

fn run_sequential_write_test(nvme: &NvmeController) -> Result<(f64, f64), String> {
    println!("\n═══════════════════════════════════════════════════════");
    println!("SEQUENTIAL WRITE TEST (SPDK Native Polling Mode)");
    println!("═══════════════════════════════════════════════════════");

    unsafe {
        let sector_size = spdk_nvme_ns_get_sector_size(nvme.ns) as u64;
        let blocks_per_io = BLOCK_SIZE / sector_size;

        // Allocate DMA buffer
        let buffer_size = (BLOCK_SIZE * QUEUE_DEPTH as u64) as usize;
        let buffer = spdk_zmalloc(
            buffer_size,
            0x1000,
            ptr::null_mut(),
            0,
            0,
        ) as *mut u8;

        if buffer.is_null() {
            return Err("Failed to allocate DMA buffer".to_string());
        }

        // Fill buffer with test data
        for i in 0..buffer_size {
            *buffer.add(i) = (i % 256) as u8;
        }

        let mut contexts: Vec<Box<IoContext>> = Vec::new();
        for _ in 0..QUEUE_DEPTH {
            contexts.push(Box::new(IoContext {
                buffer,
                completed: false,
                success: false,
            }));
        }

        let start = Instant::now();
        let mut lba = 0u64;
        let mut submitted = 0u64;
        let mut completed = 0u64;
        let mut in_flight = 0u32;

        while completed < NUM_BLOCKS {
            // Submit I/Os up to queue depth
            while in_flight < QUEUE_DEPTH && submitted < NUM_BLOCKS {
                let ctx_idx = (submitted % QUEUE_DEPTH as u64) as usize;
                let ctx_ptr = contexts[ctx_idx].as_mut() as *mut IoContext;

                (*ctx_ptr).completed = false;
                (*ctx_ptr).success = false;

                let rc = spdk_nvme_ns_cmd_write(
                    nvme.ns,
                    nvme.qpair,
                    buffer.add((ctx_idx * BLOCK_SIZE as usize) as usize) as *mut _,
                    lba,
                    blocks_per_io as u32,
                    Some(io_complete_cb),
                    ctx_ptr as *mut _,
                    0,
                );

                if rc != 0 {
                    spdk_free(buffer as *mut _);
                    return Err(format!("Failed to submit write command: {}", rc));
                }

                lba += blocks_per_io;
                submitted += 1;
                in_flight += 1;
            }

            // Poll for completions
            spdk_nvme_qpair_process_completions(nvme.qpair, 0);

            // Check for completed I/Os
            for ctx in contexts.iter_mut() {
                if ctx.completed {
                    if !ctx.success {
                        spdk_free(buffer as *mut _);
                        return Err("I/O failed".to_string());
                    }
                    ctx.completed = false;
                    completed += 1;
                    in_flight -= 1;
                }
            }
        }

        let elapsed = start.elapsed();
        let bytes_written = NUM_BLOCKS * BLOCK_SIZE;
        let throughput_gbps = (bytes_written as f64 / elapsed.as_secs_f64()) / (1024.0 * 1024.0 * 1024.0);
        let iops = NUM_BLOCKS as f64 / elapsed.as_secs_f64();

        spdk_free(buffer as *mut _);

        println!("Completed: {} blocks in {:.2}s", completed, elapsed.as_secs_f64());
        println!("Throughput: {:.2} GB/s", throughput_gbps);
        println!("IOPS: {:.0}", iops);

        Ok((throughput_gbps, iops))
    }
}

fn run_random_read_test(nvme: &NvmeController) -> Result<(f64, f64), String> {
    println!("\n═══════════════════════════════════════════════════════");
    println!("RANDOM READ TEST (4K blocks, SPDK Native Polling)");
    println!("═══════════════════════════════════════════════════════");

    unsafe {
        let sector_size = spdk_nvme_ns_get_sector_size(nvme.ns) as u64;
        let blocks_per_io = BLOCK_SIZE / sector_size;
        let max_lba = spdk_nvme_ns_get_num_sectors(nvme.ns) - blocks_per_io;

        // Allocate DMA buffer
        let buffer_size = (BLOCK_SIZE * QUEUE_DEPTH as u64) as usize;
        let buffer = spdk_zmalloc(
            buffer_size,
            0x1000,
            ptr::null_mut(),
            0,
            0,
        ) as *mut u8;

        if buffer.is_null() {
            return Err("Failed to allocate DMA buffer".to_string());
        }

        let mut contexts: Vec<Box<IoContext>> = Vec::new();
        for _ in 0..QUEUE_DEPTH {
            contexts.push(Box::new(IoContext {
                buffer,
                completed: false,
                success: false,
            }));
        }

        let start = Instant::now();
        let mut submitted = 0u64;
        let mut completed = 0u64;
        let mut in_flight = 0u32;

        // Simple pseudo-random number generator
        let mut rand_state = 0x12345678u64;

        while completed < NUM_BLOCKS {
            // Submit I/Os up to queue depth
            while in_flight < QUEUE_DEPTH && submitted < NUM_BLOCKS {
                let ctx_idx = (submitted % QUEUE_DEPTH as u64) as usize;
                let ctx_ptr = contexts[ctx_idx].as_mut() as *mut IoContext;

                (*ctx_ptr).completed = false;
                (*ctx_ptr).success = false;

                // Generate random LBA
                rand_state = rand_state.wrapping_mul(1103515245).wrapping_add(12345);
                let lba = (rand_state % max_lba) & !((blocks_per_io - 1) as u64);

                let rc = spdk_nvme_ns_cmd_read(
                    nvme.ns,
                    nvme.qpair,
                    buffer.add((ctx_idx * BLOCK_SIZE as usize) as usize) as *mut _,
                    lba,
                    blocks_per_io as u32,
                    Some(io_complete_cb),
                    ctx_ptr as *mut _,
                    0,
                );

                if rc != 0 {
                    spdk_free(buffer as *mut _);
                    return Err(format!("Failed to submit read command: {}", rc));
                }

                submitted += 1;
                in_flight += 1;
            }

            // Poll for completions
            spdk_nvme_qpair_process_completions(nvme.qpair, 0);

            // Check for completed I/Os
            for ctx in contexts.iter_mut() {
                if ctx.completed {
                    if !ctx.success {
                        spdk_free(buffer as *mut _);
                        return Err("I/O failed".to_string());
                    }
                    ctx.completed = false;
                    completed += 1;
                    in_flight -= 1;
                }
            }
        }

        let elapsed = start.elapsed();
        let bytes_read = NUM_BLOCKS * BLOCK_SIZE;
        let throughput_gbps = (bytes_read as f64 / elapsed.as_secs_f64()) / (1024.0 * 1024.0 * 1024.0);
        let iops = NUM_BLOCKS as f64 / elapsed.as_secs_f64();

        spdk_free(buffer as *mut _);

        println!("Completed: {} blocks in {:.2}s", completed, elapsed.as_secs_f64());
        println!("Throughput: {:.2} GB/s", throughput_gbps);
        println!("IOPS: {:.0} (4K random reads)", iops);

        Ok((throughput_gbps, iops))
    }
}

fn main() {
    println!("═══════════════════════════════════════════════════════");
    println!("SPDK Native Benchmark (Polling Mode, No Kernel)");
    println!("═══════════════════════════════════════════════════════");
    println!();

    unsafe {
        // Initialize SPDK environment
        let mut opts: spdk_env_opts = mem::zeroed();
        spdk_env_opts_init(&mut opts);
        opts.name = CString::new("spdk_benchmark").unwrap().into_raw();
        opts.core_mask = CString::new("0x1").unwrap().into_raw();

        println!("Initializing SPDK environment...");
        if spdk_env_init(&opts) < 0 {
            eprintln!("Failed to initialize SPDK environment");
            std::process::exit(1);
        }
        println!("✓ SPDK environment initialized");

        // Probe for NVMe controllers
        let mut nvme_ctx: *mut NvmeController = ptr::null_mut();
        let nvme_ctx_ptr = &mut nvme_ctx as *mut _ as *mut ::std::os::raw::c_void;

        println!("\nProbing for NVMe controllers...");
        if spdk_nvme_probe(
            ptr::null(),
            nvme_ctx_ptr,
            Some(probe_cb),
            Some(attach_cb),
            None,  // remove_cb - not needed for benchmark
        ) != 0 {
            eprintln!("Failed to probe NVMe controllers");
            std::process::exit(1);
        }

        if nvme_ctx.is_null() {
            eprintln!("No NVMe controllers found");
            std::process::exit(1);
        }

        let nvme = Box::from_raw(nvme_ctx);

        // Run benchmarks
        println!("\n═══════════════════════════════════════════════════════");
        println!("Starting benchmark tests...");
        println!("═══════════════════════════════════════════════════════");

        let seq_read_result = run_sequential_read_test(&nvme);
        let seq_write_result = run_sequential_write_test(&nvme);
        let rand_read_result = run_random_read_test(&nvme);

        // Print summary
        println!("\n═══════════════════════════════════════════════════════");
        println!("BENCHMARK SUMMARY");
        println!("═══════════════════════════════════════════════════════");

        if let Ok((throughput, iops)) = seq_read_result {
            println!("Sequential Read:  {:.2} GB/s, {:.0} IOPS", throughput, iops);
        }

        if let Ok((throughput, iops)) = seq_write_result {
            println!("Sequential Write: {:.2} GB/s, {:.0} IOPS", throughput, iops);
        }

        if let Ok((throughput, iops)) = rand_read_result {
            println!("Random Read (4K): {:.2} GB/s, {:.0} IOPS", throughput, iops);
        }

        println!("\n═══════════════════════════════════════════════════════");
        println!("SPDK Native Performance Characteristics:");
        println!("• Polling mode (no interrupts)");
        println!("• Zero-copy DMA transfers");
        println!("• Direct PCIe access (no kernel)");
        println!("• Lock-free I/O submission");
        println!("═══════════════════════════════════════════════════════");

        // Cleanup
        spdk_nvme_ctrlr_free_io_qpair(nvme.qpair);
        spdk_nvme_detach(nvme.ctrlr);
    }
}
