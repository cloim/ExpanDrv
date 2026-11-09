use super::*;

#[test]
fn removal_preserves_menus_owned_by_another_installation() {
    let root = format!(r"Software\ExpanDrvTest\Shell-scoped-{}", std::process::id());
    let registry = Registration { root: &root };
    let temp = tempfile::tempdir().unwrap();
    let gui = temp.path().join("gui.exe");
    std::fs::write(&gui, b"test").unwrap();
    registry
        .install(&gui, &temp.path().join("config.toml"))
        .unwrap();
    registry
        .uninstall_matching(Some(&temp.path().join("other.exe")))
        .unwrap();
    assert!(registry.enabled().unwrap());
    registry.uninstall_matching(Some(&gui)).unwrap();
    assert!(!registry.enabled().unwrap());
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(root));
    }
}

#[test]
fn registry_roundtrip_updates_own_entries_and_preserves_others() {
    let root = format!(
        r"Software\ExpanDrvTest\Shell-roundtrip-{}",
        std::process::id()
    );
    let registry = Registration { root: &root };
    let temp = tempfile::tempdir().unwrap();
    let gui = temp.path().join("한글 App.exe");
    std::fs::write(&gui, b"test").unwrap();
    let config = temp.path().join("settings file.toml");
    let unrelated = format!(r"{root}\Drive\shell\OtherApp");
    write(&unrelated, "", "keep").unwrap();
    assert!(!registry.enabled().unwrap());
    registry.install(&gui, &config).unwrap();
    registry.install(&gui, &config).unwrap();
    assert!(registry.enabled().unwrap());
    for (key, placeholder, manage) in registry.keys() {
        let cmd = read(&format!(r"{key}\command"), "").unwrap().unwrap();
        assert!(cmd.starts_with(&format!(
            "\"{}\" --config \"{}\"",
            gui.display(),
            config.display()
        )));
        assert_eq!(cmd.contains("--manage-path"), manage);
        if manage {
            assert!(cmd.ends_with(&format!("\"{placeholder}\\.\"")));
        }
        let expanded = cmd.replace(placeholder, r"X:\");
        let mut count = 0;
        unsafe {
            let argv =
                windows::Win32::UI::Shell::CommandLineToArgvW(&HSTRING::from(expanded), &mut count);
            assert!(!argv.is_null());
            let args: Vec<String> = std::slice::from_raw_parts(argv, count as usize)
                .iter()
                .map(|arg| arg.to_string().unwrap())
                .collect();
            let _ = windows::Win32::Foundation::LocalFree(Some(
                windows::Win32::Foundation::HLOCAL(argv.cast()),
            ));
            assert_eq!(args[0], gui.to_str().unwrap());
            assert_eq!(args[1], "--config");
            assert_eq!(args[2], config.to_str().unwrap());
            if manage {
                assert_eq!(args.len(), 5);
                assert_eq!(args[3], "--manage-path");
                assert_eq!(normalized(&args[4]).unwrap(), r"x:\");
            } else {
                assert_eq!(args.len(), 3);
            }
        }
    }
    registry.uninstall().unwrap();
    registry.uninstall().unwrap();
    assert!(!registry.enabled().unwrap());
    assert_eq!(read(&unrelated, "").unwrap().as_deref(), Some("keep"));
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(root));
    }
}

#[test]
fn paths_respect_boundaries_case_and_nested_mounts() {
    use expandrv_core::config::DriveConfig;
    let mut config = Config::default();
    // 기존 설정 형식을 사용하고 백엔드에는 접근하지 않습니다.
    for (id, mount) in [
        ("root", "X:"),
        ("outer", r"C:\Mount"),
        ("inner", r"C:\Mount\Nested"),
    ] {
        let drive = DriveConfig {
            id: id.into(),
            label: id.into(),
            mount_point: mount.into(),
            read_only: false,
            cache: Default::default(),
            transfer: Default::default(),
            auto_mount: false,
            proxy: None,
            backend: expandrv_core::config::BackendConfig::Local { root: "C:/".into() },
        };
        config.drives.push(drive);
    }
    for (path, expected) in [
        (r"X:\\.", Some("root")),
        (r"x:\docs\한글", Some("root")),
        (r"C:\MOUNT\Nested\folder", Some("inner")),
        (r"C:\Mount\Nested\..\other", Some("outer")),
        (r"C:\MountOther", None),
        (r"X:relative", None),
        (r"\\server\share", None),
    ] {
        assert_eq!(drive_for_path(Path::new(path), &config), expected, "{path}");
    }
}

#[test]
fn foreign_registration_is_preserved_before_any_write() {
    let root = format!(
        r"Software\ExpanDrvTest\Shell-foreign-{}",
        std::process::id()
    );
    let registry = Registration { root: &root };
    let temp = tempfile::tempdir().unwrap();
    let gui = temp.path().join("gui.exe");
    std::fs::write(&gui, b"test").unwrap();
    let foreign = registry.keys().pop().unwrap().0;
    write(&foreign, "Icon", "other-app").unwrap();
    assert!(
        registry
            .install(&gui, &temp.path().join("config.toml"))
            .is_err()
    );
    assert!(!exists(&registry.keys()[0].0).unwrap());
    registry.uninstall().unwrap();
    assert_eq!(
        read(&foreign, "Icon").unwrap().as_deref(),
        Some("other-app")
    );
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(root));
    }
}
