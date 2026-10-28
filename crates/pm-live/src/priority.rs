//! Scheduling priority for the core thread. Its ticks take tens of µs, but on a busy desktop the
//! OS can wake it on an efficiency core or preempt it mid-tick for a whole time slice (~10 ms).
//! Raising its priority makes both less likely. Best effort: a failure is logged, not fatal.

/// macOS: QoS class "user interactive", which the scheduler runs ahead of default and background
/// work and keeps on the performance cores.
#[cfg(target_os = "macos")]
pub fn raise_current_thread(name: &str) {
    // SAFETY: plain FFI calls on the current thread with valid arguments and out-pointers.
    let rc = unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
    };
    if rc != 0 {
        tracing::warn!("{name} thread: setting QoS user-interactive failed (errno {rc})");
        return;
    }
    let mut class = libc::qos_class_t::QOS_CLASS_UNSPECIFIED;
    let mut relative = 0;
    unsafe { libc::pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut relative) };
    tracing::info!("{name} thread: QoS {class:?}");
}

/// Linux: nice -10 for this thread only (on Linux a thread id works with `setpriority`).
/// Needs `CAP_SYS_NICE`, e.g. `AmbientCapabilities=CAP_SYS_NICE` or `Nice=-10` in the systemd unit;
/// without it this logs a warning and the thread keeps the default priority. Real-time scheduling
/// (SCHED_FIFO) is deliberately not used: a stuck real-time thread can starve the whole host.
#[cfg(target_os = "linux")]
pub fn raise_current_thread(name: &str) {
    const NICE: libc::c_int = -10;
    // SAFETY: plain FFI calls on the current thread.
    let tid = unsafe { libc::gettid() } as libc::id_t;
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, NICE) } != 0 {
        let err = std::io::Error::last_os_error();
        tracing::warn!("{name} thread: setting nice {NICE} failed ({err}); needs CAP_SYS_NICE");
        return;
    }
    tracing::info!("{name} thread: nice {NICE}");
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn raise_current_thread(_name: &str) {}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn core_thread_gets_user_interactive_qos() {
        std::thread::spawn(|| {
            super::raise_current_thread("test");
            let mut class = libc::qos_class_t::QOS_CLASS_UNSPECIFIED;
            let mut relative = 0;
            unsafe {
                libc::pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut relative)
            };
            assert!(matches!(
                class,
                libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE
            ));
        })
        .join()
        .unwrap();
    }
}
