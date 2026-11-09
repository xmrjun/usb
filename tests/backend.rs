use usb_disk_ejector::backend::Cancellation;

#[test]
fn cancellation_is_shared_between_gui_and_worker_clones() {
    let gui = Cancellation::new();
    let worker = gui.clone();
    assert!(!worker.is_cancelled());
    gui.cancel();
    assert!(worker.is_cancelled());
}
