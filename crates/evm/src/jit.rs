//! Configuration and runtime integration for revmc JIT execution.

#[cfg(feature = "jit")]
use std::path::PathBuf;
use std::time::Duration;

#[cfg(feature = "jit")]
use revmc::runtime::RuntimeTuning;
#[cfg(feature = "jit")]
pub use revmc::runtime::{JitBackend, JitMode, RuntimeConfig, maybe_run_jit_helper};

/// Runtime settings for revmc JIT compilation.
///
/// Building with the `jit` Cargo feature makes the backend available, while [`Self::enabled`]
/// controls whether compilation starts when the node launches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JitConfig {
    /// Enables JIT compilation when the node starts.
    pub enabled: bool,
    /// Number of observed misses before bytecode is promoted for compilation.
    pub hot_threshold: usize,
    /// Number of background compilation workers, or the revmc default when unset.
    pub worker_count: Option<usize>,
    /// Capacity of the non-blocking lookup-observation channel.
    pub channel_capacity: usize,
    /// Maximum number of compilation jobs that may be pending concurrently.
    pub max_pending_jobs: usize,
    /// Maximum eligible bytecode length in bytes, where zero means unlimited.
    pub max_bytecode_len: usize,
    /// Maximum resident compiled-code size in bytes, where zero means unlimited.
    pub code_cache_bytes: usize,
    /// Idle duration after which an unused compiled program may be evicted.
    pub idle_evict_duration: Duration,
    /// Enables compiler IR, assembly, and bytecode dumps under the node data directory.
    pub debug: bool,
    /// Compiles synchronously on the first miss; intended only for tests and debugging.
    pub blocking: bool,
}

/// Runtime controls shared by JIT-enabled and interpreter-only builds.
///
/// Control delivery rides revmc's bounded command channel. When the backend thread has never
/// been started (JIT never enabled), nothing drains that channel, so a blocking `send` would
/// stall the caller forever once the channel fills. Implementations must therefore reject
/// [`Self::pause`], [`Self::resume`], and [`Self::clear`] while the backend is disabled
/// instead of queueing commands. This mirrors the upstream fix in paradigmxyz/revmc#391,
/// which our pinned revision predates and which cannot be picked up until the pin can move
/// past revmc's revm 41 upgrade.
pub trait JitBackendControl: Send + Sync {
    /// Enables or disables JIT lookups and background compilation.
    fn set_enabled(&self, enabled: bool) -> Result<(), String>;
    /// Returns whether JIT lookups and background compilation are currently enabled.
    fn is_enabled(&self) -> bool;
    /// Pauses out-of-process helper execution without discarding compiled code.
    fn pause(&self) -> Result<(), String>;
    /// Resumes out-of-process helper execution.
    fn resume(&self) -> Result<(), String>;
    /// Clears resident and persisted compiled artifacts.
    fn clear(&self) -> Result<(), String>;
}

/// Runs `control` only when the backend is enabled (and its command-draining thread runs).
///
/// See [`JitBackendControl`] for why disabled backends must reject control commands.
#[cfg(feature = "jit")]
fn run_enabled_control(
    backend: &JitBackend,
    control: impl FnOnce(&JitBackend),
) -> Result<(), String> {
    if !JitBackend::enabled(backend) {
        return Err("JIT backend is not enabled; run `reth_jit enable` first".to_string());
    }
    control(backend);
    Ok(())
}

#[cfg(feature = "jit")]
impl JitBackendControl for JitBackend {
    /// Enables or disables the revmc backend.
    fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        JitBackend::set_enabled(self, enabled).map_err(|error| error.to_string())
    }

    /// Returns whether the revmc backend is enabled.
    fn is_enabled(&self) -> bool {
        JitBackend::enabled(self)
    }

    /// Pauses the revmc helper.
    fn pause(&self) -> Result<(), String> {
        run_enabled_control(self, JitBackend::pause)
    }

    /// Resumes the revmc helper.
    fn resume(&self) -> Result<(), String> {
        run_enabled_control(self, JitBackend::resume)
    }

    /// Clears all revmc artifacts.
    fn clear(&self) -> Result<(), String> {
        run_enabled_control(self, JitBackend::clear_all)
    }
}

impl JitConfig {
    /// Default number of misses required before compiling bytecode.
    pub const DEFAULT_HOT_THRESHOLD: usize = 8;
    /// Default capacity of the lookup-observation channel.
    pub const DEFAULT_CHANNEL_CAPACITY: usize = 4096;
    /// Default maximum number of pending compilation jobs.
    pub const DEFAULT_MAX_PENDING_JOBS: usize = 2048;
    /// Default maximum bytecode length, where zero disables the limit.
    pub const DEFAULT_MAX_BYTECODE_LEN: usize = 0;
    /// Default resident compiled-code budget of one gibibyte.
    pub const DEFAULT_CODE_CACHE_BYTES: usize = 1024 * 1024 * 1024;
    /// Default idle eviction duration of one hour.
    pub const DEFAULT_IDLE_EVICT_DURATION: Duration = Duration::from_secs(60 * 60);

    /// Creates the shared revmc backend represented by this configuration.
    #[cfg(feature = "jit")]
    pub fn build_backend(&self, dump_dir: Option<PathBuf>) -> revmc::eyre::Result<JitBackend> {
        // revmc sizes a `crossbeam` `ArrayQueue` from this value and panics on zero, even when
        // compilation is disabled, so reject it with an error instead.
        if self.channel_capacity == 0 {
            revmc::eyre::bail!("JIT channel capacity must be at least 1");
        }

        let defaults = RuntimeTuning::default();
        let tuning = RuntimeTuning {
            channel_capacity: self.channel_capacity,
            jit_hot_threshold: self.hot_threshold,
            jit_worker_count: self.worker_count.unwrap_or(defaults.jit_worker_count),
            jit_max_pending_jobs: self.max_pending_jobs,
            jit_max_bytecode_len: self.max_bytecode_len,
            resident_code_cache_bytes: self.code_cache_bytes,
            idle_evict_duration: Some(self.idle_evict_duration),
            ..defaults
        };

        JitBackend::new(RuntimeConfig {
            enabled: self.enabled,
            tuning,
            dump_dir,
            debug_assertions: self.debug,
            blocking: self.blocking,
            jit_mode: JitMode::OutOfProcess,
            ..RuntimeConfig::default()
        })
    }
}

impl Default for JitConfig {
    /// Returns the safe runtime defaults with compilation disabled.
    fn default() -> Self {
        Self {
            enabled: false,
            hot_threshold: Self::DEFAULT_HOT_THRESHOLD,
            worker_count: None,
            channel_capacity: Self::DEFAULT_CHANNEL_CAPACITY,
            max_pending_jobs: Self::DEFAULT_MAX_PENDING_JOBS,
            max_bytecode_len: Self::DEFAULT_MAX_BYTECODE_LEN,
            code_cache_bytes: Self::DEFAULT_CODE_CACHE_BYTES,
            idle_evict_duration: Self::DEFAULT_IDLE_EVICT_DURATION,
            debug: false,
            blocking: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_jit_disabled() {
        let config = JitConfig::default();

        assert!(!config.enabled);
        assert_eq!(config.hot_threshold, JitConfig::DEFAULT_HOT_THRESHOLD);
        assert_eq!(config.code_cache_bytes, JitConfig::DEFAULT_CODE_CACHE_BYTES);
    }

    #[cfg(feature = "jit")]
    #[test]
    fn build_backend_rejects_zero_jit_channel_capacity() {
        let config = JitConfig { channel_capacity: 0, ..JitConfig::default() };

        assert!(config.build_backend(None).is_err());
    }

    #[cfg(feature = "jit")]
    #[test]
    fn jit_controls_error_until_the_backend_is_enabled() {
        let backend = JitBackend::disabled();
        let control: &dyn JitBackendControl = &backend;

        assert!(!control.is_enabled());
        control.pause().expect_err("pause must be rejected while the backend is disabled");
        control.resume().expect_err("resume must be rejected while the backend is disabled");
        control.clear().expect_err("clear must be rejected while the backend is disabled");

        control.set_enabled(true).expect("enabling should spawn the backend thread");
        assert!(control.is_enabled());
        control.pause().expect("pause should reach a running backend");
        control.resume().expect("resume should reach a running backend");
        control.clear().expect("clear should reach a running backend");

        control.set_enabled(false).expect("disabling should succeed");
        assert!(!control.is_enabled());
    }

    #[cfg(feature = "jit")]
    #[test]
    fn jit_controls_never_block_when_the_backend_thread_is_not_running() {
        let backend = JitBackend::disabled();

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            // Far more control commands than the bounded command channel can hold: with no
            // backend thread draining it, blocking sends would stall this loop forever.
            for _ in 0..5_000 {
                let _ = JitBackendControl::pause(&backend);
                let _ = JitBackendControl::resume(&backend);
                let _ = JitBackendControl::clear(&backend);
            }
            let _ = done_tx.send(());
        });

        done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("JIT controls must not block when the backend thread is not running");
        worker.join().expect("control worker should finish cleanly");
    }
}
