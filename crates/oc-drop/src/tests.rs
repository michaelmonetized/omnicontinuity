use super::*;
use std::{
    cell::RefCell,
    io::{self, Read, Write},
    rc::Rc,
};

fn scratch() -> tempfile::TempDir {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-data");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::tempdir_in(base).unwrap()
}
fn read_file(bytes: &[u8], directory: &Path) -> File {
    let mut file = tempfile::tempfile_in(directory).unwrap();
    file.write_all(bytes).unwrap();
    file.rewind().unwrap();
    file
}
fn unpack(bytes: &[u8], directory: &Path, limit: u64) -> Result<Vec<u8>> {
    let input = read_file(bytes, directory);
    let mut output = tempfile::tempfile_in(directory).unwrap();
    archive::decode_dvzip(input, &mut output, limit)?;
    output.rewind()?;
    let mut bytes = Vec::new();
    output.read_to_end(&mut bytes)?;
    Ok(bytes)
}
fn fixture_file(directory: &Path) -> PathBuf {
    let path = directory.join("hello.txt");
    std::fs::write(&path, b"hello\n").unwrap();
    path
}
fn transfer_id() -> &'static str {
    "00112233-4455-4677-8899-AABBCCDDEEFF"
}
fn address() -> SocketAddrV6 {
    SocketAddrV6::new("fe80::1".parse().unwrap(), 8771, 0, 33)
}

#[test]
fn helper_discover_binary_plist_fixture() {
    let bytes = discover_body().unwrap();
    let expected: &[u8] = &[
        b'b', b'p', b'l', b'i', b's', b't', b'0', b'0', 0xd1, 1, 2, 0x5f, 0x10, 18, b'D', b'e',
        b'v', b'i', b'c', b'e', b'S', b'u', b'p', b'p', b'o', b'r', b't', b'F', b'l', b'a', b'g',
        b's', 0x12, 0, 1, 0xb3, 0xfb, 8, 11, 32, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 3, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 37,
    ];
    assert_eq!(bytes, expected);
    assert_eq!(
        Value::from_reader(Cursor::new(bytes))
            .unwrap()
            .get("DeviceSupportFlags")
            .and_then(Value::as_unsigned_integer),
        Some(111611)
    );
}

#[test]
fn helper_ask_fields_and_utis() {
    let dir = scratch();
    let path = fixture_file(dir.path());
    let bytes = ask_body(
        "aabbccddeeff",
        "My Mac",
        transfer_id(),
        &files(&[path]).unwrap(),
    )
    .unwrap();
    assert!(bytes.starts_with(b"bplist00"));
    let ask = Value::from_reader(Cursor::new(bytes)).unwrap();
    assert_eq!(validate_ask(&ask, 6).unwrap(), transfer_id());
    assert!(validate_ask(&ask, 5).is_err());
    assert_eq!(
        ask.get("SenderID").and_then(Value::as_string),
        Some("aabbccddeeff")
    );
    assert_eq!(
        ask.get("BundleID").and_then(Value::as_string),
        Some("com.apple.finder")
    );
    assert_eq!(
        ask.get("TransferType").unwrap(),
        &dict([("files", dict([]))])
    );
    assert_eq!(ask.get("Items").and_then(Value::as_array).unwrap().len(), 0);
    assert_eq!(
        ask.get("ConvertMediaFormats").and_then(Value::as_boolean),
        Some(false)
    );
    let entry = &ask.get("Files").and_then(Value::as_array).unwrap()[0];
    assert_eq!(
        entry,
        &dict([
            ("FileName", "hello.txt".into()),
            ("FileType", "public.plain-text".into()),
            ("FileSize", 6u64.into()),
            ("FileBomPath", "./hello.txt".into()),
            ("FileIsDirectory", false.into()),
            ("ConvertMediaFormats", false.into()),
        ])
    );
    for (file, expected) in [
        ("A.JPG", "public.jpeg"),
        ("a.png", "public.png"),
        ("a.heic", "public.heic"),
        ("a.mov", "com.apple.quicktime-movie"),
        ("a.mp4", "public.mpeg-4"),
        ("a.pdf", "com.adobe.pdf"),
        ("a.bin", "public.data"),
    ] {
        assert_eq!(uti(file), expected);
    }
}

#[test]
fn helper_post_header_bytes() {
    let mut bytes = Vec::new();
    http::post(&mut bytes, "/Discover", &address(), b"abc").unwrap();
    assert_eq!(bytes, b"POST /Discover HTTP/1.1\r\nHost: [fe80::1]:8771\r\nContent-Length: 3\r\nContent-Type: application/octet-stream\r\nConnection: keep-alive\r\nAccept: */*\r\nUser-Agent: AirDrop/1.0\r\nAccept-Language: en-us\r\nAccept-Encoding: br, gzip, deflate\r\n\r\nabc");
}

#[test]
fn helper_upload_chunk_headers_and_terminal_bytes() {
    let mut bytes = Vec::new();
    http::upload_headers(&mut bytes, &address(), 6, transfer_id()).unwrap();
    let mut chunked = http::ChunkedWriter::new(&mut bytes);
    chunked.write_all(b"abc").unwrap();
    chunked.finish().unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(text.starts_with("POST /Upload HTTP/1.1\r\n"));
    assert!(text.contains("Content-Type: application/x-dvzip\r\n"));
    assert!(text.contains("TotalBytes: 6\r\n"));
    assert!(text.contains(&format!("TransferID: {}\r\n", transfer_id())));
    assert!(!text.contains("Content-Length"));
    assert!(text.ends_with("\r\n\r\n3\r\nabc\r\n0\r\n\r\n"));
    let mut input = Cursor::new(bytes);
    let head = http::read_head(&mut input).unwrap();
    let mut body = Vec::new();
    http::read_body(&mut input, &head, &mut body, 3).unwrap();
    assert_eq!(body, b"abc");
}

#[test]
fn helper_dvzip_stored_and_zlib_known_bytes() {
    let dir = scratch();
    let stored = b"\x80\0\0\x05hello";
    let compressed = b"\0\0\0\x0d\x78\x9c\xcb\x48\xcd\xc9\xc9\x07\0\x06\x2c\x02\x15";
    assert_eq!(unpack(stored, dir.path(), 100).unwrap(), b"hello");
    assert_eq!(unpack(compressed, dir.path(), 100).unwrap(), b"hello");
    let mut encoded = archive::DvzipWriter::new(Vec::new());
    encoded.write_all(b"hello").unwrap();
    assert_eq!(encoded.finish().unwrap(), stored);
    let repetitive = vec![42; 2 * 65536 + 3];
    let mut encoder = archive::DvzipWriter::new(Vec::new());
    encoder.write_all(&repetitive).unwrap();
    let bytes = encoder.finish().unwrap();
    assert_eq!(bytes[0] & 0x80, 0);
    assert_eq!(
        unpack(&bytes, dir.path(), repetitive.len() as u64).unwrap(),
        repetitive
    );
}

#[test]
fn dvzip_rejects_truncation_bombs_and_trailing_compressed_bytes() {
    let dir = scratch();
    let compressed = b"\0\0\0\x0d\x78\x9c\xcb\x48\xcd\xc9\xc9\x07\0\x06\x2c\x02\x15";
    for size in 0..compressed.len() {
        assert!(
            unpack(&compressed[..size], dir.path(), 100).is_err(),
            "accepted truncated length {size}"
        );
    }
    assert!(unpack(compressed, dir.path(), 4).is_err());
    assert!(unpack(b"\x80\0\0\x05hell", dir.path(), 100).is_err());
    let mut trailing = compressed.to_vec();
    trailing[3] += 1;
    trailing.push(0);
    assert!(unpack(&trailing, dir.path(), 100).is_err());
    assert!(unpack(b"\0\0\0\0", dir.path(), 100).is_err());
}

#[test]
fn helper_odc_cpio_roundtrip_with_libarchive_and_collision() {
    let dir = scratch();
    let input = fixture_file(dir.path());
    let mut bytes = Vec::new();
    archive::write_cpio(&mut bytes, files(&[input]).unwrap()).unwrap();
    assert_eq!(&bytes[..6], b"070707");
    assert_eq!(&bytes[59..65], b"000014");
    assert_eq!(&bytes[65..76], b"00000000006");
    assert_eq!(&bytes[76..88], b"./hello.txt\0");
    let stage = tempfile::tempdir_in(dir.path()).unwrap();
    let paths = archive::extract(
        &read_file(&bytes, dir.path()),
        dir.path(),
        stage.path(),
        1000,
    )
    .unwrap();
    assert_eq!(paths, [dir.path().join("hello-1.txt")]);
    assert_eq!(std::fs::read(&paths[0]).unwrap(), b"hello\n");
    assert_eq!(
        std::fs::read(dir.path().join("hello.txt")).unwrap(),
        b"hello\n"
    );
}

#[test]
fn cpio_rejects_unsafe_names_special_files_and_truncated_members() {
    let dir = scratch();
    let input = fixture_file(dir.path());
    let mut bytes = Vec::new();
    archive::write_cpio(&mut bytes, files(&[input]).unwrap()).unwrap();
    for name in [b"../bad.txt\0\0".as_slice(), b"/evil.txt\0\0\0".as_slice()] {
        let mut bad = bytes.clone();
        bad[76..88].copy_from_slice(name);
        assert!(
            archive::extract(&read_file(&bad, dir.path()), dir.path(), dir.path(), 1000).is_err()
        );
    }
    let mut symlink = bytes.clone();
    symlink[18..24].copy_from_slice(b"120777");
    assert!(
        archive::extract(
            &read_file(&symlink, dir.path()),
            dir.path(),
            dir.path(),
            1000
        )
        .is_err()
    );
    assert!(
        archive::extract(
            &read_file(&bytes[..90], dir.path()),
            dir.path(),
            dir.path(),
            1000
        )
        .is_err()
    );
    assert!(archive::extract(&read_file(&bytes, dir.path()), dir.path(), dir.path(), 5).is_err());
    assert!(!dir.path().join("hello-1.txt").exists());
}

#[test]
fn http_stream_boundaries_and_chunk_extensions() {
    let mut bytes = Cursor::new(b"POST /Ask HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3;extension=yes\r\nabc\r\n0\r\nX-Trace: done\r\n\r\nPOST /Upload HTTP/1.1\r\nContent-Length: 1\r\n\r\nx");
    let head = http::read_head(&mut bytes).unwrap();
    let mut body = Vec::new();
    http::read_body(&mut bytes, &head, &mut body, 3).unwrap();
    assert_eq!(body, b"abc");
    let head = http::read_head(&mut bytes).unwrap();
    assert_eq!(head.request_path().unwrap(), "/Upload");
    body.clear();
    http::read_body(&mut bytes, &head, &mut body, 1).unwrap();
    assert_eq!(body, b"x");
}

#[test]
fn http_rejects_ambiguous_oversized_and_truncated_frames() {
    let frames = [
        "POST /Ask HTTP/1.1\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\nx",
        "POST /Ask HTTP/1.1\r\nContent-Length: -1\r\n\r\n",
        "POST /Ask HTTP/1.1\r\nContent-Length: 3\r\n\r\na",
        "POST /Ask HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nxXX0\r\n\r\n",
        "POST /Ask HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
        "POST /Ask HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcx\r\n0\r\n\r\n",
    ];
    for frame in frames {
        let mut input = Cursor::new(frame);
        let head = http::read_head(&mut input).unwrap();
        assert!(
            http::read_body(&mut input, &head, &mut Vec::new(), 3).is_err(),
            "accepted {frame:?}"
        );
    }
    for frame in [
        "POST /Ask HTTP/1.1\r\nContent-Length: 0\r\ncontent-length: 0\r\n\r\n",
        "POST /Ask HTTP/1.1\r\n bad: header\r\n\r\n",
        "POST /Ask HTTP/1.1\n\n",
    ] {
        assert!(http::read_head(&mut Cursor::new(frame)).is_err());
    }
    let huge = format!("POST /Ask HTTP/1.1\r\nLong: {}\r\n\r\n", "a".repeat(9000));
    assert!(http::read_head(&mut Cursor::new(huge)).is_err());
}

#[derive(Clone)]
struct Script {
    input: Cursor<Vec<u8>>,
    output: Rc<RefCell<Vec<u8>>>,
}
impl Script {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            input: Cursor::new(bytes),
            output: Rc::default(),
        }
    }
}
impl Read for Script {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.input.read(out)
    }
}
impl Write for Script {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.output.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn request_transfer(directory: &Path, transfer: &str) -> Vec<u8> {
    let path = fixture_file(directory);
    let mut input = Vec::new();
    http::post(
        &mut input,
        "/Ask",
        &address(),
        &ask_body(
            "aabbccddeeff",
            "Test",
            transfer_id(),
            &files(&[&path]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    http::upload_headers(&mut input, &address(), 6, transfer).unwrap();
    let mut dvzip = archive::DvzipWriter::new(http::ChunkedWriter::new(&mut input));
    archive::write_cpio(&mut dvzip, files(&[path]).unwrap()).unwrap();
    dvzip.finish().unwrap().finish().unwrap();
    input
}

#[test]
fn offline_ask_upload_same_connection_writes_to_caller_directory() {
    let dir = scratch();
    let staging = tempfile::tempdir_in(dir.path()).unwrap();
    let script = Script::new(request_transfer(staging.path(), transfer_id()));
    let output = script.output.clone();
    let paths = serve_connection(
        script,
        dir.path(),
        staging.path(),
        &ReceiveOptions::default(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(paths, [dir.path().join("hello.txt")]);
    assert_eq!(std::fs::read(&paths[0]).unwrap(), b"hello\n");
    let mut responses = Cursor::new(output.borrow().clone());
    let ask = http::read_message(&mut responses, SMALL_LIMIT).unwrap();
    assert_eq!(ask.status().unwrap(), 200);
    let value = Value::from_reader(Cursor::new(ask.body)).unwrap();
    assert_eq!(
        value.get("ReceiverMediaCapabilities"),
        Some(&Value::Data(b"{\"Version\": 1}".to_vec()))
    );
    assert_eq!(
        http::read_message(&mut responses, SMALL_LIMIT)
            .unwrap()
            .status()
            .unwrap(),
        200
    );
}

#[test]
fn upload_requires_accepted_ask_and_matching_transfer_id() {
    let dir = scratch();
    let script = Script::new(request_transfer(
        dir.path(),
        "FFEEDDCC-BBAA-4988-7766-554433221100",
    ));
    let output = script.output.clone();
    assert!(
        serve_connection(script, dir.path(), dir.path(), &ReceiveOptions::default())
            .unwrap()
            .is_none()
    );
    let responses = String::from_utf8_lossy(&output.borrow()).into_owned();
    assert!(responses.contains("400 Bad Request"));
    let mut request = Vec::new();
    http::upload_headers(&mut request, &address(), 0, transfer_id()).unwrap();
    let script = Script::new(request);
    let output = script.output.clone();
    assert!(
        serve_connection(script, dir.path(), dir.path(), &ReceiveOptions::default())
            .unwrap()
            .is_none()
    );
    assert!(output.borrow().starts_with(b"HTTP/1.1 403"));
    assert!(!dir.path().join("hello-1.txt").exists());
}

#[test]
fn discover_response_closes_connection_without_accepting_files() {
    let dir = scratch();
    let mut bytes = Vec::new();
    http::post(
        &mut bytes,
        "/Discover",
        &address(),
        &discover_body().unwrap(),
    )
    .unwrap();
    let script = Script::new(bytes);
    let output = script.output.clone();
    assert!(
        serve_connection(script, dir.path(), dir.path(), &ReceiveOptions::default())
            .unwrap()
            .is_none()
    );
    let response =
        http::read_message(&mut Cursor::new(output.borrow().clone()), SMALL_LIMIT).unwrap();
    assert_eq!(response.status().unwrap(), 200);
    assert!(response.body.starts_with(b"bplist00"));
}

#[test]
fn file_validation_and_unambiguous_name_selection() {
    let dir = scratch();
    let path = fixture_file(dir.path());
    assert!(files(&[&path, &path]).is_err());
    assert!(files(&[dir.path()]).is_err());
    assert!(files::<&Path>(&[]).is_err());
    let peer = |name: &str| Peer {
        name: name.into(),
        address: address(),
    };
    assert_eq!(
        choose_peer("mac", vec![peer("Mac Pro"), peer("Mac")])
            .unwrap()
            .name,
        "Mac"
    );
    assert!(matches!(
        choose_peer("mac", vec![peer("Mac Pro"), peer("Mac Air")]),
        Err(Error::AmbiguousPeer(_))
    ));
    assert!(matches!(
        choose_peer("missing", vec![peer("Mac")]),
        Err(Error::PeerNotFound(_))
    ));
    assert!(choose_peer("", vec![peer("Mac")]).is_err());
}

#[test]
fn gzip_fallback_is_bounded_and_rejects_truncation_and_trailing_data() {
    let dir = scratch();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
    encoder.write_all(b"hello gzip").unwrap();
    let bytes = encoder.finish().unwrap();
    assert_eq!(unpack(&bytes, dir.path(), 100).unwrap(), b"hello gzip");
    assert!(unpack(&bytes, dir.path(), 5).is_err());
    assert!(unpack(&bytes[..bytes.len() - 1], dir.path(), 100).is_err());
    let mut bad = bytes;
    bad.push(1);
    assert!(unpack(&bad, dir.path(), 100).is_err());
}

#[test]
fn expect_continue_is_written_only_after_framing_validation() {
    let mut input = Cursor::new(
        b"POST /Upload HTTP/1.1\r\nExpect: 100-continue\r\nContent-Length: 3\r\n\r\nabc",
    );
    let head = http::read_head(&mut input).unwrap();
    http::validate_framing(&head, 3).unwrap();
    let mut output = Vec::new();
    http::continue_request(&mut output, &head).unwrap();
    assert_eq!(output, b"HTTP/1.1 100 Continue\r\n\r\n");
    assert!(http::validate_framing(&head, 2).is_err());
    let mut input = Cursor::new(
        b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc",
    );
    let response = http::read_message(&mut input, 3).unwrap();
    assert_eq!(response.status().unwrap(), 200);
    assert_eq!(response.body, b"abc");
}

#[test]
fn receiver_rejects_bad_requests_and_link_transfers() {
    let dir = scratch();
    let script = Script::new(b"GET / HTTP/1.1\r\nContent-Length: 0\r\n\r\n".to_vec());
    let output = script.output.clone();
    assert!(
        serve_connection(script, dir.path(), dir.path(), &ReceiveOptions::default())
            .unwrap()
            .is_none()
    );
    assert!(output.borrow().starts_with(b"HTTP/1.1 400"));
    let link = dict([
        ("TransferID", dict([("id", transfer_id().into())])),
        ("TransferType", dict([("links", dict([]))])),
    ]);
    assert!(validate_ask(&link, 100).is_err());
}

#[test]
fn native_tls_discover_ask_upload_over_loopback_without_radio() {
    let dir = scratch();
    let stage = tempfile::tempdir_in(dir.path()).unwrap();
    let identity = transport::Identity::new().unwrap();
    let listener = std::net::TcpListener::bind("[::1]:0").unwrap();
    let endpoint = listener.local_addr().unwrap();
    let destination = dir.path().to_owned();
    let staging = stage.path().to_owned();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (socket, _) = listener.accept().unwrap();
            let stream = identity
                .accept(socket, Instant::now() + Duration::from_secs(10))
                .unwrap();
            if let Some(paths) =
                serve_connection(stream, &destination, &staging, &ReceiveOptions::default())
                    .unwrap()
            {
                return paths;
            }
        }
        panic!("no upload");
    });
    let connect = || {
        let mut builder =
            openssl::ssl::SslConnector::builder(openssl::ssl::SslMethod::tls_client()).unwrap();
        builder.set_verify(openssl::ssl::SslVerifyMode::NONE);
        let socket = std::net::TcpStream::connect(endpoint).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        builder
            .build()
            .configure()
            .unwrap()
            .verify_hostname(false)
            .connect("localhost", socket)
            .unwrap()
    };
    let mut discovery = BufReader::new(connect());
    http::post(
        discovery.get_mut(),
        "/Discover",
        &address(),
        &discover_body().unwrap(),
    )
    .unwrap();
    assert_eq!(
        http::read_message(&mut discovery, SMALL_LIMIT)
            .unwrap()
            .status()
            .unwrap(),
        200
    );
    drop(discovery);
    let mut transfer = BufReader::new(connect());
    let source = fixture_file(stage.path());
    let ask = ask_body(
        "001122334455",
        "Loopback",
        transfer_id(),
        &files(&[&source]).unwrap(),
    )
    .unwrap();
    http::post(transfer.get_mut(), "/Ask", &address(), &ask).unwrap();
    assert_eq!(
        http::read_message(&mut transfer, SMALL_LIMIT)
            .unwrap()
            .status()
            .unwrap(),
        200
    );
    http::upload_headers(transfer.get_mut(), &address(), 6, transfer_id()).unwrap();
    let mut dvzip = archive::DvzipWriter::new(http::ChunkedWriter::new(transfer.get_mut()));
    archive::write_cpio(&mut dvzip, files(&[source]).unwrap()).unwrap();
    dvzip.finish().unwrap().finish().unwrap();
    assert_eq!(
        http::read_message(&mut transfer, SMALL_LIMIT)
            .unwrap()
            .status()
            .unwrap(),
        200
    );
    let paths = server.join().unwrap();
    assert_eq!(paths, [dir.path().join("hello.txt")]);
    assert_eq!(std::fs::read(&paths[0]).unwrap(), b"hello\n");
}
