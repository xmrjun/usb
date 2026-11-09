use std::collections::BTreeSet;

use usb_disk_ejector::{
    backend::{BackendError, Cancellation},
    model::Volume,
    volume_prepare::{prepare_volumes, VolumePreparation},
};

fn volume(id: &str) -> Volume {
    Volume {
        id: id.into(),
        mount_paths: Vec::new(),
        capacity_bytes: Some(1_000_000),
        filesystem: Some("exFAT".into()),
        disk_numbers: vec![5],
        extents: Vec::new(),
        mapping_complete: true,
    }
}

#[derive(Default)]
struct FakePreparation {
    events: Vec<String>,
    live: BTreeSet<String>,
    fail_on: Vec<String>,
    cancel_on: Option<String>,
    cancellation: Cancellation,
    validations: usize,
}

impl FakePreparation {
    fn event(&mut self, event: String) -> Result<(), BackendError> {
        self.events.push(event.clone());
        if self.cancel_on.as_ref() == Some(&event) {
            self.cancellation.cancel();
        }
        if self.fail_on.contains(&event) {
            Err(BackendError(format!("fault at {event}")))
        } else {
            Ok(())
        }
    }

    fn on_handle(
        &mut self,
        operation: &str,
        handle: &str,
        volume: &Volume,
    ) -> Result<(), BackendError> {
        assert_eq!(handle, volume.id);
        assert!(self.live.contains(handle));
        self.event(format!("{operation}:{handle}"))
    }

    fn run(&mut self, volumes: &[Volume]) -> Result<(), BackendError> {
        prepare_volumes(volumes, &self.cancellation.clone(), self)
    }
}

impl VolumePreparation for FakePreparation {
    type Handle = String;

    fn open(&mut self, volume: &Volume) -> Result<Self::Handle, BackendError> {
        self.event(format!("open:{}", volume.id))?;
        assert!(self.live.insert(volume.id.clone()));
        Ok(volume.id.clone())
    }

    fn verify(&mut self, handle: &Self::Handle, volume: &Volume) -> Result<(), BackendError> {
        self.on_handle("verify", handle, volume)
    }

    fn lock(&mut self, handle: &Self::Handle, volume: &Volume) -> Result<(), BackendError> {
        self.on_handle("lock", handle, volume)
    }

    fn flush(&mut self, handle: &Self::Handle, volume: &Volume) -> Result<(), BackendError> {
        self.on_handle("flush", handle, volume)
    }

    fn dismount(&mut self, handle: &Self::Handle, volume: &Volume) -> Result<(), BackendError> {
        self.on_handle("dismount", handle, volume)
    }

    fn close(&mut self, handle: Self::Handle, volume: &Volume) -> Result<(), BackendError> {
        assert_eq!(handle, volume.id);
        assert!(self.live.remove(&handle));
        self.event(format!("close:{handle}"))
    }

    fn validate_target(&mut self) -> Result<(), BackendError> {
        self.validations += 1;
        self.event(format!("validate:{}", self.validations))
    }
}

#[test]
fn all_volumes_are_locked_before_flush_and_dismount_and_closed_before_success() {
    let mut ops = FakePreparation::default();
    assert_eq!(ops.run(&[volume("a"), volume("b")]), Ok(()));
    assert_eq!(
        ops.events,
        [
            "validate:1", "open:a", "verify:a", "open:b", "verify:b",
            "lock:a", "lock:b", "flush:a", "flush:b", "validate:2",
            "dismount:a", "dismount:b", "close:b", "close:a",
        ]
    );
    assert!(ops.live.is_empty());
}

#[test]
fn empty_input_and_preexisting_cancellation_do_not_open_or_validate_anything() {
    let mut ops = FakePreparation::default();
    assert!(ops.run(&[]).unwrap_err().0.contains("没有关联"));
    ops.cancellation.cancel();
    assert!(ops.run(&[volume("a")]).unwrap_err().0.contains("取消"));
    assert!(ops.events.is_empty());
}

#[test]
fn duplicate_volume_ids_are_rejected_before_opening_any_handles() {
    for second in ["volume-a", "VOLUME-A"] {
        let mut ops = FakePreparation::default();
        assert!(ops.run(&[volume("volume-a"), volume(second)]).unwrap_err().0.contains("重复"));
        assert!(ops.events.is_empty());
        assert!(ops.live.is_empty());
    }
}

#[test]
fn every_failure_before_dismount_closes_acquired_handles_without_dismounting() {
    for failure in [
        "validate:1", "open:a", "verify:a", "open:b", "verify:b",
        "lock:a", "lock:b", "flush:a", "flush:b", "validate:2",
    ] {
        let mut ops = FakePreparation {
            fail_on: vec![failure.into()],
            ..FakePreparation::default()
        };
        let error = ops.run(&[volume("a"), volume("b")]).unwrap_err();
        assert!(error.0.contains(&format!("fault at {failure}")));
        assert!(ops.live.is_empty(), "leaked handle after {failure}");
        assert!(!ops.events.iter().any(|event| event.starts_with("dismount:")));
        // Successful opens must each be closed, including a handle whose
        // verification failed. A failed open never transfers ownership.
        for id in ["a", "b"] {
            let did_open = ops.events.contains(&format!("open:{id}"))
                && failure != format!("open:{id}");
            assert_eq!(ops.events.contains(&format!("close:{id}")), did_open);
        }
    }
}

#[test]
fn cancellation_after_each_phase_stops_work_but_never_skips_cleanup() {
    for cancel_on in [
        "validate:1", "open:a", "verify:a", "open:b", "verify:b",
        "lock:a", "lock:b", "flush:a", "flush:b", "validate:2",
        "dismount:a", "dismount:b", "close:b", "close:a",
    ] {
        let mut ops = FakePreparation {
            cancel_on: Some(cancel_on.into()),
            ..FakePreparation::default()
        };
        let error = ops.run(&[volume("a"), volume("b")]).unwrap_err();
        assert!(error.0.contains("取消"), "lost cancellation at {cancel_on}");
        assert!(ops.live.is_empty());
        let cancelled_at = ops.events.iter().position(|event| event == cancel_on).unwrap();
        assert!(ops.events[cancelled_at + 1..]
            .iter()
            .all(|event| event.starts_with("close:")));
        if cancel_on == "dismount:a" {
            assert!(error.0.contains("已完成卸载 1 个卷"));
        }
    }
}

#[test]
fn partial_dismount_failure_stops_later_volumes_and_reports_no_rollback() {
    let mut ops = FakePreparation {
        fail_on: vec!["dismount:b".into()],
        ..FakePreparation::default()
    };
    let error = ops.run(&[volume("a"), volume("b"), volume("c")]).unwrap_err();
    assert!(error.0.contains("fault at dismount:b"));
    assert!(error.0.contains("已完成卸载 1 个卷"));
    assert!(error.0.contains("未执行卸载回滚"));
    assert!(!ops.events.contains(&"dismount:c".into()));
    assert!(ops.events.ends_with(&["close:c".into(), "close:b".into(), "close:a".into()]));
    assert!(ops.live.is_empty());
}

#[test]
fn close_errors_are_all_reported_and_do_not_skip_other_closes() {
    let mut ops = FakePreparation {
        fail_on: vec!["close:a".into(), "close:b".into()],
        ..FakePreparation::default()
    };
    let error = ops.run(&[volume("a"), volume("b")]).unwrap_err();
    assert!(error.0.contains("fault at close:a"));
    assert!(error.0.contains("fault at close:b"));
    assert!(error.0.contains("已完成卸载 2 个卷"));
    assert!(ops.live.is_empty());
    assert!(ops.events.ends_with(&["close:b".into(), "close:a".into()]));
}

#[test]
fn cleanup_failure_keeps_the_primary_error_and_never_reports_success() {
    let mut ops = FakePreparation {
        fail_on: vec!["lock:b".into(), "close:b".into()],
        ..FakePreparation::default()
    };
    let error = ops.run(&[volume("a"), volume("b")]).unwrap_err();
    assert!(error.0.contains("fault at lock:b"));
    assert!(error.0.contains("fault at close:b"));
    assert!(!error.0.contains("已完成卸载"));
    assert!(ops.live.is_empty());
    assert!(ops.events.ends_with(&["close:b".into(), "close:a".into()]));
}
