use super::*;
use crate::api::schemas::sophon_manifests::Manifest;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::Arc,
};

const OLD_VERSION: &[u8] = b"old version-detection file";
const NEW_VERSION: &[u8] = b"new version-detection file";
const NEW_DATA: &[u8] = b"updated game data";

struct Fixture {
    root: PathBuf,
    game: PathBuf,
    patcher: SophonPatcher,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("sophon-update-{}", rand::random::<u128>()));
        let game = root.join("game");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::write(game.join("globalgamemanagers"), OLD_VERSION).unwrap();

        let patch_assets = [("globalgamemanagers", NEW_VERSION), ("data.bin", NEW_DATA)]
            .into_iter()
            .map(|(name, contents)| SophonPatchAssetProperty {
                asset_name: name.into(),
                asset_size: contents.len() as u64,
                asset_hash_md5: crate::md5_hash_str(contents),
                asset_patch_chunks: HashMap::from([(
                    "1.0.0".into(),
                    SophonPatchAssetChunk {
                        patch_name: name.into(),
                        patch_size: contents.len() as u64,
                        patch_length: contents.len() as u64,
                        patch_md5: crate::md5_hash_str(contents),
                        ..Default::default()
                    },
                )]),
            })
            .collect();
        let download = DownloadInfo {
            encryption: 0,
            password: String::new(),
            compression: 0,
            url_prefix: "http://127.0.0.1:9".into(),
            url_suffix: String::new(),
        };
        let patcher = SophonPatcher {
            client: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            patch_manifest: SophonPatchProto {
                patch_assets,
                ..Default::default()
            },
            diff_info: SophonDiff {
                category_id: String::new(),
                category_name: String::new(),
                matching_field: "game".into(),
                manifest: Manifest {
                    id: String::new(),
                    checksum: String::new(),
                    compressed_size: "0".into(),
                    uncompressed_size: "0".into(),
                },
                diff_download: download.clone(),
                manifest_download: download,
                stats: Default::default(),
            },
            temp_folder: root.join("temp"),
            patch_function: None,
            last_file_suffix: Some("globalgamemanagers".into()),
            check_free_space: false,
            patches_in_memory: false,
            patch_queue_mem_limit: None,
        };
        patcher.create_temp_dirs().unwrap();
        Self {
            root,
            game,
            patcher,
        }
    }

    fn cache(&self, contents: &[u8]) {
        let file = self
            .patcher
            .patches_temp()
            .join(format!("{}.bin", crate::md5_hash_str(contents)));
        std::fs::write(file, contents).unwrap();
    }

    fn run(&self) -> (Result<(), SophonError>, Vec<Update>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let result = self.patcher.update(
            &self.game,
            Version::new(1, 0, 0),
            2,
            Box::new(move |event| captured.lock().unwrap().push(event)),
        );
        (
            result,
            Arc::try_unwrap(events).unwrap().into_inner().unwrap(),
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn server(responses: Vec<Option<&'static [u8]>>) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        for body in responses {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline, "Missing test request");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(err) => panic!("Failed to accept test request: {err}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 16384, "Oversized test request");
            }
            let (status, body) = match body {
                Some(body) => ("206 Partial Content", body),
                None => ("503 Service Unavailable", &[][..]),
            };
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        }
    });
    (url, handle)
}

#[test]
fn exhausted_download_retries_preserve_version_file_and_cache() {
    let mut fixture = Fixture::new();
    fixture.cache(NEW_VERSION);
    let (url, server) = server(vec![None; DEFAULT_CHUNK_RETRIES as usize + 1]);
    fixture.patcher.diff_info.diff_download.url_prefix = url;
    let (result, events) = fixture.run();
    server.join().unwrap();

    assert!(matches!(
        result,
        Err(SophonError::UpdateIncomplete {
            patched_files: 1,
            total_files: 2
        })
    ));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Update::PatchingFinished))
    );
    assert_eq!(
        std::fs::read(fixture.game.join("globalgamemanagers")).unwrap(),
        OLD_VERSION
    );
    assert_eq!(
        std::fs::read(fixture.patcher.files_temp().join("last_file.tmp")).unwrap(),
        NEW_VERSION
    );
}

#[test]
fn invalid_source_file_prevents_success() {
    let mut fixture = Fixture::new();
    fixture.cache(NEW_VERSION);
    let chunk = fixture.patcher.patch_manifest.patch_assets[1]
        .asset_patch_chunks
        .get_mut("1.0.0")
        .unwrap();
    chunk.original_file_name = "missing-source.bin".into();
    chunk.original_file_length = 1;
    chunk.original_file_md5 = crate::md5_hash_str(b"x");

    let (result, events) = fixture.run();
    assert!(result.is_err());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Update::PatchingFinished))
    );
    assert_eq!(
        std::fs::read(fixture.game.join("globalgamemanagers")).unwrap(),
        OLD_VERSION
    );
}

#[test]
fn a_recovered_download_error_does_not_fail_the_update() {
    let mut fixture = Fixture::new();
    fixture.cache(NEW_VERSION);
    let (url, server) = server(vec![None, Some(NEW_DATA)]);
    fixture.patcher.diff_info.diff_download.url_prefix = url;
    let (result, events) = fixture.run();
    server.join().unwrap();

    result.unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Update::DownloadingError(_)))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Update::PatchingFinished))
    );
    assert_eq!(
        std::fs::read(fixture.game.join("globalgamemanagers")).unwrap(),
        NEW_VERSION
    );
    assert_eq!(
        std::fs::read(fixture.game.join("data.bin")).unwrap(),
        NEW_DATA
    );
}

#[test]
fn failure_to_install_version_file_is_returned_and_keeps_staged_file() {
    let fixture = Fixture::new();
    fixture.cache(NEW_VERSION);
    fixture.cache(NEW_DATA);
    let target = fixture.game.join("globalgamemanagers");
    std::fs::remove_file(&target).unwrap();
    std::fs::create_dir(&target).unwrap();

    let (result, events) = fixture.run();
    assert!(result.is_err());
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Update::PatchingFinished))
    );
    assert!(fixture.patcher.files_temp().join("last_file.tmp").is_file());
}

#[test]
fn a_complete_update_commits_version_file_and_can_be_repeated() {
    let fixture = Fixture::new();
    fixture.cache(NEW_VERSION);
    fixture.cache(NEW_DATA);
    for _ in 0..2 {
        let (result, events) = fixture.run();
        result.unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Update::PatchingFinished))
        );
        assert_eq!(
            std::fs::read(fixture.game.join("globalgamemanagers")).unwrap(),
            NEW_VERSION
        );
        assert!(!fixture.patcher.files_temp().join("last_file.tmp").exists());
    }
}
