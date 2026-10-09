//! Local filesystem and feed fixture policy checks.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;
use nzb_web::dir_watcher::DirWatcher;
use nzb_web::log_buffer::LogBuffer;
use nzb_web::nzb_core::db::Database;
use nzb_web::nzb_core::models::JobStatus;
use nzb_web::queue_manager::QueueManager;

#[test]
fn gzip_fixture_is_deterministic_and_uses_the_watch_folder_suffix() {
    let input = b"<nzb><file subject=\"fixture\" /></nzb>";
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(input).unwrap();
    let compressed = encoder.finish().unwrap();
    let mut second_encoder = GzEncoder::new(Vec::new(), Compression::default());
    second_encoder.write_all(input).unwrap();
    let second_compressed = second_encoder.finish().unwrap();
    assert!(!compressed.is_empty());
    assert_eq!(compressed, second_compressed);
    assert!(
        Path::new("release.nzb.gz")
            .to_string_lossy()
            .ends_with(".nzb.gz")
    );
}

#[tokio::test]
async fn existing_gzip_nzb_is_imported_once_and_moved_to_processed() {
    let temp = tempfile::tempdir().unwrap();
    let watch_dir = temp.path().join("watch");
    let incomplete = temp.path().join("incomplete");
    let complete = temp.path().join("complete");
    std::fs::create_dir_all(&watch_dir).unwrap();
    let source = br#"<?xml version="1.0"?><nzb xmlns="http://www.newzbin.com/DTD/2003/nzb"><file subject="watched.txt" date="0" poster="test@test"><groups><group>alt.test</group></groups><segments><segment number="1" bytes="5">watched-1@test</segment></segments></file></nzb>"#;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(source).unwrap();
    let compressed = encoder.finish().unwrap();
    let input = watch_dir.join("watched.nzb.gz");
    std::fs::write(&input, compressed).unwrap();

    let queue = QueueManager::new(
        Vec::new(),
        Database::open_memory().unwrap(),
        incomplete.clone(),
        complete,
        LogBuffer::default(),
        1,
        Vec::new(),
        0,
        0,
        false,
        5,
        true,
        true,
        100.0,
        2,
    );
    let watcher = DirWatcher::new(watch_dir.clone(), queue.clone());
    let watcher_task = tokio::spawn(watcher.run());
    let imported = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if queue.queue_size() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    watcher_task.abort();
    assert!(
        imported.is_ok(),
        "watch folder did not enqueue the gzip NZB"
    );
    assert!(watch_dir.join("processed/watched.nzb.gz").exists());
    assert!(!input.exists());
    assert_eq!(queue.get_jobs()[0].status, JobStatus::Downloading);
}

fn watch_queue(temp: &Path) -> std::sync::Arc<QueueManager> {
    QueueManager::new(
        Vec::new(),
        Database::open_memory().unwrap(),
        temp.join("incomplete"),
        temp.join("complete"),
        LogBuffer::default(),
        1,
        Vec::new(),
        0,
        0,
        false,
        5,
        true,
        true,
        100.0,
        2,
    )
}

fn fixture_nzb(subject: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0"?><nzb xmlns="http://www.newzbin.com/DTD/2003/nzb"><file subject="{subject}" date="0" poster="test@test"><groups><group>alt.test</group></groups><segments><segment number="1" bytes="5">{subject}-1@test</segment></segments></file></nzb>"#
    )
    .into_bytes()
}

async fn wait_for_queue_size(queue: &QueueManager, expected: usize) -> bool {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if queue.queue_size() == expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn existing_zip_of_nzbs_is_imported_and_moved_to_processed() {
    use zip::write::SimpleFileOptions;

    let temp = tempfile::tempdir().unwrap();
    let watch_dir = temp.path().join("watch");
    std::fs::create_dir_all(&watch_dir).unwrap();

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, subject) in [("first.nzb", "first.bin"), ("second.nzb", "second.bin")] {
        writer.start_file(name, options).unwrap();
        writer.write_all(&fixture_nzb(subject)).unwrap();
    }
    writer.start_file("readme.txt", options).unwrap();
    writer.write_all(b"not an nzb").unwrap();
    let archive = writer.finish().unwrap().into_inner();
    let input = watch_dir.join("bundle.zip");
    std::fs::write(&input, archive).unwrap();

    let queue = watch_queue(temp.path());
    let watcher_task = tokio::spawn(DirWatcher::new(watch_dir.clone(), queue.clone()).run());
    let imported = wait_for_queue_size(&queue, 2).await;
    watcher_task.abort();

    assert!(
        imported,
        "watch folder did not enqueue both NZBs from the zip"
    );
    let mut names: Vec<String> = queue.get_jobs().into_iter().map(|job| job.name).collect();
    names.sort();
    assert_eq!(names, ["first", "second"]);
    assert!(watch_dir.join("processed/bundle.zip").exists());
    assert!(!input.exists());
}

#[tokio::test]
async fn existing_bzip2_nzb_is_imported_and_moved_to_processed() {
    let temp = tempfile::tempdir().unwrap();
    let watch_dir = temp.path().join("watch");
    std::fs::create_dir_all(&watch_dir).unwrap();

    let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
    encoder.write_all(&fixture_nzb("bz.bin")).unwrap();
    let input = watch_dir.join("packed.nzb.bz2");
    std::fs::write(&input, encoder.finish().unwrap()).unwrap();

    let queue = watch_queue(temp.path());
    let watcher_task = tokio::spawn(DirWatcher::new(watch_dir.clone(), queue.clone()).run());
    let imported = wait_for_queue_size(&queue, 1).await;
    watcher_task.abort();

    assert!(imported, "watch folder did not enqueue the bzip2 NZB");
    assert_eq!(queue.get_jobs()[0].name, "packed");
    assert!(watch_dir.join("processed/packed.nzb.bz2").exists());
    assert!(!input.exists());
}
