use super::bridge::{HostBridge, HostBridgeConfig, HostBridgeError};
use super::config::{RuntimeConfig, DEFAULT_IO_TIMEOUT};
use super::handles::{VirtualHandleId, VirtualHandleTable};
use super::identity::{RuntimeIdentity, RuntimeIdentityState};
use super::profile::{VirtualHidProfile, VirtualHidProfileCatalog};
use super::trace::TraceLimiter;
use crate::protocol::{GuestRuntimeOverrides, LogSeverity};
use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_GET_FEATURE_TIMEOUT: Duration = Duration::from_millis(100);
const DEFAULT_SET_FEATURE_TIMEOUT: Duration = Duration::from_millis(50);
const DEFAULT_FEEDBACK_TIMEOUT: Duration = Duration::from_millis(20);

pub struct GuestDriverRuntime {
    config: RuntimeConfig,
    connection: Mutex<ConnectionState>,
    wake: Condvar,
    handles: Mutex<VirtualHandleTable>,
    trace_limiter: Mutex<TraceLimiter>,
    device_generation: AtomicU32,
}

struct ConnectionState {
    bridge: Option<Arc<HostBridge>>,
    identity: RuntimeIdentity,
    catalog: Option<VirtualHidProfileCatalog>,
    // Retain the worker's reference until all operations release their clones.
    // Only the connection worker may drop the last reference (HostBridge joins).
    retired: Vec<Arc<HostBridge>>,
    worker_running: bool,
    stopping: bool,
    connecting: bool,
    epoch: u64,
    retry: RetrySchedule,
    reconnect_interval: Duration,
    last_error: Option<String>,
}

#[derive(Default)]
struct RetrySchedule {
    completed_at: Option<Instant>,
    interval: Duration,
}

impl RetrySchedule {
    fn remaining(&self, now: Instant) -> Duration {
        self.completed_at
            .map(|completed| {
                self.interval
                    .saturating_sub(now.saturating_duration_since(completed))
            })
            .unwrap_or_default()
    }

    fn failed_at(&mut self, completed: Instant, interval: Duration) {
        self.completed_at = Some(completed);
        self.interval = interval;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuestDriverSnapshot {
    pub identity_state: RuntimeIdentityState,
    pub bridge_connected: bool,
    pub session_trace_id: Option<u32>,
    pub advertised_profiles: usize,
    pub open_handles: usize,
}

impl GuestDriverRuntime {
    pub fn new(config: RuntimeConfig) -> Self {
        let identity = RuntimeIdentity::new(config.allow_debug_fallback());
        let catalog = identity.identity().map(|identity| {
            VirtualHidProfileCatalog::from_identity(identity, config.allow_debug_fallback())
        });
        let trace_limiter = TraceLimiter::new(
            config.trace_reports,
            config.trace_report_limit,
            config.trace_report_max_bytes,
        );

        let reconnect_interval = config.lazy_reconnect_interval;
        Self {
            config,
            connection: Mutex::new(ConnectionState {
                bridge: None,
                identity,
                catalog,
                retired: Vec::new(),
                worker_running: false,
                stopping: false,
                connecting: false,
                epoch: 0,
                retry: RetrySchedule::default(),
                reconnect_interval,
                last_error: None,
            }),
            wake: Condvar::new(),
            handles: Mutex::new(VirtualHandleTable::default()),
            trace_limiter: Mutex::new(trace_limiter),
            device_generation: AtomicU32::new(0),
        }
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    pub fn snapshot(&self) -> GuestDriverSnapshot {
        let state = self.connection.lock().unwrap();
        GuestDriverSnapshot {
            identity_state: state.identity.state(),
            bridge_connected: state.bridge.is_some(),
            session_trace_id: state
                .bridge
                .as_ref()
                .map(|bridge| bridge.info().session_trace_id),
            advertised_profiles: state
                .catalog
                .as_ref()
                .map_or(0, |catalog| catalog.descriptors().len()),
            open_handles: self
                .handles
                .lock()
                .map(|handles| handles.total_open_count())
                .unwrap_or(0),
        }
    }

    pub fn catalog(&self) -> Option<VirtualHidProfileCatalog> {
        self.catalog_result().ok().flatten()
    }

    pub fn session_trace_id(&self) -> Option<u32> {
        self.connection.lock().ok().and_then(|state| {
            state
                .bridge
                .as_ref()
                .map(|bridge| bridge.info().session_trace_id)
        })
    }

    pub fn guest_log_level_override(&self) -> Option<LogSeverity> {
        self.guest_runtime_overrides().log_level
    }

    pub fn guest_runtime_overrides(&self) -> GuestRuntimeOverrides {
        self.connection
            .lock()
            .ok()
            .and_then(|state| {
                state
                    .bridge
                    .as_ref()
                    .map(|bridge| bridge.info().guest_runtime_overrides.clone())
            })
            .unwrap_or_default()
    }

    pub fn catalog_if_connected(&self) -> Option<VirtualHidProfileCatalog> {
        self.catalog()
    }

    /// Enumeration only reads a coherent published snapshot. It never connects
    /// or destroys a bridge, even when a previous session is unhealthy.
    pub fn catalog_result(&self) -> Result<Option<VirtualHidProfileCatalog>, GuestDriverError> {
        let state = self
            .connection
            .lock()
            .map_err(|_| GuestDriverError::StatePoisoned("connection"))?;
        if self.config.host_bridge_required && !Self::state_bridge_healthy(&state) {
            return Ok(None);
        }
        if !state
            .identity
            .can_advertise(self.config.host_bridge_required)
        {
            return Ok(None);
        }
        Ok(state.catalog.clone())
    }

    pub fn can_advertise(&self) -> bool {
        self.catalog().is_some()
    }

    pub fn ensure_connected(&self) -> bool {
        self.ensure_connected_result().unwrap_or(false)
    }

    pub fn ensure_connected_result(&self) -> Result<bool, GuestDriverError> {
        self.wake.notify_one(); // requests coalesce; they cannot bypass the retry deadline
        let state = self
            .connection
            .lock()
            .map_err(|_| GuestDriverError::StatePoisoned("connection"))?;
        Ok(self.config.host_bridge_enabled && Self::state_bridge_healthy(&state))
    }

    /// Compatibility entry point: connection attempts belong exclusively to the worker.
    pub fn connect_bridge(&self) -> Result<(), GuestDriverError> {
        if self.ensure_connected_result()? {
            Ok(())
        } else {
            Err(GuestDriverError::DeviceNotConnected)
        }
    }

    pub fn clear_bridge(&self, _reason: &'static str) {
        if let Ok(mut state) = self.connection.lock() {
            state.epoch = state.epoch.wrapping_add(1);
            self.retire_bridge(&mut state);
        }
        self.wake.notify_one();
    }

    pub fn device_generation(&self) -> u32 {
        self.device_generation.load(Ordering::Acquire)
    }

    fn clear_bridge_if_current(&self, current: &Arc<HostBridge>, _reason: &'static str) {
        if let Ok(mut state) = self.connection.lock() {
            if state
                .bridge
                .as_ref()
                .is_some_and(|bridge| Arc::ptr_eq(bridge, current))
            {
                state.epoch = state.epoch.wrapping_add(1);
                self.retire_bridge(&mut state);
            }
        }
        self.wake.notify_one();
    }

    fn retire_bridge(&self, state: &mut ConnectionState) {
        let removed = state.bridge.take();
        state.identity.mark_stale();
        state.catalog = state.identity.identity().map(|identity| {
            VirtualHidProfileCatalog::from_identity(identity, self.config.allow_debug_fallback())
        });
        self.reset_trace_limiter();
        if let Some(bridge) = removed {
            state.retired.push(bridge);
            self.device_generation.fetch_add(1, Ordering::Release);
        }
    }

    /// Run on an existing attach thread, outside the loader lock. A second call
    /// is a no-op. Disabled bridges never start a connector.
    pub fn run_connection_worker(&self, on_change: impl Fn()) {
        self.run_connection_worker_with(HostBridge::connect, Instant::now, on_change);
    }

    fn run_connection_worker_with(
        &self,
        connect: impl Fn(HostBridgeConfig) -> Result<HostBridge, HostBridgeError>,
        now: impl Fn() -> Instant,
        on_change: impl Fn(),
    ) {
        let mut state = self.connection.lock().unwrap();
        if !self.config.host_bridge_enabled || state.worker_running || state.stopping {
            return;
        }
        state.worker_running = true;
        loop {
            let mut cleanup = Vec::new();
            let mut index = 0;
            while index < state.retired.len() {
                if Arc::strong_count(&state.retired[index]) == 1 {
                    cleanup.push(state.retired.swap_remove(index));
                } else {
                    index += 1;
                }
            }
            if state.stopping {
                self.retire_bridge(&mut state);
                if state.retired.is_empty() {
                    drop(state);
                    drop(cleanup); // join outside shared state lock
                    state = self.connection.lock().unwrap();
                    state.worker_running = false;
                    self.wake.notify_all();
                    return;
                }
            }
            let mut changed = false;
            if state.bridge.is_some() && !Self::state_bridge_healthy(&state) {
                self.retire_bridge(&mut state);
                changed = true;
            }
            let remaining = state.retry.remaining(now());
            if !state.stopping && state.bridge.is_none() && !state.connecting && remaining.is_zero()
            {
                state.connecting = true;
                let epoch = state.epoch;
                drop(state);
                drop(cleanup);
                if changed {
                    on_change();
                }
                let result = connect(self.config.host_bridge_config());
                state = self.connection.lock().unwrap();
                state.connecting = false;
                // A failed attempt's duration must not consume the retry interval.
                let interval = state.reconnect_interval;
                state.retry.failed_at(now(), interval);
                match result {
                    Ok(bridge) => {
                        let bridge = Arc::new(bridge);
                        if state.stopping || state.epoch != epoch {
                            state.retired.push(bridge);
                        } else {
                            let overrides = &bridge.info().guest_runtime_overrides;
                            let allow_fallback =
                                self.allow_debug_fallback_with_overrides(overrides);
                            let catalog = VirtualHidProfileCatalog::from_identity(
                                &bridge.info().identity,
                                allow_fallback,
                            );
                            let mut identity = RuntimeIdentity::new(allow_fallback);
                            identity.set_host_identity(bridge.info().identity.clone());
                            let _ = self.apply_trace_overrides(overrides);
                            state.reconnect_interval =
                                self.reconnect_interval_with_overrides(overrides);
                            state.identity = identity;
                            state.catalog = Some(catalog);
                            state.bridge = Some(bridge);
                            state.retry = RetrySchedule::default();
                            state.last_error = None;
                            self.device_generation.fetch_add(1, Ordering::Release);
                        }
                    }
                    Err(error) => {
                        state.last_error = Some(error.to_string());
                        // Host overrides only survive through the first retry after disconnect.
                        state.reconnect_interval = self.config.lazy_reconnect_interval;
                    }
                }
                drop(state);
                on_change();
                state = self.connection.lock().unwrap();
                continue;
            }
            // Health checks detect disconnect without any SDL or HID polling.
            let wait = if state.bridge.is_some() || state.stopping {
                Duration::from_millis(50)
            } else {
                remaining.min(Duration::from_millis(50))
            };
            drop(state);
            drop(cleanup);
            if changed {
                on_change();
            }
            state = self.connection.lock().unwrap();
            if state.stopping && state.retired.is_empty() {
                continue;
            }
            state = self.wake.wait_timeout(state, wait).unwrap().0;
        }
    }

    pub fn connection_error(&self) -> Option<String> {
        self.connection
            .lock()
            .ok()
            .and_then(|state| state.last_error.clone())
    }

    /// Signal only; safe callers do not wait for handshake or bridge teardown.
    pub fn request_connection_worker_stop(&self) {
        if let Ok(mut state) = self.connection.lock() {
            state.stopping = true;
            state.epoch = state.epoch.wrapping_add(1);
        }
        self.wake.notify_all();
    }

    /// Explicit shutdown outside DllMain. Process termination must never call this.
    pub fn stop_connection_worker(&self) {
        self.request_connection_worker_stop();
        let mut state = self.connection.lock().unwrap();
        while state.worker_running {
            state = self.wake.wait(state).unwrap();
        }
    }

    pub fn open_profile(
        &self,
        profile: VirtualHidProfile,
    ) -> Result<VirtualHandleId, GuestDriverError> {
        if !self.can_advertise() {
            return Err(GuestDriverError::DeviceNotConnected);
        }
        if !self.profile_available(profile) {
            return Err(GuestDriverError::ProfileUnavailable(profile));
        }
        self.handles
            .lock()
            .map(|mut handles| handles.open(profile))
            .map_err(|_| GuestDriverError::StatePoisoned("handles"))
    }

    pub fn close_handle(&self, handle: VirtualHandleId) -> Result<usize, GuestDriverError> {
        self.handles
            .lock()
            .map_err(|_| GuestDriverError::StatePoisoned("handles"))?
            .close(handle)
            .ok_or(GuestDriverError::InvalidHandle)
    }

    pub fn handle_profile(
        &self,
        handle: VirtualHandleId,
    ) -> Result<VirtualHidProfile, GuestDriverError> {
        self.handles
            .lock()
            .map_err(|_| GuestDriverError::StatePoisoned("handles"))?
            .profile(handle)
            .ok_or(GuestDriverError::InvalidHandle)
    }

    pub fn open_count(&self, profile: VirtualHidProfile) -> usize {
        self.handles
            .lock()
            .map(|handles| handles.open_count(profile))
            .unwrap_or(0)
    }

    pub fn is_profile_open(&self, profile: VirtualHidProfile) -> bool {
        self.open_count(profile) > 0
    }

    pub fn copy_next_input_report(
        &self,
        profile: VirtualHidProfile,
        output: &mut [u8],
    ) -> Result<Option<usize>, GuestDriverError> {
        self.with_bridge(profile, "INPUT", |bridge, interface_number| {
            bridge.copy_next_input_report(interface_number, output)
        })
    }

    pub fn copy_feature_report(
        &self,
        profile: VirtualHidProfile,
        report_id: u8,
        output: &mut [u8],
    ) -> Result<usize, GuestDriverError> {
        let timeout_ms = self.operation_timeout_ms(DEFAULT_GET_FEATURE_TIMEOUT);
        self.with_bridge(profile, "GET_FEATURE", |bridge, interface_number| {
            bridge.copy_feature_report(interface_number, report_id, output, timeout_ms)
        })
    }

    pub fn set_feature(
        &self,
        profile: VirtualHidProfile,
        payload: &[u8],
    ) -> Result<u16, GuestDriverError> {
        let timeout_ms = self.operation_timeout_ms(DEFAULT_SET_FEATURE_TIMEOUT);
        self.with_bridge(profile, "SET_FEATURE", |bridge, interface_number| {
            bridge.set_feature(interface_number, payload, timeout_ms)
        })
    }

    pub fn set_output(
        &self,
        profile: VirtualHidProfile,
        payload: &[u8],
    ) -> Result<u16, GuestDriverError> {
        let timeout_ms = self.operation_timeout_ms(DEFAULT_FEEDBACK_TIMEOUT);
        self.with_bridge(profile, "SET_OUTPUT", |bridge, interface_number| {
            bridge.set_output(interface_number, payload, timeout_ms)
        })
    }

    pub fn write_report(
        &self,
        profile: VirtualHidProfile,
        payload: &[u8],
    ) -> Result<u16, GuestDriverError> {
        let timeout_ms = self.operation_timeout_ms(DEFAULT_FEEDBACK_TIMEOUT);
        self.with_bridge(profile, "WRITE", |bridge, interface_number| {
            bridge.write_report(interface_number, payload, timeout_ms)
        })
    }

    pub fn trace_payload(&self, payload: &[u8]) -> Option<String> {
        let mut limiter = self.trace_limiter.lock().ok()?;
        limiter
            .should_trace()
            .then(|| limiter.render_bytes(payload))
    }

    fn profile_available(&self, profile: VirtualHidProfile) -> bool {
        self.connection.lock().ok().is_some_and(|state| {
            state
                .catalog
                .as_ref()
                .is_some_and(|catalog| catalog.descriptor(profile).is_some())
        })
    }

    fn with_bridge<T>(
        &self,
        profile: VirtualHidProfile,
        operation: &'static str,
        call: impl FnOnce(&HostBridge, u8) -> Result<T, HostBridgeError>,
    ) -> Result<T, GuestDriverError> {
        // Route and bridge must come from the same session, even if the worker
        // publishes a different PID/interface immediately after this lock.
        let (bridge, interface_number) = {
            let state = self
                .connection
                .lock()
                .map_err(|_| GuestDriverError::StatePoisoned("connection"))?;
            if !self.config.host_bridge_enabled || !Self::state_bridge_healthy(&state) {
                return Err(GuestDriverError::DeviceNotConnected);
            }
            let interface_number = state
                .catalog
                .as_ref()
                .and_then(|catalog| {
                    catalog
                        .descriptor(profile)
                        .map(|descriptor| descriptor.interface_number)
                })
                .ok_or(GuestDriverError::ProfileUnavailable(profile))?;
            (state.bridge.as_ref().unwrap().clone(), interface_number)
        };
        let result = call(&bridge, interface_number);
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                if error.should_disconnect_bridge() {
                    self.clear_bridge_if_current(&bridge, operation);
                }
                Err(error.into())
            }
        }
    }

    fn operation_timeout_ms(&self, default_timeout: Duration) -> u16 {
        let override_io_timeout = self
            .guest_runtime_overrides()
            .io_timeout_ms
            .map(|millis| Duration::from_millis(u64::from(millis)));
        let timeout = if let Some(timeout) = override_io_timeout {
            timeout
        } else if self.config.io_timeout == DEFAULT_IO_TIMEOUT {
            default_timeout
        } else {
            self.config.io_timeout
        };
        timeout.as_millis().min(u128::from(u16::MAX)) as u16
    }

    fn apply_trace_overrides(
        &self,
        overrides: &GuestRuntimeOverrides,
    ) -> Result<(), GuestDriverError> {
        let trace_reports = overrides.trace_reports.unwrap_or(self.config.trace_reports);
        let trace_report_limit = overrides
            .trace_report_limit
            .map(|value| value as usize)
            .unwrap_or(self.config.trace_report_limit);
        let trace_report_max_bytes = overrides
            .trace_report_max_bytes
            .map(usize::from)
            .unwrap_or(self.config.trace_report_max_bytes);
        *self
            .trace_limiter
            .lock()
            .map_err(|_| GuestDriverError::StatePoisoned("trace_limiter"))? =
            TraceLimiter::new(trace_reports, trace_report_limit, trace_report_max_bytes);
        Ok(())
    }

    fn allow_debug_fallback_with_overrides(&self, overrides: &GuestRuntimeOverrides) -> bool {
        !self.config.host_bridge_required
            && overrides
                .replay_enabled
                .unwrap_or(self.config.replay_enabled)
    }

    fn reconnect_interval_with_overrides(&self, overrides: &GuestRuntimeOverrides) -> Duration {
        overrides
            .reconnect_interval_ms
            .map(|millis| Duration::from_millis(u64::from(millis)))
            .unwrap_or(self.config.lazy_reconnect_interval)
    }

    fn reset_trace_limiter(&self) {
        if let Ok(mut trace_limiter) = self.trace_limiter.lock() {
            *trace_limiter = TraceLimiter::new(
                self.config.trace_reports,
                self.config.trace_report_limit,
                self.config.trace_report_max_bytes,
            );
        }
    }

    fn state_bridge_healthy(state: &ConnectionState) -> bool {
        state.bridge.as_ref().is_some_and(|bridge| {
            let stats = bridge.input_stats();
            stats.read_errors == 0 && stats.command_errors == 0
        })
    }
}

#[derive(Debug)]
pub enum GuestDriverError {
    HostBridge(HostBridgeError),
    DeviceNotConnected,
    InvalidHandle,
    ProfileUnavailable(VirtualHidProfile),
    StatePoisoned(&'static str),
}

impl fmt::Display for GuestDriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostBridge(error) => write!(f, "{error}"),
            Self::DeviceNotConnected => f.write_str("host bridge is not connected"),
            Self::InvalidHandle => f.write_str("invalid virtual HID handle"),
            Self::ProfileUnavailable(profile) => {
                write!(f, "virtual HID profile is unavailable: {}", profile.label())
            }
            Self::StatePoisoned(name) => write!(f, "guest driver state lock poisoned: {name}"),
        }
    }
}

impl std::error::Error for GuestDriverError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::HostBridge(error) => Some(error),
            _ => None,
        }
    }
}

impl From<HostBridgeError> for GuestDriverError {
    fn from(value: HostBridgeError) -> Self {
        Self::HostBridge(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guest_driver::config::DEFAULT_LAZY_RECONNECT_INTERVAL;
    use crate::guest_driver::identity::default_fallback_identity;
    use crate::protocol::{
        CollectionRole, Frame, GuestRuntimeOverrides, HelloOk, InputAttach, InputAttachOk,
        InputReport, MessageType, StatusCode, WireDecode, WirePayload, WriteReport, WriteResult,
    };
    use crate::transport::{ChannelStream, TransportAddrs, TransportListeners};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::{Duration, Instant};

    struct TestWorker {
        runtime: Arc<GuestDriverRuntime>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl TestWorker {
        fn start(runtime: Arc<GuestDriverRuntime>) -> Self {
            let worker_runtime = runtime.clone();
            let thread = thread::spawn(move || worker_runtime.run_connection_worker(|| {}));
            Self {
                runtime,
                thread: Some(thread),
            }
        }
    }

    impl Drop for TestWorker {
        fn drop(&mut self) {
            self.runtime.stop_connection_worker();
            self.thread.take().unwrap().join().unwrap();
        }
    }

    fn wait_until(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(Instant::now() < deadline, "worker did not publish state");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn catalog_query_does_not_wait_for_unresponsive_host() {
        let listeners = TransportListeners::bind(TransportAddrs::loopback(0, 0)).unwrap();
        let runtime = Arc::new(GuestDriverRuntime::new(RuntimeConfig {
            addrs: listeners.local_addrs().unwrap(),
            ..RuntimeConfig::driver_defaults()
        }));
        let _worker = TestWorker::start(runtime.clone());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut control = listeners.accept_control().unwrap();
            control.read_frame().unwrap();
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        let (result_tx, result_rx) = mpsc::channel();
        let caller_runtime = runtime.clone();
        let caller = thread::spawn(move || {
            result_tx.send(caller_runtime.catalog_result()).unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // The server cannot finish the handshake until after the query returns.
        // This timeout is a deadlock watchdog, not a millisecond performance budget.
        let result = result_rx.recv_timeout(Duration::from_secs(1));
        release_tx.send(()).unwrap();
        caller.join().unwrap();
        server.join().unwrap();
        assert!(
            matches!(result, Ok(Ok(None))),
            "query waited for handshake: {result:?}"
        );
    }

    #[test]
    fn required_mode_starts_without_advertised_profiles() {
        let runtime = GuestDriverRuntime::new(RuntimeConfig {
            host_bridge_required: true,
            ..RuntimeConfig::default()
        });

        assert!(!runtime.can_advertise());
        assert!(runtime.catalog().is_none());
    }

    #[test]
    fn debug_fallback_can_open_profile_without_bridge() {
        let runtime = GuestDriverRuntime::new(RuntimeConfig {
            replay_enabled: true,
            ..RuntimeConfig::default()
        });

        let handle = runtime.open_profile(VirtualHidProfile::Main).unwrap();

        assert_eq!(
            runtime.handle_profile(handle).unwrap(),
            VirtualHidProfile::Main
        );
        assert!(runtime.is_profile_open(VirtualHidProfile::Main));
        assert_eq!(runtime.open_count(VirtualHidProfile::Main), 1);
        assert_eq!(runtime.close_handle(handle).unwrap(), 0);
        assert!(!runtime.is_profile_open(VirtualHidProfile::Main));
    }

    #[test]
    fn catalog_uses_fallback_identity_in_debug_mode() {
        let runtime = GuestDriverRuntime::new(RuntimeConfig {
            replay_enabled: true,
            ..RuntimeConfig::default()
        });
        let expected = default_fallback_identity();

        assert_eq!(
            runtime.catalog().unwrap().identity().serial,
            expected.serial
        );
    }

    #[test]
    fn stale_bridge_clears_required_catalog() {
        let runtime = GuestDriverRuntime::new(RuntimeConfig::default());
        {
            let mut state = runtime.connection.lock().unwrap();
            state
                .identity
                .set_host_identity(default_fallback_identity());
        }
        {
            let mut state = runtime.connection.lock().unwrap();
            state.catalog = Some(VirtualHidProfileCatalog::from_identity(
                &default_fallback_identity(),
                false,
            ));
        }

        runtime.clear_bridge("test");

        assert!(runtime.catalog().is_none());
        assert_eq!(runtime.device_generation(), 0);
        assert!(!runtime.can_advertise());
    }

    #[test]
    fn command_without_catalog_reports_disconnected_before_profile_unavailable() {
        let runtime = GuestDriverRuntime::new(RuntimeConfig {
            host_bridge_required: true,
            ..RuntimeConfig::default()
        });

        let result = runtime.set_feature(VirtualHidProfile::Main, &[0x00]);

        assert!(matches!(result, Err(GuestDriverError::DeviceNotConnected)));
    }

    #[test]
    fn stale_bridge_uses_debug_fallback_catalog_when_allowed() {
        let mut host_identity = default_fallback_identity();
        host_identity.serial = "HOSTSERIAL".to_string();
        let runtime = GuestDriverRuntime::new(RuntimeConfig {
            replay_enabled: true,
            ..RuntimeConfig::default()
        });
        {
            let mut state = runtime.connection.lock().unwrap();
            state.identity.set_host_identity(host_identity.clone());
        }
        {
            let mut state = runtime.connection.lock().unwrap();
            state.catalog = Some(VirtualHidProfileCatalog::from_identity(
                &host_identity,
                true,
            ));
        }

        runtime.clear_bridge("test");

        assert_eq!(
            runtime.catalog().unwrap().identity().serial,
            default_fallback_identity().serial
        );
        assert!(runtime.can_advertise());
    }

    #[test]
    fn operation_timeouts_follow_low_latency_defaults_unless_overridden() {
        let runtime = GuestDriverRuntime::new(RuntimeConfig::default());

        assert_eq!(
            runtime.operation_timeout_ms(DEFAULT_GET_FEATURE_TIMEOUT),
            100
        );
        assert_eq!(
            runtime.operation_timeout_ms(DEFAULT_SET_FEATURE_TIMEOUT),
            50
        );
        assert_eq!(runtime.operation_timeout_ms(DEFAULT_FEEDBACK_TIMEOUT), 20);

        let runtime = GuestDriverRuntime::new(RuntimeConfig {
            io_timeout: Duration::from_millis(75),
            ..RuntimeConfig::default()
        });

        assert_eq!(
            runtime.operation_timeout_ms(DEFAULT_GET_FEATURE_TIMEOUT),
            75
        );
        assert_eq!(
            runtime.operation_timeout_ms(DEFAULT_SET_FEATURE_TIMEOUT),
            75
        );
        assert_eq!(runtime.operation_timeout_ms(DEFAULT_FEEDBACK_TIMEOUT), 75);
    }

    #[test]
    fn host_replay_override_is_session_scoped_and_respects_required_mode() {
        let runtime = GuestDriverRuntime::new(RuntimeConfig::default());

        assert!(!runtime.allow_debug_fallback_with_overrides(&GuestRuntimeOverrides::default()));
        assert!(
            runtime.allow_debug_fallback_with_overrides(&GuestRuntimeOverrides {
                replay_enabled: Some(true),
                ..GuestRuntimeOverrides::default()
            })
        );

        let required_runtime = GuestDriverRuntime::new(RuntimeConfig {
            host_bridge_required: true,
            ..RuntimeConfig::default()
        });
        assert!(
            !required_runtime.allow_debug_fallback_with_overrides(&GuestRuntimeOverrides {
                replay_enabled: Some(true),
                ..GuestRuntimeOverrides::default()
            })
        );
    }

    #[test]
    fn host_reconnect_interval_override_is_session_scoped() {
        let runtime = GuestDriverRuntime::new(RuntimeConfig::default());

        assert_eq!(
            runtime.reconnect_interval_with_overrides(&GuestRuntimeOverrides::default()),
            DEFAULT_LAZY_RECONNECT_INTERVAL
        );
        assert_eq!(
            runtime.reconnect_interval_with_overrides(&GuestRuntimeOverrides {
                reconnect_interval_ms: Some(250),
                ..GuestRuntimeOverrides::default()
            }),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn host_runtime_overrides_apply_for_session_and_reset_on_disconnect() {
        let listeners = TransportListeners::bind(TransportAddrs::loopback(0, 0)).unwrap();
        let addrs = listeners.local_addrs().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();

        let server = thread::spawn(move || {
            ready_tx.send(()).unwrap();
            let mut control = listeners.accept_control().unwrap();
            control
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            control
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let hello = control.read_frame().unwrap();
            assert_eq!(hello.header.message_type, MessageType::Hello);
            write_test_payload(
                &mut control,
                hello.header.id,
                &HelloOk::success_with_trace_and_overrides(
                    0xCAFE_BABE,
                    0x12345,
                    54,
                    GuestRuntimeOverrides {
                        trace_reports: Some(true),
                        trace_report_limit: Some(1),
                        trace_report_max_bytes: Some(2),
                        io_timeout_ms: Some(75),
                        ..GuestRuntimeOverrides::default()
                    },
                ),
            );
            write_test_payload(&mut control, 0, &default_fallback_identity());

            let mut input = listeners.accept_input().unwrap();
            input
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            input
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let attach = input.read_frame().unwrap();
            assert_eq!(attach.header.message_type, MessageType::InputAttach);
            write_test_payload(
                &mut input,
                attach.header.id,
                &InputAttachOk {
                    status: StatusCode::Ok,
                    input_report_len: 54,
                    first_input_seq: 1,
                },
            );
        });

        ready_rx.recv().unwrap();
        let runtime = GuestDriverRuntime::new(RuntimeConfig {
            addrs,
            host_bridge_enabled: true,
            host_bridge_required: true,
            ..RuntimeConfig::default()
        });
        let runtime = Arc::new(runtime);
        let _worker = TestWorker::start(runtime.clone());
        wait_until(|| runtime.device_generation() == 1);

        assert_eq!(runtime.device_generation(), 1);
        assert_eq!(
            runtime.operation_timeout_ms(DEFAULT_GET_FEATURE_TIMEOUT),
            75
        );
        assert_eq!(
            runtime.trace_payload(&[0x01, 0x02, 0x03, 0x04]).as_deref(),
            Some("01 02 ...(+2 bytes)")
        );
        assert_eq!(runtime.trace_payload(&[0x05]).as_deref(), None);

        runtime.clear_bridge("test");

        assert_eq!(runtime.device_generation(), 2);
        assert_eq!(
            runtime.operation_timeout_ms(DEFAULT_GET_FEATURE_TIMEOUT),
            100
        );
        assert_eq!(runtime.trace_payload(&[0x01]).as_deref(), None);
        assert!(runtime.guest_runtime_overrides().is_empty());
        server.join().unwrap();
    }

    #[test]
    fn input_pop_is_not_blocked_by_pending_feedback_command() {
        let listeners = TransportListeners::bind(TransportAddrs::loopback(0, 0)).unwrap();
        let addrs = listeners.local_addrs().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (write_received_tx, write_received_rx) = mpsc::channel();

        let server = thread::spawn(move || {
            ready_tx.send(()).unwrap();
            let mut control = listeners.accept_control().unwrap();
            control
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            control
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let hello = control.read_frame().unwrap();
            assert_eq!(hello.header.message_type, MessageType::Hello);
            write_test_payload(
                &mut control,
                hello.header.id,
                &HelloOk::success(0xCAFE_BABE, 54),
            );
            write_test_payload(&mut control, 0, &default_fallback_identity());

            let mut input = listeners.accept_input().unwrap();
            input
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            input
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let attach = input.read_frame().unwrap();
            assert_eq!(attach.header.message_type, MessageType::InputAttach);
            let attach_payload = InputAttach::decode(&attach.payload).unwrap();
            assert_eq!(attach_payload.session_id, 0xCAFE_BABE);
            write_test_payload(
                &mut input,
                attach.header.id,
                &InputAttachOk {
                    status: StatusCode::Ok,
                    input_report_len: 54,
                    first_input_seq: 1,
                },
            );

            let write = control.read_frame().unwrap();
            assert_eq!(write.header.message_type, MessageType::Write);
            let write_payload = WriteReport::decode(&write.payload).unwrap();
            write_received_tx.send(()).unwrap();
            write_test_payload(
                &mut input,
                1,
                &InputReport {
                    interface_number: 2,
                    role: CollectionRole::PuckMain,
                    host_monotonic_us: 42,
                    data: vec![0x79, 0x02],
                },
            );
            thread::sleep(Duration::from_millis(120));
            write_test_payload(
                &mut control,
                write.header.id,
                &WriteResult {
                    status: StatusCode::Ok,
                    bytes_written: write_payload.data.len() as u16,
                    os_error: 0,
                },
            );
        });

        ready_rx.recv().unwrap();
        let runtime = Arc::new(GuestDriverRuntime::new(RuntimeConfig {
            addrs,
            host_bridge_enabled: true,
            host_bridge_required: true,
            io_timeout: Duration::from_millis(250),
            ..RuntimeConfig::default()
        }));
        let _worker = TestWorker::start(runtime.clone());
        wait_until(|| runtime.device_generation() == 1);

        let writer_runtime = Arc::clone(&runtime);
        let writer = thread::spawn(move || {
            writer_runtime
                .write_report(VirtualHidProfile::Main, &[0x80, 0x00])
                .unwrap()
        });
        write_received_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();

        let mut output = [0_u8; 64];
        let started = Instant::now();
        let count = loop {
            if let Some(count) = runtime
                .copy_next_input_report(VirtualHidProfile::Main, &mut output)
                .unwrap()
            {
                break count;
            }
            assert!(
                started.elapsed() < Duration::from_millis(80),
                "input report was blocked behind pending feedback command"
            );
            thread::sleep(Duration::from_millis(1));
        };

        assert_eq!(count, 2);
        assert_eq!(&output[..2], &[0x79, 0x02]);
        assert_eq!(writer.join().unwrap(), 2);
        server.join().unwrap();
    }

    fn write_test_payload<T: WirePayload>(stream: &mut ChannelStream, id: u32, payload: &T) {
        stream
            .write_frame(&Frame::new(
                T::MESSAGE_TYPE,
                id,
                payload.to_bytes().unwrap(),
            ))
            .unwrap();
    }
}

#[cfg(test)]
#[path = "runtime_connectivity_tests.rs"]
mod connectivity_tests;
