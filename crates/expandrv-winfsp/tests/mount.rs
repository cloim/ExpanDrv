//! Local 백엔드를 실제 드라이브 문자로 마운트해 Win32 파일 API로 검증합니다.
//! WinFsp 런타임이 없으면 건너뜁니다.

use expandrv_backends::local::LocalBackend;
use expandrv_vfs::{Vfs, VfsOptions};
use expandrv_winfsp::{MountOptions, MountedDrive, runtime};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// 마운트 테스트끼리 같은 드라이브 문자를 고르지 않도록 순서대로 실행합니다.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn free_drive() -> String {
    ('H'..='Z')
        .rev()
        .map(|c| format!("{c}:"))
        .find(|d| !Path::new(&format!("{d}\\")).exists())
        .expect("빈 드라이브 문자 없음")
}

#[test]
fn network_share_mounts_as_remote_drive() {
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows::core::HSTRING;
    const DRIVE_REMOTE: u32 = 4;
    if runtime::runtime_dll().is_none() {
        eprintln!("WinFsp 런타임 미설치: 건너뜀");
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let root = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    fs::write(root.path().join("hello.txt"), b"network").unwrap();
    let vfs = Arc::new(
        Vfs::new(
            Arc::new(LocalBackend::new(root.path()).unwrap()),
            rt.handle().clone(),
            VfsOptions::new(staging.path().to_owned()),
        )
        .unwrap(),
    );
    let mount_point = free_drive();
    let drive = MountedDrive::mount(
        vfs.clone(),
        &MountOptions {
            mount_point: mount_point.clone(),
            label: "ExpanDrvNet".into(),
            file_info_timeout_ms: 1000,
            serial: 2,
            network_share: Some("ExpanDrvNetTest".into()),
        },
    )
    .unwrap();
    let unc = drive.unc_path().expect("UNC 경로");
    assert!(unc.starts_with(r"\\ExpanDrv-") && unc.ends_with(r"\ExpanDrvNetTest"));
    let kind = unsafe { GetDriveTypeW(&HSTRING::from(format!("{mount_point}\\"))) };
    assert_eq!(kind, DRIVE_REMOTE, "네트워크 드라이브로 보여야 합니다");
    assert_eq!(
        fs::read(format!("{mount_point}\\hello.txt")).unwrap(),
        b"network"
    );
    fs::write(format!("{mount_point}\\new.txt"), b"x").unwrap();
    // 네트워크 드라이브에서도 새 폴더를 만든 직후 이름을 바꿀 수 있어야 합니다.
    fs::create_dir(format!("{mount_point}\\새 폴더")).unwrap();
    fs::rename(
        format!("{mount_point}\\새 폴더"),
        format!("{mount_point}\\Flux1.D"),
    )
    .unwrap();
    drop(drive);
    vfs.sync(Duration::from_secs(10)).unwrap();
    assert_eq!(fs::read(root.path().join("new.txt")).unwrap(), b"x");
    assert!(root.path().join("Flux1.D").is_dir() && !root.path().join("새 폴더").exists());
}

#[test]
fn local_backend_mount_end_to_end() {
    if runtime::runtime_dll().is_none() {
        eprintln!("WinFsp 런타임 미설치: 건너뜀");
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let root = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    fs::write(root.path().join("existing.txt"), b"0123456789").unwrap();
    fs::create_dir(root.path().join("sub")).unwrap();

    let vfs = Arc::new(
        Vfs::new(
            Arc::new(LocalBackend::new(root.path()).unwrap()),
            rt.handle().clone(),
            VfsOptions {
                metadata_ttl: Duration::from_secs(5),
                op_timeout: Duration::from_secs(30),
                ..VfsOptions::new(staging.path().to_owned())
            },
        )
        .unwrap(),
    );
    let mount_point = free_drive();
    let drive = MountedDrive::mount(
        vfs.clone(),
        &MountOptions {
            mount_point: mount_point.clone(),
            label: "ExpanDrvTest".into(),
            file_info_timeout_ms: 1000,
            serial: 0x4558_5044,
            network_share: None,
        },
    )
    .unwrap();
    let m = |rel: &str| PathBuf::from(format!("{mount_point}\\{rel}"));
    let local = |rel: &str| root.path().join(rel);
    // 업로드는 핸들 정리 후 백그라운드로 진행되므로 원격 확인 전에 기다립니다.
    let settle = || vfs.sync(Duration::from_secs(30)).unwrap();

    // 열거와 읽기(대소문자 무시)
    let mut names: Vec<_> = fs::read_dir(m(""))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["existing.txt", "sub"]);
    assert_eq!(fs::read(m("EXISTING.TXT")).unwrap(), b"0123456789");

    // 새 파일 쓰기(1 MiB 넘게)와 업로드
    let big: Vec<u8> = (0..1_500_000u32).map(|i| (i % 251) as u8).collect();
    fs::write(m("sub\\big.bin"), &big).unwrap();
    settle();
    assert_eq!(fs::read(local("sub/big.bin")).unwrap(), big);

    // 기존 파일 부분 수정
    {
        let mut f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(m("existing.txt"))
            .unwrap();
        f.seek(SeekFrom::Start(3)).unwrap();
        f.write_all(b"XY").unwrap();
        f.seek(SeekFrom::End(0)).unwrap();
        f.write_all(b"!").unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        assert_eq!(s, "012XY56789!");
    }
    settle();
    assert_eq!(fs::read(local("existing.txt")).unwrap(), b"012XY56789!");

    // 디렉터리 생성, 이름 변경, 덮어쓰기 이동
    fs::create_dir(m("newdir")).unwrap();
    assert!(local("newdir").is_dir());
    fs::rename(m("sub\\big.bin"), m("newdir\\moved.bin")).unwrap();
    assert!(local("newdir/moved.bin").is_file() && !local("sub/big.bin").exists());
    fs::write(m("a.txt"), b"A").unwrap();
    fs::write(m("b.txt"), b"B").unwrap();
    fs::rename(m("a.txt"), m("b.txt")).unwrap();
    settle();
    assert_eq!(fs::read(local("b.txt")).unwrap(), b"A");
    fs::rename(m("newdir"), m("renamed")).unwrap();
    assert_eq!(
        fs::metadata(m("renamed\\moved.bin")).unwrap().len(),
        1_500_000
    );

    // 삭제 규칙
    assert!(
        fs::remove_dir(m("renamed")).is_err(),
        "비어 있지 않은 디렉터리"
    );
    fs::remove_file(m("renamed\\moved.bin")).unwrap();
    fs::remove_dir(m("renamed")).unwrap();
    fs::remove_file(m("b.txt")).unwrap();
    assert!(!local("renamed").exists() && !local("b.txt").exists());

    // 잘라내기
    let f = fs::OpenOptions::new()
        .write(true)
        .open(m("existing.txt"))
        .unwrap();
    f.set_len(4).unwrap();
    drop(f);
    settle();
    assert_eq!(fs::read(local("existing.txt")).unwrap(), b"012X");

    drop(drive);
    assert!(!Path::new(&format!("{mount_point}\\")).exists());
}

/// 원격에서 생긴 파일을 폴링으로 감지하고 탐색기 변경 알림까지 전달하는지 확인합니다.
#[test]
fn remote_change_reaches_directory_watchers() {
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::Storage::FileSystem::{
        FILE_NOTIFY_CHANGE_FILE_NAME, FindCloseChangeNotification, FindFirstChangeNotificationW,
    };
    use windows::Win32::System::Threading::WaitForSingleObject;
    use windows::core::HSTRING;

    if runtime::runtime_dll().is_none() {
        eprintln!("WinFsp 런타임 미설치: 건너뜀");
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let root = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let vfs = Arc::new(
        Vfs::new(
            Arc::new(LocalBackend::new(root.path()).unwrap()),
            rt.handle().clone(),
            VfsOptions::new(staging.path().to_owned()),
        )
        .unwrap(),
    );
    vfs.start_watch(Duration::from_millis(300));
    let mount_point = free_drive();
    let drive = MountedDrive::mount(
        vfs.clone(),
        &MountOptions {
            mount_point: mount_point.clone(),
            label: "ExpanDrvNotify".into(),
            file_info_timeout_ms: 1000,
            serial: 1,
            network_share: None,
        },
    )
    .unwrap();
    let dir = format!("{mount_point}\\");
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);

    let watch = unsafe {
        FindFirstChangeNotificationW(
            &HSTRING::from(dir.as_str()),
            false,
            FILE_NOTIFY_CHANGE_FILE_NAME,
        )
    }
    .unwrap();
    fs::write(root.path().join("from-remote.txt"), b"hi").unwrap();
    let signaled = unsafe { WaitForSingleObject(watch, 10_000) } == WAIT_OBJECT_0;
    unsafe { FindCloseChangeNotification(watch) }.unwrap();
    assert!(signaled, "10초 안에 변경 알림이 오지 않았습니다");
    let names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["from-remote.txt"]);
    drop(drive);
}
