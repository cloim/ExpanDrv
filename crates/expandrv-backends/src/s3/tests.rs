use super::*;
use expandrv_core::config::SecretSource;

fn p(s: &str) -> RemotePath {
    RemotePath::parse(s).unwrap()
}

fn cfg(endpoint: &str, bucket: &str, prefix: &str) -> S3Config {
    S3Config {
        endpoint: endpoint.into(),
        region: "us-east-1".into(),
        bucket: bucket.into(),
        prefix: prefix.into(),
        access_key_id: "testuser".into(),
        secret_access_key: SecretSource::Env {
            var: "EXPANDRV_UNUSED".into(),
        },
        path_style: true,
    }
}

fn offline(prefix: &str) -> S3Backend {
    S3Backend::new(
        &cfg("http://127.0.0.1:9/", "bkt", prefix),
        &SecretString::from("x"),
    )
    .unwrap()
}

const LIST_PAGE1: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>bkt</Name><Prefix>root/docs/</Prefix><KeyCount>4</KeyCount><MaxKeys>1000</MaxKeys>
  <Delimiter>/</Delimiter><IsTruncated>true</IsTruncated>
  <NextContinuationToken>tok+/=1</NextContinuationToken>
  <Contents>
    <Key>root/docs/</Key><LastModified>2024-05-01T10:20:30.000Z</LastModified>
    <ETag>&quot;d41d8cd98f00b204e9800998ecf8427e&quot;</ETag><Size>0</Size><StorageClass>STANDARD</StorageClass>
  </Contents>
  <Contents>
    <Key>root/docs/한글 보고서 &amp; 메모.txt</Key><LastModified>2024-05-02T01:02:03.000Z</LastModified>
    <ETag>"abc-2"</ETag><Size>12345</Size><Owner><ID>x</ID></Owner><StorageClass>STANDARD</StorageClass>
  </Contents>
  <CommonPrefixes><Prefix>root/docs/사진/</Prefix></CommonPrefixes>
  <Contents>
    <Key>root/docs/bad:name.txt</Key><Size>1</Size>
  </Contents>
  <CommonPrefixes><Prefix>root/docs/sub/</Prefix></CommonPrefixes>
</ListBucketResult>"#;

const LIST_PAGE2: &str = r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <IsTruncated>false</IsTruncated>
  <Contents><Key>root/docs/sub</Key><Size>7</Size></Contents>
  <Contents><Key>root/docs/z.bin</Key><Size>3</Size></Contents>
</ListBucketResult>"#;

#[test]
fn parses_list_page_with_prefixes_and_unicode() {
    let page = xml::parse_list(LIST_PAGE1).unwrap();
    assert!(page.is_truncated);
    assert_eq!(page.next_continuation_token.as_deref(), Some("tok+/=1"));
    assert_eq!(page.contents.len(), 3);
    assert_eq!(page.contents[1].key, "root/docs/한글 보고서 & 메모.txt");
    assert_eq!(page.contents[1].size, 12345);
    assert_eq!(page.common_prefixes.len(), 2);

    let mut out = Vec::new();
    page_entries("root/docs/", &page, &mut out);
    let page2 = xml::parse_list(LIST_PAGE2).unwrap();
    assert!(!page2.is_truncated);
    assert!(page2.next_continuation_token.is_none());
    page_entries("root/docs/", &page2, &mut out);
    let entries = dedupe_entries(out);

    let names: Vec<_> = entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
    // 표식 객체와 금지 문자 이름은 빠지고, 같은 이름의 파일과 접두사는 파일만 남습니다.
    assert_eq!(
        names,
        [
            ("sub", EntryKind::File),
            ("z.bin", EntryKind::File),
            ("사진", EntryKind::Dir),
            ("한글 보고서 & 메모.txt", EntryKind::File),
        ]
    );
    let doc = &entries[3];
    assert_eq!(doc.size, 12345);
    assert_eq!(doc.etag.as_deref(), Some("abc-2"));
    assert_eq!(
        doc.modified,
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_714_611_723))
    );
}

#[test]
fn parses_empty_list_and_multipart_responses() {
    let page = xml::parse_list(
        r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name><Prefix></Prefix><KeyCount>0</KeyCount><IsTruncated>false</IsTruncated></ListBucketResult>"#,
    )
    .unwrap();
    assert!(page.contents.is_empty() && page.common_prefixes.is_empty());

    let id = xml::parse_upload_id(
        r#"<?xml version="1.0"?><InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Bucket>b</Bucket><Key>k</Key><UploadId>VXBsb2FkIElE</UploadId></InitiateMultipartUploadResult>"#,
    )
    .unwrap();
    assert_eq!(id, "VXBsb2FkIElE");
    assert_eq!(
        xml::parse_copy_etag(
            r#"<CopyPartResult><LastModified>2024-01-01T00:00:00Z</LastModified><ETag>"e1"</ETag></CopyPartResult>"#
        )
        .unwrap(),
        "\"e1\""
    );
    let body = xml::complete_body(&[(1, "\"a\"".into()), (2, "\"b\"".into())]);
    assert_eq!(
        body,
        "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>&quot;a&quot;</ETag></Part>\
         <Part><PartNumber>2</PartNumber><ETag>&quot;b&quot;</ETag></Part></CompleteMultipartUpload>"
    );
}

#[test]
fn parses_error_body() {
    let text = r#"<?xml version="1.0" encoding="UTF-8"?>
<Error><Code>NoSuchKey</Code><Message>The specified key does not exist.</Message><Key>a</Key><RequestId>1</RequestId></Error>"#;
    assert_eq!(xml::root_name(text).as_deref(), Some("Error"));
    let (code, msg) = xml::parse_error(text);
    assert_eq!(code.as_deref(), Some("NoSuchKey"));
    assert!(msg.unwrap().contains("does not exist"));
    assert_eq!(xml::parse_error(""), (None, None));
    assert_eq!(
        xml::parse_error("<CopyObjectResult><ETag>x</ETag></CopyObjectResult>"),
        (None, None)
    );
}

#[test]
fn maps_status_and_codes() {
    use xml::map_status as m;
    assert!(matches!(m(404, Some("NoSuchKey"), None), Error::NotFound));
    assert!(matches!(m(404, None, None), Error::NotFound));
    assert!(matches!(
        m(404, Some("NoSuchBucket"), None),
        Error::Config(_)
    ));
    assert!(matches!(
        m(403, Some("SignatureDoesNotMatch"), None),
        Error::Auth(_)
    ));
    assert!(matches!(
        m(403, Some("InvalidAccessKeyId"), None),
        Error::Auth(_)
    ));
    assert!(matches!(
        m(403, Some("AccessDenied"), None),
        Error::PermissionDenied
    ));
    assert!(matches!(m(403, None, None), Error::PermissionDenied));
    assert!(matches!(m(401, None, None), Error::Auth(_)));
    assert!(matches!(
        m(301, Some("PermanentRedirect"), None),
        Error::Config(_)
    ));
    assert!(matches!(m(503, Some("SlowDown"), None), Error::Network(_)));
    assert!(matches!(
        m(200, Some("InternalError"), None),
        Error::Network(_)
    ));
    assert!(matches!(
        m(400, Some("RequestTimeout"), None),
        Error::Timeout
    ));
    assert!(matches!(
        m(507, Some("XMinioStorageFull"), None),
        Error::StorageFull
    ));
    match m(400, Some("InvalidArgument"), Some("bad part")) {
        Error::Protocol(s) => assert_eq!(s, "HTTP 400 InvalidArgument: bad part"),
        e => panic!("{e:?}"),
    }
}

#[test]
fn maps_keys_with_prefix() {
    assert_eq!(normalize_prefix("").unwrap(), "");
    assert_eq!(normalize_prefix("/").unwrap(), "");
    assert_eq!(normalize_prefix("photos").unwrap(), "photos/");
    assert_eq!(normalize_prefix("/a//b/").unwrap(), "a/b/");
    assert_eq!(normalize_prefix("a\\b").unwrap(), "a/b/");
    assert!(normalize_prefix("a/../b").is_err());

    let be = offline("/media/");
    assert_eq!(be.key(&p("/x/y.txt")), "media/x/y.txt");
    assert_eq!(be.dir_prefix(&p("/x")), "media/x/");
    assert_eq!(be.dir_prefix(&RemotePath::root()), "media/");
    let be = offline("");
    assert_eq!(be.key(&p("/x")), "x");
    assert_eq!(be.dir_prefix(&RemotePath::root()), "");
    assert_eq!(
        be.copy_source("폴더/a b+c.txt"),
        "/bkt/%ED%8F%B4%EB%8D%94/a%20b%2Bc.txt"
    );
}

#[test]
fn builds_object_urls_for_both_styles() {
    let be = offline("");
    let url = be
        .request(Method::Get, Some("d/한 글.txt"), &[], &[])
        .unwrap()
        .build()
        .unwrap()
        .url()
        .clone();
    assert!(
        url.as_str()
            .starts_with("http://127.0.0.1:9/bkt/d/%ED%95%9C%20%EA%B8%80.txt?X-Amz-Algorithm="),
        "{url}"
    );
    assert!(!url.as_str().contains("X-Amz-Credential=x")); // 비밀 키는 URL에 들어가지 않습니다.

    let mut c = cfg("https://s3.example.com/base", "bkt", "");
    c.path_style = false;
    let be = S3Backend::new(&c, &SecretString::from("x")).unwrap();
    let url = be
        .request(Method::Get, None, &[("prefix", "a/")], &[])
        .unwrap();
    let url = url.build().unwrap().url().clone();
    assert!(
        url.as_str()
            .starts_with("https://bkt.s3.example.com/base/?X-Amz-Algorithm="),
        "{url}"
    );
    assert!(url.as_str().contains("prefix=a%2F"));

    for bad in ["ftp://h/", "not a url", "https://u:p@h/"] {
        assert!(matches!(
            S3Backend::new(&cfg(bad, "bkt", ""), &SecretString::from("x")),
            Err(Error::Config(_))
        ));
    }
}

#[test]
fn computes_part_sizes() {
    assert_eq!(part_size(20 * MIB, PART_SIZE), 8 * MIB);
    assert_eq!(part_size(10_000 * 8 * MIB, PART_SIZE), 8 * MIB);
    // 100 GiB는 8 MiB로 12800 파트가 되므로 11 MiB로 키웁니다.
    let big = 100 << 30;
    let ps = part_size(big, PART_SIZE);
    assert_eq!(ps, 11 * MIB);
    assert!(big.div_ceil(ps) <= MAX_PARTS);
    // 5 TiB 상한에서도 파트 수 제한을 지킵니다.
    let max = 5u64 << 40;
    assert!(max.div_ceil(part_size(max, PART_SIZE)) <= MAX_PARTS);
    assert!(max.div_ceil(part_size(max, COPY_PART_SIZE)) <= MAX_PARTS);

    let r = part_ranges(20 * MIB, 8 * MIB);
    assert_eq!(
        r,
        [
            (1, 0, 8 * MIB),
            (2, 8 * MIB, 8 * MIB),
            (3, 16 * MIB, 4 * MIB)
        ]
    );
    assert_eq!(part_ranges(16 * MIB, 8 * MIB).len(), 2);
}

#[test]
fn parses_dates() {
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_445_412_480);
    assert_eq!(parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT"), Some(t));
    assert_eq!(parse_iso_date("2015-10-21T07:28:00.000Z"), Some(t));
    assert_eq!(parse_http_date("garbage"), None);
}

/// MinIO 통합 테스트. `EXPANDRV_TEST_S3_ENDPOINT`(예: `http://127.0.0.1:19000`)가 있을 때만 실행합니다.
mod minio {
    use super::*;

    struct Fixed(String);

    impl SecretResolver for Fixed {
        fn resolve(&self, _: &SecretSource) -> Result<SecretString> {
            Ok(SecretString::from(self.0.clone()))
        }
    }

    fn endpoint() -> Option<String> {
        let ep = std::env::var("EXPANDRV_TEST_S3_ENDPOINT").ok();
        if ep.is_none() {
            eprintln!("EXPANDRV_TEST_S3_ENDPOINT가 없어 S3 통합 테스트를 건너뜁니다");
        }
        ep
    }

    fn secret() -> Fixed {
        Fixed(std::env::var("EXPANDRV_TEST_S3_SECRET").unwrap_or_else(|_| "testpass123".into()))
    }

    fn access_key() -> String {
        std::env::var("EXPANDRV_TEST_S3_ACCESS_KEY").unwrap_or_else(|_| "testuser".into())
    }

    /// 테스트마다 새 버킷을 만들어 서로 간섭하지 않게 합니다.
    async fn fresh_bucket(ep: &str, tag: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("expandrv-{tag}-{nanos:x}");
        let mut c = cfg(ep, &name, "");
        c.access_key_id = access_key();
        let be = S3Backend::new(&c, &secret().resolve(&c.secret_access_key).unwrap()).unwrap();
        let rb = be
            .request(Method::Put, None, &[], &[])
            .unwrap()
            .header(CONTENT_LENGTH, 0);
        be.exec(rb).await.expect("버킷 생성");
        name
    }

    async fn backend(ep: &str, bucket: &str, prefix: &str) -> S3Backend {
        let mut c = cfg(ep, bucket, prefix);
        c.access_key_id = access_key();
        S3Backend::connect(&c, &secret()).await.unwrap()
    }

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    fn names(entries: &[Entry]) -> Vec<(String, EntryKind)> {
        entries.iter().map(|e| (e.name.clone(), e.kind)).collect()
    }

    /// 스트리밍 세션: 파트 크기를 넘으면 멀티파트, 넘지 않으면 단일 PUT으로 확정합니다.
    #[tokio::test]
    async fn streaming_session_single_put_and_multipart() {
        let Some(ep) = endpoint() else { return };
        let bucket = fresh_bucket(&ep, "stream").await;
        let be = backend(&ep, &bucket, "").await;
        assert!(be.capabilities().streaming_upload);

        // 파트 크기(8 MiB) 미만: 단일 PUT
        let small = pattern(300_000);
        let mut s = be
            .begin_upload(
                &p("/small.bin"),
                UploadHint {
                    size: Some(300_000),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        s.write(0, Bytes::from(small[..100_000].to_vec()))
            .await
            .unwrap();
        s.write(100_000, Bytes::from(small[100_000..].to_vec()))
            .await
            .unwrap();
        assert_eq!(s.finish().await.unwrap().size, 300_000);
        assert_eq!(
            &be.read_range(&p("/small.bin"), 0, 300_000).await.unwrap()[..],
            &small[..]
        );

        // 파트 크기를 넘는 파일: 파트 3개(8, 8, 1 MiB + 5)
        let big = pattern((17 << 20) + 5);
        let mut s = be
            .begin_upload(&p("/big.bin"), UploadHint::default())
            .await
            .unwrap();
        let mut off = 0usize;
        for chunk in big.chunks(1 << 20) {
            s.write(off as u64, Bytes::from(chunk.to_vec()))
                .await
                .unwrap();
            off += chunk.len();
        }
        s.flush().await.unwrap();
        let e = s.finish().await.unwrap();
        assert_eq!(e.size, big.len() as u64);
        let got = be
            .read_range(&p("/big.bin"), (8 << 20) - 10, 40)
            .await
            .unwrap();
        assert_eq!(&got[..], &big[(8 << 20) - 10..(8 << 20) + 30]);

        // 중단: 객체가 생기지 않고 진행 중 멀티파트도 남지 않습니다.
        let mut s = be
            .begin_upload(&p("/aborted.bin"), UploadHint::default())
            .await
            .unwrap();
        s.write(0, Bytes::from(pattern(9 << 20))).await.unwrap();
        s.abort().await;
        assert!(matches!(
            be.stat(&p("/aborted.bin")).await,
            Err(Error::NotFound)
        ));
        let names: Vec<_> = be
            .list(&RemotePath::root())
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["big.bin", "small.bin"]);
    }

    #[tokio::test]
    async fn full_flow() {
        let Some(ep) = endpoint() else { return };
        let bucket = fresh_bucket(&ep, "flow").await;
        let mut be = backend(&ep, &bucket, "").await;
        // 작은 파일로도 UploadPartCopy 경로를 검증하도록 임계값을 낮춥니다.
        be.copy_threshold = 10 * MIB;
        be.copy_part_size = 8 * MIB;

        assert!(be.stat(&RemotePath::root()).await.unwrap().is_dir());
        assert!(be.list(&RemotePath::root()).await.unwrap().is_empty());

        // mkdir
        assert!(be.mkdir(&p("/d")).await.unwrap().is_dir());
        assert!(matches!(
            be.mkdir(&p("/d")).await,
            Err(Error::AlreadyExists)
        ));
        assert!(be.stat(&p("/d")).await.unwrap().is_dir());
        assert!(be.list(&p("/d")).await.unwrap().is_empty());

        // 바이트 쓰기와 stat
        let e = be
            .write(
                &p("/d/a.txt"),
                WriteSource::Bytes(Bytes::from_static(b"hello world")),
            )
            .await
            .unwrap();
        assert_eq!(
            (e.name.as_str(), e.size, e.kind),
            ("a.txt", 11, EntryKind::File)
        );
        let st = be.stat(&p("/d/a.txt")).await.unwrap();
        assert_eq!(st.size, 11);
        assert!(st.etag.is_some() && st.modified.is_some());

        // 부분 읽기
        assert_eq!(
            &be.read_range(&p("/d/a.txt"), 6, 5).await.unwrap()[..],
            b"world"
        );
        assert_eq!(
            &be.read_range(&p("/d/a.txt"), 2, 3).await.unwrap()[..],
            b"llo"
        );
        assert_eq!(
            &be.read_range(&p("/d/a.txt"), 6, 100).await.unwrap()[..],
            b"world"
        );
        assert!(
            be.read_range(&p("/d/a.txt"), 50, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            be.read_range(&p("/d/a.txt"), 11, 1)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            be.read_range(&p("/d"), 0, 1).await,
            Err(Error::IsADirectory)
        ));

        // 중첩 경로, 유니코드와 공백 이름(표식 없는 암묵 디렉터리)
        be.write(
            &p("/d/sub/한글 파일+1.txt"),
            WriteSource::Bytes(Bytes::from("가나다")),
        )
        .await
        .unwrap();
        assert_eq!(
            names(&be.list(&p("/d")).await.unwrap()),
            [
                ("a.txt".to_owned(), EntryKind::File),
                ("sub".to_owned(), EntryKind::Dir)
            ]
        );
        assert_eq!(
            names(&be.list(&p("/d/sub")).await.unwrap()),
            [("한글 파일+1.txt".to_owned(), EntryKind::File)]
        );
        assert!(be.stat(&p("/d/sub")).await.unwrap().is_dir());
        assert_eq!(
            &be.read_range(&p("/d/sub/한글 파일+1.txt"), 0, 100)
                .await
                .unwrap()[..],
            "가나다".as_bytes()
        );

        // 16 MiB 초과 파일은 멀티파트 업로드
        let dir = tempfile::tempdir().unwrap();
        let data = pattern(20 * MIB as usize + 12345);
        let src = dir.path().join("big.bin");
        std::fs::write(&src, &data).unwrap();
        let e = be
            .write(
                &p("/d/big.bin"),
                WriteSource::File {
                    path: src.clone(),
                    size: data.len() as u64,
                },
            )
            .await
            .unwrap();
        assert_eq!(e.size, data.len() as u64);
        assert!(e.etag.as_deref().unwrap().ends_with("-3"), "{:?}", e.etag);
        let off = 8 * MIB - 3;
        assert_eq!(
            &be.read_range(&p("/d/big.bin"), off, 6).await.unwrap()[..],
            &data[off as usize..off as usize + 6]
        );
        let tail = be
            .read_range(&p("/d/big.bin"), data.len() as u64 - 10, 100)
            .await
            .unwrap();
        assert_eq!(&tail[..], &data[data.len() - 10..]);

        // 16 MiB 이하 파일은 단일 PUT 스트리밍
        let small = pattern(300_000);
        let src_small = dir.path().join("small.bin");
        std::fs::write(&src_small, &small).unwrap();
        let e = be
            .write(
                &p("/d/small.bin"),
                WriteSource::File {
                    path: src_small,
                    size: small.len() as u64,
                },
            )
            .await
            .unwrap();
        assert_eq!(e.size, small.len() as u64);
        assert_eq!(
            &be.read_range(&p("/d/small.bin"), 0, 1 << 20).await.unwrap()[..],
            &small[..]
        );

        // 임계값 초과 복사는 UploadPartCopy
        be.rename(&p("/d/big.bin"), &p("/d/big2.bin"), false)
            .await
            .unwrap();
        assert!(matches!(
            be.stat(&p("/d/big.bin")).await,
            Err(Error::NotFound)
        ));
        assert_eq!(
            be.stat(&p("/d/big2.bin")).await.unwrap().size,
            data.len() as u64
        );
        assert_eq!(
            &be.read_range(&p("/d/big2.bin"), off, 6).await.unwrap()[..],
            &data[off as usize..off as usize + 6]
        );

        // 이름 변경 replace 규칙
        be.write(&p("/d/b.txt"), WriteSource::Bytes(Bytes::from("B")))
            .await
            .unwrap();
        assert!(matches!(
            be.rename(&p("/d/a.txt"), &p("/d/b.txt"), false).await,
            Err(Error::AlreadyExists)
        ));
        assert_eq!(
            &be.read_range(&p("/d/b.txt"), 0, 10).await.unwrap()[..],
            b"B"
        );
        be.rename(&p("/d/a.txt"), &p("/d/b.txt"), true)
            .await
            .unwrap();
        assert_eq!(
            &be.read_range(&p("/d/b.txt"), 0, 100).await.unwrap()[..],
            b"hello world"
        );
        assert!(matches!(
            be.stat(&p("/d/a.txt")).await,
            Err(Error::NotFound)
        ));
        be.rename(&p("/d/b.txt"), &p("/d/B.txt"), false)
            .await
            .unwrap();
        assert!(be.stat(&p("/d/B.txt")).await.is_ok());
        assert!(matches!(
            be.stat(&p("/d/b.txt")).await,
            Err(Error::NotFound)
        ));

        // 디렉터리 이름 변경은 지원하지 않습니다.
        assert!(matches!(
            be.rename(&p("/d"), &p("/e"), false).await,
            Err(Error::Unsupported(_))
        ));

        // 없는 항목과 종류 불일치
        assert!(matches!(be.stat(&p("/none")).await, Err(Error::NotFound)));
        assert!(matches!(be.list(&p("/none")).await, Err(Error::NotFound)));
        assert!(matches!(
            be.list(&p("/d/B.txt")).await,
            Err(Error::NotADirectory)
        ));
        assert!(matches!(
            be.read_range(&p("/none"), 0, 1).await,
            Err(Error::NotFound)
        ));
        assert!(matches!(
            be.delete(&p("/none"), EntryKind::File).await,
            Err(Error::NotFound)
        ));
        assert!(matches!(
            be.delete(&p("/none"), EntryKind::Dir).await,
            Err(Error::NotFound)
        ));
        assert!(matches!(
            be.rename(&p("/none"), &p("/x"), false).await,
            Err(Error::NotFound)
        ));

        // 비어 있지 않은 디렉터리 삭제 거부 후 정리
        assert!(matches!(
            be.delete(&p("/d"), EntryKind::Dir).await,
            Err(Error::DirectoryNotEmpty)
        ));
        assert!(matches!(
            be.delete(&p("/d/sub"), EntryKind::Dir).await,
            Err(Error::DirectoryNotEmpty)
        ));
        for f in [
            "/d/B.txt",
            "/d/big2.bin",
            "/d/small.bin",
            "/d/sub/한글 파일+1.txt",
        ] {
            be.delete(&p(f), EntryKind::File).await.unwrap();
        }
        // 표식 없는 암묵 디렉터리는 마지막 파일과 함께 사라집니다.
        assert!(matches!(be.stat(&p("/d/sub")).await, Err(Error::NotFound)));
        assert!(be.list(&p("/d")).await.unwrap().is_empty());
        be.delete(&p("/d"), EntryKind::Dir).await.unwrap();
        assert!(matches!(be.stat(&p("/d")).await, Err(Error::NotFound)));
        assert!(be.list(&RemotePath::root()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn prefixed_root() {
        let Some(ep) = endpoint() else { return };
        let bucket = fresh_bucket(&ep, "prefix").await;
        let whole = backend(&ep, &bucket, "").await;
        whole
            .write(&p("/outside.txt"), WriteSource::Bytes(Bytes::from("o")))
            .await
            .unwrap();

        let be = backend(&ep, &bucket, "/nested/root/").await;
        assert!(be.list(&RemotePath::root()).await.unwrap().is_empty());
        be.write(&p("/x.txt"), WriteSource::Bytes(Bytes::from("xyz")))
            .await
            .unwrap();
        be.mkdir(&p("/m")).await.unwrap();
        assert_eq!(
            names(&be.list(&RemotePath::root()).await.unwrap()),
            [
                ("m".to_owned(), EntryKind::Dir),
                ("x.txt".to_owned(), EntryKind::File)
            ]
        );
        assert_eq!(whole.stat(&p("/nested/root/x.txt")).await.unwrap().size, 3);
        assert!(whole.stat(&p("/nested/root/m")).await.unwrap().is_dir());
        assert!(matches!(
            be.stat(&p("/outside.txt")).await,
            Err(Error::NotFound)
        ));
        assert_eq!(
            names(&whole.list(&RemotePath::root()).await.unwrap()),
            [
                ("nested".to_owned(), EntryKind::Dir),
                ("outside.txt".to_owned(), EntryKind::File)
            ]
        );
    }

    #[tokio::test]
    async fn connect_errors() {
        let Some(ep) = endpoint() else { return };
        let bucket = fresh_bucket(&ep, "auth").await;
        let mut c = cfg(&ep, &bucket, "");
        c.access_key_id = access_key();
        let err = S3Backend::connect(&c, &Fixed("wrong-secret".into()))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, Error::Auth(_)), "{err:?}");
        assert!(!err.to_string().contains("wrong-secret"));

        let mut c = cfg(&ep, "expandrv-no-such-bucket", "");
        c.access_key_id = access_key();
        let err = S3Backend::connect(&c, &secret()).await.err().unwrap();
        assert!(matches!(err, Error::Config(_)), "{err:?}");

        let c = cfg("http://127.0.0.1:1/", &bucket, "");
        let err = S3Backend::connect(&c, &secret()).await.err().unwrap();
        assert!(matches!(err, Error::Network(_) | Error::Timeout), "{err:?}");

        // 팩토리 경유 연결
        let mut c = cfg(&ep, &bucket, "");
        c.access_key_id = access_key();
        let be = crate::connect(
            &expandrv_core::config::BackendConfig::S3(c),
            None,
            &secret(),
        )
        .await
        .unwrap();
        assert_eq!(be.kind(), "s3");
        assert!(!be.capabilities().rename_dir);
    }
}
