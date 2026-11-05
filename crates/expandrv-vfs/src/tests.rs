use super::*;
use expandrv_backends::local::LocalBackend;

struct Fixture {
    _rt: tokio::runtime::Runtime,
    root: tempfile::TempDir,
    staging: tempfile::TempDir,
    vfs: Vfs,
}

fn fixture(read_only: bool) -> Fixture {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let backend = Arc::new(LocalBackend::new(root.path()).unwrap());
    let vfs = Vfs::new(
        backend,
        rt.handle().clone(),
        VfsOptions {
            read_only,
            metadata_ttl: Duration::from_secs(60),
            staging_dir: staging.path().to_owned(),
            op_timeout: Duration::from_secs(10),
        },
    )
    .unwrap();
    Fixture {
        _rt: rt,
        root,
        staging,
        vfs,
    }
}

fn p(s: &str) -> RemotePath {
    RemotePath::parse(s).unwrap()
}

fn staging_files(f: &Fixture) -> usize {
    std::fs::read_dir(f.staging.path()).unwrap().count()
}

#[test]
fn create_write_upload_on_last_cleanup() {
    let f = fixture(false);
    let h = f.vfs.create(&p("/a.txt"), EntryKind::File).unwrap();
    let (n, e) = f.vfs.write(&h, 0, b"hello", false, false).unwrap();
    assert_eq!((n, e.size), (5, 5));
    f.vfs.write(&h, 0, b" world", true, false).unwrap();

    // 업로드 전에는 원격이 비어 있고, 목록과 속성은 스테이징 크기를 보여 줍니다.
    assert_eq!(std::fs::read(f.root.path().join("a.txt")).unwrap(), b"");
    assert_eq!(f.vfs.stat(&p("/a.txt")).unwrap().1.size, 11);
    assert_eq!(f.vfs.list(&RemotePath::root()).unwrap()[0].size, 11);

    f.vfs.cleanup(&h, false).unwrap();
    f.vfs.close(&h);
    assert_eq!(
        std::fs::read(f.root.path().join("a.txt")).unwrap(),
        b"hello world"
    );
    assert_eq!(staging_files(&f), 0);
}

#[test]
fn partial_write_preserves_remote_content() {
    let f = fixture(false);
    std::fs::write(f.root.path().join("b.bin"), b"0123456789").unwrap();
    let h = f.vfs.open(&p("/b.bin")).unwrap();
    let mut buf = [0u8; 4];
    assert_eq!(f.vfs.read(&h, 3, &mut buf).unwrap(), 4);
    assert_eq!(&buf, b"3456");

    f.vfs.write(&h, 2, b"AB", false, false).unwrap();
    // 페이징 쓰기는 파일을 늘리지 않습니다.
    assert_eq!(f.vfs.write(&h, 9, b"XYZ", false, true).unwrap().0, 1);
    f.vfs.cleanup(&h, false).unwrap();
    assert_eq!(
        std::fs::read(f.root.path().join("b.bin")).unwrap(),
        b"01AB45678X"
    );
}

#[test]
fn truncate_and_extend() {
    let f = fixture(false);
    std::fs::write(f.root.path().join("c"), b"abcdef").unwrap();
    let h = f.vfs.open(&p("/c")).unwrap();
    assert_eq!(f.vfs.set_size(&h, 3, false).unwrap().size, 3);
    assert_eq!(f.vfs.set_size(&h, 10, true).unwrap().size, 3); // 할당 확장은 크기 불변
    assert_eq!(f.vfs.set_size(&h, 5, false).unwrap().size, 5);
    f.vfs.cleanup(&h, false).unwrap();
    assert_eq!(std::fs::read(f.root.path().join("c")).unwrap(), b"abc\0\0");

    let h = f.vfs.open(&p("/c")).unwrap();
    f.vfs.overwrite(&h).unwrap();
    f.vfs.cleanup(&h, false).unwrap();
    assert_eq!(std::fs::metadata(f.root.path().join("c")).unwrap().len(), 0);
}

#[test]
fn rename_open_file_then_upload_to_new_path() {
    let f = fixture(false);
    std::fs::create_dir(f.root.path().join("d")).unwrap();
    let h = f.vfs.create(&p("/d/x.txt"), EntryKind::File).unwrap();
    f.vfs.write(&h, 0, b"data", false, false).unwrap();
    f.vfs.rename(&h, &p("/d/y.txt"), false).unwrap();
    assert_eq!(h.path().as_str(), "/d/y.txt");
    f.vfs.cleanup(&h, false).unwrap();
    assert_eq!(
        std::fs::read(f.root.path().join("d/y.txt")).unwrap(),
        b"data"
    );
    assert!(matches!(f.vfs.stat(&p("/d/x.txt")), Err(Error::NotFound)));

    let dh = f.vfs.open(&p("/d")).unwrap();
    f.vfs.rename(&dh, &p("/e"), false).unwrap();
    f.vfs.cleanup(&dh, false).unwrap();
    assert_eq!(f.vfs.stat(&p("/e/y.txt")).unwrap().1.size, 4);
}

#[test]
fn delete_rules() {
    let f = fixture(false);
    let d = f.vfs.create(&p("/dir"), EntryKind::Dir).unwrap();
    let file = f.vfs.create(&p("/dir/f"), EntryKind::File).unwrap();
    assert!(matches!(
        f.vfs.check_delete(&d),
        Err(Error::DirectoryNotEmpty)
    ));
    f.vfs.check_delete(&file).unwrap();
    f.vfs.cleanup(&file, true).unwrap();
    f.vfs.check_delete(&d).unwrap();
    f.vfs.cleanup(&d, true).unwrap();
    assert!(!f.root.path().join("dir").exists());
    assert!(matches!(f.vfs.stat(&p("/dir")), Err(Error::NotFound)));
}

#[test]
fn cached_listing_answers_missing_names_without_backend() {
    let f = fixture(false);
    std::fs::write(f.root.path().join("Real.txt"), b"1").unwrap();
    f.vfs.list(&RemotePath::root()).unwrap();
    // 목록 캐시가 있으면 원격에 없는 이름은 바로 NotFound입니다.
    std::fs::write(f.root.path().join("late.txt"), b"1").unwrap();
    assert!(matches!(f.vfs.stat(&p("/late.txt")), Err(Error::NotFound)));
    assert_eq!(f.vfs.stat(&p("/real.TXT")).unwrap().0.as_str(), "/Real.txt");
    f.vfs.refresh();
    assert!(f.vfs.stat(&p("/late.txt")).is_ok());
}

#[test]
fn read_only_rejects_mutations() {
    let f = fixture(true);
    std::fs::write(f.root.path().join("r"), b"x").unwrap();
    assert!(matches!(
        f.vfs.create(&p("/n"), EntryKind::File),
        Err(Error::ReadOnly)
    ));
    let h = f.vfs.open(&p("/r")).unwrap();
    assert!(matches!(
        f.vfs.write(&h, 0, b"y", false, false),
        Err(Error::ReadOnly)
    ));
    assert!(matches!(
        f.vfs.rename(&h, &p("/s"), false),
        Err(Error::ReadOnly)
    ));
    assert!(matches!(f.vfs.check_delete(&h), Err(Error::ReadOnly)));
}

#[test]
fn create_existing_fails() {
    let f = fixture(false);
    std::fs::write(f.root.path().join("e"), b"x").unwrap();
    assert!(matches!(
        f.vfs.create(&p("/e"), EntryKind::File),
        Err(Error::AlreadyExists)
    ));
    assert!(matches!(
        f.vfs.create(&p("/e/x"), EntryKind::File),
        Err(Error::NotADirectory)
    ));
}
