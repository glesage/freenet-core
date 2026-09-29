//! Process measurements for the feasibility runs: memory, CPU time and
//! storage, read from the OS by the process itself.

use std::path::Path;

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize, uniffi::Record)]
pub struct ProcessMetrics {
    /// Resident memory.
    pub resident_bytes: u64,
    /// The figure the OS kills apps by: `phys_footprint` on Apple platforms,
    /// proportional set size (PSS) on Android. `None` where unavailable.
    pub footprint_bytes: Option<u64>,
    /// Peak resident memory where the OS reports it.
    pub resident_peak_bytes: Option<u64>,
    /// Reserved address space. Wasm memories reserve large regions, and iOS
    /// caps a process's address space.
    pub virtual_bytes: u64,
    pub user_cpu_ms: f64,
    pub system_cpu_ms: f64,
    pub threads: Option<u32>,
}

#[uniffi::export]
pub fn process_metrics() -> ProcessMetrics {
    let (user_cpu_ms, system_cpu_ms) = cpu_times();
    let memory = memory();
    ProcessMetrics {
        resident_bytes: memory.resident,
        footprint_bytes: memory.footprint,
        resident_peak_bytes: memory.resident_peak,
        virtual_bytes: memory.virtual_size,
        user_cpu_ms,
        system_cpu_ms,
        threads: memory.threads,
    }
}

/// The node's peer traffic since the process started, counted by Core's
/// transport: every UDP datagram it sent, including keep-alives and
/// retransmissions, and every datagram it received that passed
/// authentication. Loopback traffic between the app and the node is not in
/// either figure. Subtract two readings to get one workload's traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, uniffi::Record)]
pub struct NodeTraffic {
    pub upload_bytes: u64,
    pub download_bytes: u64,
}

#[uniffi::export]
pub fn node_traffic() -> NodeTraffic {
    let metrics = &*freenet::transport::TRANSPORT_METRICS;
    NodeTraffic {
        upload_bytes: metrics.cumulative_bytes_sent(),
        download_bytes: metrics.cumulative_bytes_received(),
    }
}

#[uniffi::export]
pub fn node_traffic_json(traffic: NodeTraffic) -> String {
    serde_json::to_string(&traffic).unwrap_or_default()
}

/// Bytes stored below `path`, not following symlinks.
#[uniffi::export]
pub fn directory_size_bytes(path: String) -> u64 {
    fn walk(path: &Path) -> u64 {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return 0;
        };
        if meta.is_file() {
            return meta.len();
        }
        if !meta.is_dir() {
            return 0;
        }
        std::fs::read_dir(path)
            .map(|entries| entries.flatten().map(|entry| walk(&entry.path())).sum())
            .unwrap_or(0)
    }
    walk(Path::new(&path))
}

/// Milliseconds since the Unix epoch, for timestamps shared with the host.
#[uniffi::export]
pub fn unix_time_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

fn cpu_times() -> (f64, f64) {
    // SAFETY: `rusage` is plain integers, so all-zero is a valid value, and
    // `getrusage` writes only into the buffer it is given.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: RUSAGE_SELF with a valid, caller-owned out-pointer.
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if rc != 0 {
        return (0.0, 0.0);
    }
    let ms = |tv: libc::timeval| tv.tv_sec as f64 * 1000.0 + tv.tv_usec as f64 / 1000.0;
    (ms(usage.ru_utime), ms(usage.ru_stime))
}

#[derive(Default)]
struct Memory {
    resident: u64,
    resident_peak: Option<u64>,
    footprint: Option<u64>,
    virtual_size: u64,
    threads: Option<u32>,
}

#[cfg(target_vendor = "apple")]
fn memory() -> Memory {
    /// The leading fields of `struct task_vm_info` in `<mach/task_info.h>`,
    /// through `phys_footprint` (revision 1). The header declares it under
    /// `#pragma pack(4)`.
    #[repr(C, packed(4))]
    #[derive(Default, Clone, Copy)]
    struct TaskVmInfo {
        virtual_size: u64,
        region_count: i32,
        page_size: i32,
        resident_size: u64,
        resident_size_peak: u64,
        device: u64,
        device_peak: u64,
        internal: u64,
        internal_peak: u64,
        external: u64,
        external_peak: u64,
        reusable: u64,
        reusable_peak: u64,
        purgeable_volatile_pmap: u64,
        purgeable_volatile_resident: u64,
        purgeable_volatile_virtual: u64,
        compressed: u64,
        compressed_peak: u64,
        compressed_lifetime: u64,
        phys_footprint: u64,
    }
    const TASK_VM_INFO: libc::task_flavor_t = 22;
    let mut info = TaskVmInfo::default();
    let mut count = (std::mem::size_of::<TaskVmInfo>() / std::mem::size_of::<libc::natural_t>())
        as libc::mach_msg_type_number_t;
    // SAFETY: `task_info` fills at most `count` natural_t words of the
    // caller-owned buffer, which is exactly that large, for this task's own
    // port. `mach_task_self` reads a process-global port.
    #[allow(deprecated)]
    let kr = unsafe {
        libc::task_info(
            libc::mach_task_self(),
            TASK_VM_INFO,
            (&mut info as *mut TaskVmInfo).cast(),
            &mut count,
        )
    };
    if kr != libc::KERN_SUCCESS {
        return Memory::default();
    }
    let TaskVmInfo {
        virtual_size,
        resident_size,
        resident_size_peak,
        phys_footprint,
        ..
    } = info;
    Memory {
        resident: resident_size,
        resident_peak: Some(resident_size_peak),
        footprint: Some(phys_footprint),
        virtual_size,
        threads: None,
    }
}

#[cfg(any(target_os = "android", target_os = "linux"))]
fn memory() -> Memory {
    fn kib_field(text: &str, name: &str) -> Option<u64> {
        text.lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u64>().ok())
            .map(|kib| kib * 1024)
    }
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let pss = std::fs::read_to_string("/proc/self/smaps_rollup")
        .ok()
        .and_then(|text| kib_field(&text, "Pss:"));
    Memory {
        resident: kib_field(&status, "VmRSS:").unwrap_or(0),
        resident_peak: kib_field(&status, "VmHWM:"),
        footprint: pss,
        virtual_size: kib_field(&status, "VmSize:").unwrap_or(0),
        threads: status
            .lines()
            .find(|line| line.starts_with("Threads:"))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse().ok()),
    }
}

#[cfg(not(any(target_vendor = "apple", target_os = "android", target_os = "linux")))]
fn memory() -> Memory {
    Memory::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_this_process() {
        let metrics = process_metrics();
        assert!(metrics.resident_bytes > 0);
        assert!(metrics.virtual_bytes >= metrics.resident_bytes);
        assert!(metrics.user_cpu_ms >= 0.0);
        #[cfg(target_vendor = "apple")]
        assert!(metrics.footprint_bytes.unwrap() > 0);
    }

    #[test]
    fn sizes_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), vec![0u8; 1000]).unwrap();
        std::fs::create_dir(dir.path().join("b")).unwrap();
        std::fs::write(dir.path().join("b/c"), vec![0u8; 24]).unwrap();
        assert_eq!(directory_size_bytes(dir.path().display().to_string()), 1024);
    }
}
