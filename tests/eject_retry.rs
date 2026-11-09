use std::cell::{Cell, RefCell};
use std::time::Duration;

use usb_disk_ejector::{
    backend::{BackendError, Cancellation, EjectFailure, EjectResult, VetoKind},
    eject_retry::request_with_retry,
};

fn veto(config_ret: u32, kind: VetoKind, name: &str) -> EjectResult {
    EjectResult::Rejected(EjectFailure {
        config_ret,
        veto: Some((kind, name.into())),
    })
}

#[test]
fn transient_vetoes_retry_with_fresh_validation_then_stop_on_success() {
    let validations = Cell::new(0);
    let calls = Cell::new(0);
    let waits = RefCell::new(Vec::new());
    let result = request_with_retry(
        &Cancellation::new(),
        || {
            validations.set(validations.get() + 1);
            Ok(validations.get())
        },
        |target| {
            calls.set(calls.get() + 1);
            assert_eq!(target, calls.get());
            Ok(match calls.get() {
                1 => veto(23, VetoKind::PendingClose, "volume-a"),
                2 => veto(23, VetoKind::OutstandingOpen, "volume-b"),
                _ => EjectResult::Ejected,
            })
        },
        |duration| waits.borrow_mut().push(duration),
    );
    assert_eq!(result, Ok(EjectResult::Ejected));
    assert_eq!(validations.get(), 3);
    assert_eq!(calls.get(), 3);
    assert_eq!(*waits.borrow(), vec![Duration::from_millis(500); 2]);
}

#[test]
fn success_and_nontransient_failures_are_never_retried() {
    let mut outcomes = vec![
        EjectResult::Ejected,
        veto(51, VetoKind::OutstandingOpen, "wrong-configret"),
        veto(51, VetoKind::PendingClose, "wrong-configret"),
        EjectResult::Rejected(EjectFailure {
            config_ret: 23,
            veto: None,
        }),
    ];
    for kind in [
        VetoKind::Unknown,
        VetoKind::LegacyDevice,
        VetoKind::WindowsApp,
        VetoKind::WindowsService,
        VetoKind::Device,
        VetoKind::Driver,
        VetoKind::IllegalDeviceRequest,
        VetoKind::InsufficientPower,
        VetoKind::NonDisableable,
        VetoKind::LegacyDriver,
        VetoKind::InsufficientRights,
    ] {
        outcomes.push(veto(23, kind, "permanent-veto"));
    }
    for outcome in outcomes {
        let calls = Cell::new(0);
        let result = request_with_retry(
            &Cancellation::new(),
            || Ok(()),
            |_| {
                calls.set(calls.get() + 1);
                Ok(outcome.clone())
            },
            |_| panic!("nontransient outcomes must not wait"),
        );
        assert_eq!(result, Ok(outcome));
        assert_eq!(calls.get(), 1);
    }
}

#[test]
fn third_transient_veto_is_returned_unchanged() {
    let calls = Cell::new(0);
    let waits = Cell::new(0);
    let final_veto = veto(23, VetoKind::OutstandingOpen, r"STORAGE\Volume\last");
    let result = request_with_retry(
        &Cancellation::new(),
        || Ok(()),
        |_| {
            calls.set(calls.get() + 1);
            Ok(if calls.get() == 3 {
                final_veto.clone()
            } else {
                veto(23, VetoKind::PendingClose, "earlier-veto")
            })
        },
        |_| waits.set(waits.get() + 1),
    );
    assert_eq!(result, Ok(final_veto));
    assert_eq!(calls.get(), 3);
    assert_eq!(waits.get(), 2);
}

#[test]
fn cancellation_during_wait_prevents_revalidation_and_another_request() {
    let cancellation = Cancellation::new();
    let validations = Cell::new(0);
    let calls = Cell::new(0);
    let result = request_with_retry(
        &cancellation,
        || {
            validations.set(validations.get() + 1);
            Ok(())
        },
        |_| {
            calls.set(calls.get() + 1);
            Ok(veto(23, VetoKind::OutstandingOpen, "busy-volume"))
        },
        |_| cancellation.cancel(),
    );
    assert!(result.unwrap_err().0.contains("取消"));
    assert_eq!(validations.get(), 1);
    assert_eq!(calls.get(), 1);
}

#[test]
fn changed_identity_after_wait_prevents_another_request() {
    let identity_changed = Cell::new(false);
    let calls = Cell::new(0);
    let expected_error = BackendError("device identity changed".into());
    let result = request_with_retry(
        &Cancellation::new(),
        || {
            if identity_changed.get() {
                Err(expected_error.clone())
            } else {
                Ok(())
            }
        },
        |_| {
            calls.set(calls.get() + 1);
            Ok(veto(23, VetoKind::PendingClose, "busy-volume"))
        },
        |_| identity_changed.set(true),
    );
    assert_eq!(result, Err(expected_error));
    assert_eq!(calls.get(), 1);
}

#[test]
fn cancellation_during_validation_prevents_the_first_request() {
    let cancellation = Cancellation::new();
    let result = request_with_retry(
        &cancellation,
        || {
            cancellation.cancel();
            Ok(())
        },
        |_| panic!("cancellation must prevent the PnP request"),
        |_| panic!("cancellation must not wait"),
    );
    assert!(result.unwrap_err().0.contains("取消"));
}

#[test]
fn request_errors_are_returned_without_retry() {
    let expected_error = BackendError("request failed".into());
    let result = request_with_retry(
        &Cancellation::new(),
        || Ok(()),
        |_| Err(expected_error.clone()),
        |_| panic!("backend errors must not wait"),
    );
    assert_eq!(result, Err(expected_error));
}
