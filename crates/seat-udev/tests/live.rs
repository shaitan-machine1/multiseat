#[test]
fn monitor_first_context_respects_real_device_initialization() {
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    let context = seat_udev::Context::new().expect("open libudev context");
    let path = Path::new("/dev/null");
    let metadata = std::fs::metadata(path).expect("inspect /dev/null");
    let device = udev::Device::from_devnum(udev::DeviceType::Character, metadata.rdev())
        .expect("resolve /dev/null through libudev");
    let result = context.lookup_path(path);
    if device.is_initialized() {
        let parsed = result.expect("initialized device is accepted");
        assert_eq!(parsed.syspath, device.syspath());
        assert_eq!(parsed.devnum, Some(metadata.rdev()));
        assert_eq!(parsed.devnode.as_deref(), Some(path));
    } else {
        assert!(matches!(result, Err(seat_udev::Error::NotInitialized(_))));
    }
}
