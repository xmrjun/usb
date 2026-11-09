const NATIVE_GUI: &str = include_str!("../src/gui/native.rs");

#[test]
fn window_proc_never_forms_a_long_lived_exclusive_app_reference() {
    assert!(!NATIVE_GUI.contains("&mut *app_pointer"));
    assert!(NATIVE_GUI.contains("let app = unsafe { &*app_pointer }"));
}

#[test]
fn notify_header_is_validated_before_payload_is_interpreted() {
    let header = NATIVE_GUI.find("as *const NMHDR").unwrap();
    let payload = NATIVE_GUI.find("as *const NMLISTVIEW").unwrap();
    assert!(header < payload);
    assert!(NATIVE_GUI[header..payload].contains("header.hwndFrom"));
    assert!(NATIVE_GUI[header..payload].contains("header.code"));
}
