use std::io::Cursor;

use usb_disk_ejector::{
    backend::{Backend, BackendError, Cancellation, EjectFailure, EjectResult, VetoKind},
    cli::run,
    model::{Device, Disk, Partition, Volume, VolumeExtent},
    policy::ListedDevice,
};

#[derive(Clone)]
struct FakeBackend {
    devices: Vec<ListedDevice>,
    result: Result<EjectResult, BackendError>,
    requested: Vec<(String, String)>,
}

impl Backend for FakeBackend {
    fn list_devices(&mut self) -> Result<Vec<ListedDevice>, BackendError> {
        Ok(self.devices.clone())
    }

    fn request_eject(
        &mut self,
        expected: &Device,
        cancellation: &Cancellation,
    ) -> Result<EjectResult, BackendError> {
        assert!(!cancellation.is_cancelled());
        self.requested
            .push((expected.public_id.clone(), expected.fingerprint()));
        self.result.clone()
    }
}

fn device() -> ListedDevice {
    ListedDevice {
        device: Device::new(
            r"USB\VID_1234\SERIAL".into(),
            "{11111111-2222-3333-4444-555555555555}".into(),
            "USBSTOR".into(),
            "Example USB".into(),
            vec![Disk {
                instance_id: "disk-instance".into(),
                number: 3,
                name: "Example USB Disk".into(),
                serial: "SERIAL".into(),
                capacity_bytes: 8_000_000_000,
                partition_count: 1,
                partitions: vec![Partition {
                    number: 1,
                    starting_offset: 1_048_576,
                    length: 7_000_000_000,
                }],
                partition_table_supported: true,
            }],
            vec![Volume {
                id: "volume-one".into(),
                mount_paths: vec![r"E:\".into()],
                capacity_bytes: Some(7_500_000_000),
                filesystem: Some("exFAT".into()),
                disk_numbers: vec![3],
                extents: vec![VolumeExtent {
                    disk_number: 3,
                    starting_offset: 1_048_576,
                    extent_length: 7_000_000_000,
                }],
                mapping_complete: true,
            }],
        ),
        eligibility: Ok(()),
    }
}

fn fake_backend(result: Result<EjectResult, BackendError>) -> FakeBackend {
    FakeBackend {
        devices: vec![device()],
        result,
        requested: Vec::new(),
    }
}

#[test]
fn list_shows_exact_id_name_mount_capacity_and_partitions() {
    let mut backend = fake_backend(Ok(EjectResult::Ejected));
    let mut output = Vec::new();
    let exit = run(
        ["usb-ejector", "list"],
        &mut backend,
        Cursor::new(Vec::<u8>::new()),
        &mut output,
        Vec::new(),
    );
    let text = String::from_utf8(output).unwrap();
    assert_eq!(exit, 0);
    assert!(text.contains(&device().device.public_id));
    assert!(text.contains("Example USB"));
    assert!(text.contains(r"E:\"));
    assert!(text.contains("physical: 8.0 GB"));
    assert!(text.contains("7.5 GB logical volume"));
    assert!(text.contains("1 partition"));
}

#[test]
fn eject_requires_exact_id_and_typed_confirmation() {
    let expected_id = device().device.public_id;
    let mut backend = fake_backend(Ok(EjectResult::Ejected));
    let mut output = Vec::new();
    let exit = run(
        ["usb-ejector", "eject", expected_id.as_str()],
        &mut backend,
        Cursor::new(b"EJECT\n"),
        &mut output,
        Vec::new(),
    );
    assert_eq!(exit, 0);
    assert_eq!(backend.requested.len(), 1);
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("Physical disk 3"));
    assert!(text.contains("safe removal was requested successfully"));
}

#[test]
fn cancellation_never_calls_backend_eject() {
    let expected_id = device().device.public_id;
    let mut backend = fake_backend(Ok(EjectResult::Ejected));
    let exit = run(
        ["usb-ejector", "eject", expected_id.as_str()],
        &mut backend,
        Cursor::new(b"no\n"),
        Vec::new(),
        Vec::new(),
    );
    assert_eq!(exit, 2);
    assert!(backend.requested.is_empty());
}

#[test]
fn blocked_or_unknown_selection_never_calls_backend_eject() {
    let mut blocked = device();
    blocked.eligibility = Err(usb_disk_ejector::policy::BlockReason::SystemDisk);
    let blocked_id = blocked.device.public_id.clone();
    let mut backend = fake_backend(Ok(EjectResult::Ejected));
    backend.devices = vec![blocked];
    assert_eq!(
        run(
            ["usb-ejector", "eject", blocked_id.as_str()],
            &mut backend,
            Cursor::new(b"EJECT\n"),
            Vec::new(),
            Vec::new(),
        ),
        1
    );
    assert!(backend.requested.is_empty());

    let mut backend = fake_backend(Ok(EjectResult::Ejected));
    assert_eq!(
        run(
            ["usb-ejector", "eject", "usb-not-present"],
            &mut backend,
            Cursor::new(b"EJECT\n"),
            Vec::new(),
            Vec::new(),
        ),
        1
    );
    assert!(backend.requested.is_empty());
}

#[test]
fn outstanding_open_veto_does_not_claim_to_identify_a_process() {
    let expected_id = device().device.public_id;
    let mut backend = fake_backend(Ok(EjectResult::Rejected(EjectFailure {
        config_ret: 23,
        veto: Some((
            VetoKind::OutstandingOpen,
            r"USBSTOR\DISK&VEN_EXAMPLE\SERIAL".into(),
        )),
    })));
    let mut error = Vec::new();
    let exit = run(
        ["usb-ejector", "eject", expected_id.as_str()],
        &mut backend,
        Cursor::new(b"EJECT\n"),
        Vec::new(),
        &mut error,
    );
    let text = String::from_utf8(error).unwrap();
    assert_eq!(exit, 1);
    assert!(text.contains("outstanding open handles"));
    assert!(text.contains("device instance identifier"));
    assert!(!text.contains("occupying process"));
}
