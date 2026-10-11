use super::*;
use crate::guest_driver::identity::default_fallback_identity;
use crate::protocol::{Frame, HelloOk, IdentityPayload, InputAttachOk, StatusCode, WirePayload};
use crate::transport::{ChannelStream, TransportAddrs, TransportListeners};
use std::sync::{atomic::AtomicUsize, mpsc};
use std::thread::{self, JoinHandle};

struct Worker {
    runtime: Arc<GuestDriverRuntime>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn start(
        runtime: Arc<GuestDriverRuntime>,
        connect: impl Fn(HostBridgeConfig) -> Result<HostBridge, HostBridgeError> + Send + 'static,
    ) -> Self {
        let worker_runtime = runtime.clone();
        let thread = thread::spawn(move || {
            worker_runtime.run_connection_worker_with(connect, Instant::now, || {})
        });
        Self {
            runtime,
            thread: Some(thread),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.runtime.stop_connection_worker();
        self.thread.take().unwrap().join().unwrap();
    }
}

fn wait_until(predicate: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "worker state did not arrive");
        thread::sleep(Duration::from_millis(1));
    }
}

fn write<T: WirePayload>(stream: &mut ChannelStream, id: u32, payload: &T) {
    stream
        .write_frame(&Frame::new(
            T::MESSAGE_TYPE,
            id,
            payload.to_bytes().unwrap(),
        ))
        .unwrap();
}

fn serve(listeners: &TransportListeners, identity: &IdentityPayload, close: &mpsc::Receiver<()>) {
    let mut control = listeners.accept_control().unwrap();
    control
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let hello = control.read_frame().unwrap();
    write(
        &mut control,
        hello.header.id,
        &HelloOk::success(42, identity.default_input_report_len()),
    );
    write(&mut control, 0, identity);
    let mut input = listeners.accept_input().unwrap();
    input
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let attach = input.read_frame().unwrap();
    write(
        &mut input,
        attach.header.id,
        &InputAttachOk {
            status: StatusCode::Ok,
            input_report_len: identity.default_input_report_len(),
            first_input_seq: 1,
        },
    );
    close.recv_timeout(Duration::from_secs(5)).unwrap();
}

fn identity(pid: u16, serial: &str) -> IdentityPayload {
    let mut identity = default_fallback_identity();
    identity.product_id = pid;
    identity.serial = serial.into();
    if pid != 0x1304 {
        identity
            .collections
            .retain(|collection| collection.role == crate::protocol::CollectionRole::PuckMain);
        identity.collections[0].interface_number = 0;
        identity.collections[0].input_report_len = 46;
    }
    identity
}

#[test]
fn retry_deadline_uses_failure_completion_and_fake_clock_boundaries() {
    let started = Instant::now();
    let completed = started + Duration::from_secs(10); // handshake outlasted retry interval
    let mut retry = RetrySchedule::default();
    retry.failed_at(completed, Duration::from_secs(1));
    assert_eq!(retry.remaining(completed), Duration::from_secs(1));
    assert_eq!(
        retry.remaining(completed + Duration::from_millis(999)),
        Duration::from_millis(1)
    );
    assert_eq!(
        retry.remaining(completed + Duration::from_secs(1)),
        Duration::ZERO
    );
}

#[test]
fn concurrent_queries_coalesce_while_one_connector_is_blocked() {
    let runtime = Arc::new(GuestDriverRuntime::new(RuntimeConfig::driver_defaults()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let worker = Worker::start(runtime.clone(), move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        entered_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        Err(HostBridgeError::CommandWorkerStopped)
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let callers: Vec<_> = (0..8)
        .map(|_| {
            let runtime = runtime.clone();
            thread::spawn(move || {
                for _ in 0..100 {
                    assert!(runtime.catalog_result().unwrap().is_none());
                    assert!(!runtime.can_advertise());
                    assert!(!runtime.ensure_connected_result().unwrap());
                    assert!(runtime.connect_bridge().is_err());
                    assert_eq!(runtime.device_generation(), 0);
                }
            })
        })
        .collect();
    for caller in callers {
        caller.join().unwrap();
    }
    runtime.run_connection_worker_with(
        |_| panic!("duplicate worker connected"),
        Instant::now,
        || {},
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    runtime.request_connection_worker_stop();
    release_tx.send(()).unwrap();
    drop(worker);
    assert_eq!(runtime.device_generation(), 0);
}

#[test]
fn disabled_bridge_never_runs_connector_and_keeps_debug_fallback() {
    let runtime = GuestDriverRuntime::new(RuntimeConfig {
        replay_enabled: true,
        ..RuntimeConfig::default()
    });
    runtime.run_connection_worker_with(
        |_| panic!("disabled bridge connected"),
        Instant::now,
        || {},
    );
    assert!(!runtime.connection.lock().unwrap().worker_running);
    assert!(runtime.can_advertise());
    assert_eq!(runtime.device_generation(), 0);
}

#[test]
fn host_late_start_connects_without_enumeration_request() {
    let listeners = TransportListeners::bind(TransportAddrs::loopback(0, 0)).unwrap();
    let runtime = Arc::new(GuestDriverRuntime::new(RuntimeConfig {
        addrs: listeners.local_addrs().unwrap(),
        lazy_reconnect_interval: Duration::from_millis(1),
        ..RuntimeConfig::driver_defaults()
    }));
    let available = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let host_available = available.clone();
    let (attempted_tx, attempted_rx) = mpsc::channel();
    let worker = Worker::start(runtime.clone(), move |config| {
        if !host_available.load(Ordering::Acquire) {
            let _ = attempted_tx.send(());
            return Err(HostBridgeError::CommandWorkerStopped);
        }
        HostBridge::connect(config)
    });
    attempted_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(runtime.device_generation(), 0);
    let expected = identity(0x1302, "late-usb");
    let host_identity = expected.clone();
    let (close_tx, close_rx) = mpsc::channel();
    let server = thread::spawn(move || serve(&listeners, &host_identity, &close_rx));
    available.store(true, Ordering::Release);
    // Observe only generation; no catalog/connect call drives the worker.
    wait_until(|| runtime.device_generation() == 1);
    assert_eq!(runtime.catalog().unwrap().identity(), &expected);
    drop(worker);
    close_tx.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn disconnect_and_reconnect_publish_consistent_identity_and_wrapping_generation() {
    let listeners = TransportListeners::bind(TransportAddrs::loopback(0, 0)).unwrap();
    let runtime = Arc::new(GuestDriverRuntime::new(RuntimeConfig {
        addrs: listeners.local_addrs().unwrap(),
        ..RuntimeConfig::driver_defaults()
    }));
    runtime.device_generation.store(u32::MAX, Ordering::Release);
    let (close_tx, close_rx) = mpsc::channel();
    let usb = identity(0x1302, "usb");
    let bluetooth = identity(0x1303, "bluetooth");
    let first = usb.clone();
    let second = bluetooth.clone();
    let server = thread::spawn(move || {
        serve(&listeners, &first, &close_rx);
        serve(&listeners, &second, &close_rx);
    });
    let (pending_tx, pending_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let attempts = AtomicUsize::new(0);
    let worker = Worker::start(runtime.clone(), move |config| {
        if attempts.fetch_add(1, Ordering::SeqCst) == 1 {
            pending_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
        }
        HostBridge::connect(config)
    });
    wait_until(|| runtime.device_generation() == 0);
    assert_eq!(runtime.catalog().unwrap().identity(), &usb);
    close_tx.send(()).unwrap();
    pending_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(runtime.device_generation(), 1);
    assert!(runtime.catalog().is_none());
    assert!(!runtime.snapshot().bridge_connected);
    resume_tx.send(()).unwrap();
    wait_until(|| runtime.device_generation() == 2);
    assert_eq!(runtime.catalog().unwrap().identity(), &bluetooth);
    for _ in 0..100 {
        assert!(runtime.ensure_connected());
    }
    assert_eq!(runtime.device_generation(), 2);
    drop(worker);
    assert_eq!(runtime.device_generation(), 3);
    close_tx.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn late_success_is_discarded_on_stop_or_epoch_invalidation() {
    for stop in [false, true] {
        let listeners = TransportListeners::bind(TransportAddrs::loopback(0, 0)).unwrap();
        let runtime = Arc::new(GuestDriverRuntime::new(RuntimeConfig {
            addrs: listeners.local_addrs().unwrap(),
            lazy_reconnect_interval: Duration::from_secs(60),
            ..RuntimeConfig::driver_defaults()
        }));
        let (close_tx, close_rx) = mpsc::channel();
        let server =
            thread::spawn(move || serve(&listeners, &identity(0x1304, "obsolete"), &close_rx));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = Worker::start(runtime.clone(), move |config| {
            let bridge = HostBridge::connect(config)?;
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(bridge)
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if stop {
            runtime.request_connection_worker_stop();
        } else {
            runtime.clear_bridge("invalidate pending connect");
        }
        release_tx.send(()).unwrap();
        wait_until(|| !runtime.connection.lock().unwrap().connecting);
        assert_eq!(runtime.device_generation(), 0);
        assert!(runtime.catalog().is_none());
        drop(worker);
        close_tx.send(()).unwrap();
        server.join().unwrap();
    }
}

#[test]
fn retired_bridge_last_reference_is_kept_off_query_threads() {
    let listeners = TransportListeners::bind(TransportAddrs::loopback(0, 0)).unwrap();
    let runtime = Arc::new(GuestDriverRuntime::new(RuntimeConfig {
        addrs: listeners.local_addrs().unwrap(),
        ..RuntimeConfig::driver_defaults()
    }));
    let (close_tx, close_rx) = mpsc::channel();
    let server = thread::spawn(move || serve(&listeners, &identity(0x1304, "held"), &close_rx));
    let attempts = AtomicUsize::new(0);
    let worker = Worker::start(runtime.clone(), move |config| {
        if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            HostBridge::connect(config)
        } else {
            Err(HostBridgeError::CommandWorkerStopped)
        }
    });
    wait_until(|| runtime.device_generation() == 1);
    let operation_reference = runtime
        .connection
        .lock()
        .unwrap()
        .bridge
        .as_ref()
        .unwrap()
        .clone();
    runtime.clear_bridge("test held operation");
    wait_until(|| runtime.connection.lock().unwrap().retired.len() == 1);
    assert!(Arc::strong_count(&operation_reference) >= 2);
    assert_eq!(runtime.device_generation(), 2);
    assert!(runtime.catalog().is_none());
    assert_eq!(10_u32.wrapping_add(runtime.device_generation()), 12);
    drop(operation_reference); // worker still owns it; this cannot join
    wait_until(|| runtime.connection.lock().unwrap().retired.is_empty());
    drop(worker);
    close_tx.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn worker_retry_requests_obey_fake_clock_and_session_interval_override() {
    let runtime = Arc::new(GuestDriverRuntime::new(RuntimeConfig {
        lazy_reconnect_interval: Duration::from_secs(1),
        ..RuntimeConfig::driver_defaults()
    }));
    // Model the session-scoped interval carried by the last connected host.
    runtime.connection.lock().unwrap().reconnect_interval = runtime
        .reconnect_interval_with_overrides(&GuestRuntimeOverrides {
            reconnect_interval_ms: Some(250),
            ..GuestRuntimeOverrides::default()
        });
    let tick = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::new(AtomicUsize::new(0));
    let clock_tick = tick.clone();
    let connector_tick = tick.clone();
    let count = attempts.clone();
    let (check_tx, check_rx) = mpsc::channel();
    let (completed_tx, completed_rx) = mpsc::channel();
    let worker_runtime = runtime.clone();
    let base = Instant::now();
    let worker = Worker {
        runtime: runtime.clone(),
        thread: Some(thread::spawn(move || {
            worker_runtime.run_connection_worker_with(
                move |_| {
                    if count.fetch_add(1, Ordering::SeqCst) == 0 {
                        connector_tick.store(2000, Ordering::Release); // delayed handshake failure
                    }
                    Err(HostBridgeError::CommandWorkerStopped)
                },
                move || {
                    let value = clock_tick.load(Ordering::Acquire);
                    let _ = check_tx.send(value);
                    base + Duration::from_millis(value as u64)
                },
                move || {
                    let _ = completed_tx.send(());
                },
            )
        })),
    };
    completed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    {
        let state = runtime.connection.lock().unwrap();
        assert_eq!(
            state.retry.remaining(base + Duration::from_millis(2000)),
            Duration::from_millis(250)
        );
        assert_eq!(state.reconnect_interval, Duration::from_secs(1));
    }
    tick.store(2249, Ordering::Release);
    for _ in 0..100 {
        assert!(!runtime.ensure_connected());
    }
    while check_rx.recv_timeout(Duration::from_secs(5)).unwrap() != 2249 {}
    {
        let state = runtime.connection.lock().unwrap();
        assert!(!state.connecting);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
    tick.store(2250, Ordering::Release);
    runtime.wake.notify_one();
    completed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        runtime
            .connection
            .lock()
            .unwrap()
            .retry
            .remaining(base + Duration::from_millis(2250)),
        Duration::from_secs(1)
    );
    drop(worker);
}
