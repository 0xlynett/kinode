use crate::kinode::process::downloads::{
    ChunkRequest, DownloadCompleteRequest, DownloadError, DownloadRequests, HashMismatch,
    LocalDownloadRequest, ProgressUpdate, RemoteDownloadRequest, SizeUpdate,
};
use kinode_process_lib::*;
use kinode_process_lib::{
    print_to_terminal, println, timer,
    vfs::{open_dir, open_file, Directory, File, SeekFrom},
};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::str::FromStr;

pub mod ft_worker_lib;

wit_bindgen::generate!({
    path: "target/wit",
    generate_unused_types: true,
    world: "app-store-sys-v0",
    additional_derives: [serde::Deserialize, serde::Serialize],
});

const CHUNK_SIZE: u64 = 262144; // 256KB

call_init!(init);
fn init(our: Address) {
    let Ok(Message::Request {
        source: parent_process,
        body,
        ..
    }) = await_message()
    else {
        panic!("ft_worker: got bad init message");
    };

    if parent_process.node() != our.node() {
        panic!("ft_worker: got bad init message source");
    }

    // killswitch timer, 2 minutes. sender or receiver gets killed/cleaned up.
    timer::set_timer(120000, None);

    let start = std::time::Instant::now();

    let req: DownloadRequests =
        serde_json::from_slice(&body).expect("ft_worker: got unparseable init message");

    match req {
        DownloadRequests::LocalDownload(local_request) => {
            let LocalDownloadRequest {
                package_id,
                desired_version_hash,
                ..
            } = local_request;
            match handle_receiver(
                &parent_process,
                &package_id.to_process_lib(),
                &desired_version_hash,
            ) {
                Ok(_) => print_to_terminal(
                    1,
                    &format!(
                        "ft_worker: receive downloaded package in {}ms",
                        start.elapsed().as_millis()
                    ),
                ),
                Err(e) => print_to_terminal(1, &format!("ft_worker: receive error: {}", e)),
            }
        }
        DownloadRequests::RemoteDownload(remote_request) => {
            let RemoteDownloadRequest {
                package_id,
                desired_version_hash,
                worker_address,
            } = remote_request;

            match handle_sender(
                &worker_address,
                &package_id.to_process_lib(),
                &desired_version_hash,
            ) {
                Ok(_) => print_to_terminal(
                    1,
                    &format!(
                        "ft_worker: sent package to {} in {}ms",
                        worker_address,
                        start.elapsed().as_millis()
                    ),
                ),
                Err(e) => print_to_terminal(1, &format!("ft_worker: send error: {}", e)),
            }
        }
        _ => println!("ft_worker: got unexpected message"),
    }
}

fn handle_sender(worker: &str, package_id: &PackageId, version_hash: &str) -> anyhow::Result<()> {
    let target_worker = Address::from_str(worker)?;

    let filename = format!(
        "/app_store:sys/downloads/{}:{}/{}.zip",
        package_id.package_name, package_id.publisher_node, version_hash
    );

    let mut file = open_file(&filename, false, None)?;
    let size = file.metadata()?.len;
    let num_chunks = (size as f64 / CHUNK_SIZE as f64).ceil() as u64;

    Request::new()
        .body(serde_json::to_vec(&DownloadRequests::Size(SizeUpdate {
            package_id: package_id.clone().into(),
            size,
        }))?)
        .target(target_worker.clone())
        .send()?;
    file.seek(SeekFrom::Start(0))?;

    for i in 0..num_chunks {
        send_chunk(&mut file, i, size, &target_worker, package_id, version_hash)?;
    }

    Ok(())
}

fn handle_receiver(
    parent_process: &Address,
    package_id: &PackageId,
    version_hash: &str,
) -> anyhow::Result<()> {
    // TODO: write to a temporary location first, then check hash as we go, then rename to final location.

    let package_dir = open_or_create_dir(&format!(
        "/app_store:sys/downloads/{}:{}/",
        package_id.package_name,
        package_id.publisher(),
    ))?;

    let timer_address = Address::from_str("our@timer:distro:sys")?;

    let mut file = open_or_create_file(&format!("{}{}.zip", &package_dir.path, version_hash))?;
    let mut size: Option<u64> = None;
    let mut hasher = Sha256::new();

    loop {
        let message = await_message()?;
        if *message.source() == timer_address {
            return Ok(());
        }
        let Message::Request { body, .. } = message else {
            return Err(anyhow::anyhow!("ft_worker: got bad message"));
        };

        let req: DownloadRequests = serde_json::from_slice(&body)?;

        match req {
            DownloadRequests::Chunk(chunk) => {
                handle_chunk(&mut file, &chunk, parent_process, &mut size, &mut hasher)?;
                if let Some(s) = size {
                    if chunk.offset + chunk.length >= s {
                        let recieved_hash = format!("{:x}", hasher.finalize());

                        if recieved_hash != version_hash {
                            print_to_terminal(
                                1,
                                &format!(
                                    "ft_worker: {} hash mismatch: desired: {} != actual: {}",
                                    package_id.to_string(),
                                    version_hash,
                                    recieved_hash
                                ),
                            );
                            let req = DownloadCompleteRequest {
                                package_id: package_id.clone().into(),
                                version_hash: version_hash.to_string(),
                                error: Some(DownloadError::HashMismatch(HashMismatch {
                                    desired: version_hash.to_string(),
                                    actual: recieved_hash,
                                })),
                            };
                            Request::new()
                                .body(serde_json::to_vec(&DownloadRequests::DownloadComplete(
                                    req,
                                ))?)
                                .target(parent_process.clone())
                                .send()?;
                        }

                        let manifest_filename =
                            format!("{}{}.json", package_dir.path, version_hash);

                        let contents = file.read()?;
                        extract_and_write_manifest(&contents, &manifest_filename)?;

                        Request::new()
                            .body(serde_json::to_vec(&DownloadRequests::DownloadComplete(
                                DownloadCompleteRequest {
                                    package_id: package_id.clone().into(),
                                    version_hash: version_hash.to_string(),
                                    error: None,
                                },
                            ))?)
                            .target(parent_process.clone())
                            .send()?;
                        return Ok(());
                    }
                }
            }
            DownloadRequests::Size(update) => {
                size = Some(update.size);
            }
            _ => println!("ft_worker: got unexpected message"),
        }
    }
}

fn send_chunk(
    file: &mut File,
    chunk_index: u64,
    total_size: u64,
    target: &Address,
    package_id: &PackageId,
    version_hash: &str,
) -> anyhow::Result<()> {
    let offset = chunk_index * CHUNK_SIZE;
    let length = CHUNK_SIZE.min(total_size - offset);

    let mut buffer = vec![0; length as usize];
    // this extra seek might be unnecessary. fix multireads per process in vfs
    file.seek(SeekFrom::Start(offset))?;
    file.read_at(&mut buffer)?;

    Request::new()
        .body(serde_json::to_vec(&DownloadRequests::Chunk(
            ChunkRequest {
                package_id: package_id.clone().into(),
                version_hash: version_hash.to_string(),
                offset,
                length,
            },
        ))?)
        .target(target.clone())
        .blob_bytes(buffer)
        .send()?;
    Ok(())
}

fn handle_chunk(
    file: &mut File,
    chunk: &ChunkRequest,
    parent: &Address,
    size: &mut Option<u64>,
    hasher: &mut Sha256,
) -> anyhow::Result<()> {
    let bytes = if let Some(blob) = get_blob() {
        blob.bytes
    } else {
        return Err(anyhow::anyhow!("ft_worker: got no blob"));
    };

    file.write_all(&bytes)?;
    hasher.update(&bytes);

    if let Some(total_size) = size {
        // let progress = ((chunk.offset + chunk.length) as f64 / *total_size as f64 * 100.0) as u64;

        Request::new()
            .body(serde_json::to_vec(&DownloadRequests::Progress(
                ProgressUpdate {
                    package_id: chunk.package_id.clone(),
                    downloaded: chunk.offset + chunk.length,
                    total: *total_size,
                    version_hash: chunk.version_hash.clone(),
                },
            ))?)
            .target(parent.clone())
            .send()?;
    }

    Ok(())
}

fn extract_and_write_manifest(file_contents: &[u8], manifest_path: &str) -> anyhow::Result<()> {
    let reader = std::io::Cursor::new(file_contents);
    let mut archive = zip::ZipArchive::new(reader)?;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        if file.name() == "manifest.json" {
            let mut contents = String::new();
            file.read_to_string(&mut contents)?;

            let manifest_file = open_or_create_file(&manifest_path)?;
            manifest_file.write(contents.as_bytes())?;

            print_to_terminal(1, "Extracted and wrote manifest.json");
            break;
        }
    }

    Ok(())
}

/// helper function for vfs files, open if exists, if not create
fn open_or_create_file(path: &str) -> anyhow::Result<File> {
    match open_file(path, false, None) {
        Ok(file) => Ok(file),
        Err(_) => match open_file(path, true, None) {
            Ok(file) => Ok(file),
            Err(_) => Err(anyhow::anyhow!("could not create file")),
        },
    }
}

/// helper function for vfs directories, open if exists, if not create
fn open_or_create_dir(path: &str) -> anyhow::Result<Directory> {
    match open_dir(path, true, None) {
        Ok(dir) => Ok(dir),
        Err(_) => match open_dir(path, false, None) {
            Ok(dir) => Ok(dir),
            Err(_) => Err(anyhow::anyhow!("could not create dir")),
        },
    }
}

impl crate::kinode::process::main::PackageId {
    pub fn to_process_lib(&self) -> kinode_process_lib::PackageId {
        kinode_process_lib::PackageId::new(&self.package_name, &self.publisher_node)
    }

    pub fn from_process_lib(package_id: &kinode_process_lib::PackageId) -> Self {
        Self {
            package_name: package_id.package_name.clone(),
            publisher_node: package_id.publisher_node.clone(),
        }
    }
}

// Conversion from wit PackageId to process_lib's PackageId
impl From<crate::kinode::process::downloads::PackageId> for kinode_process_lib::PackageId {
    fn from(package_id: crate::kinode::process::downloads::PackageId) -> Self {
        kinode_process_lib::PackageId::new(&package_id.package_name, &package_id.publisher_node)
    }
}

// Conversion from process_lib's PackageId to wit PackageId
impl From<kinode_process_lib::PackageId> for crate::kinode::process::downloads::PackageId {
    fn from(package_id: kinode_process_lib::PackageId) -> Self {
        Self {
            package_name: package_id.package_name,
            publisher_node: package_id.publisher_node,
        }
    }
}
