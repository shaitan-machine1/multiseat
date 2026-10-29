#[test]
fn monitor_first_context_rejects_an_uninitialized_real_device() {
    let context = seat_udev::Context::new().expect("open libudev context");
    let error = context
        .lookup_path(std::path::Path::new("/dev/null"))
        .expect_err("this test host's /dev/null is not initialized in udev");
    assert!(matches!(error, seat_udev::Error::NotInitialized(_)));
}
