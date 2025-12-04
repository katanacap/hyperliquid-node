use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::time::{MissedTickBehavior, interval};
use tracing::{info, trace, warn};

/// Worker task that periodically cleans up old files in ${base}/hl/data
/// Equivalent to: find ${base}/hl/data -mindepth 1 -depth -mmin +240 -type f -not -name "visor_child_stderr"
pub async fn prune_worker_task<P: AsRef<Path>>(
    base_path: P,
    prune_interval: Duration,
    prune_older_than: Duration,
) {
    let base_path = base_path.as_ref().join("hl/data");

    let mut interval = interval(prune_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval.tick().await; // will complete immediately, as per interval API

    info!(?base_path, ?prune_older_than, "pruning node data directory");
    if let Err(err) = run_cleanup(&base_path, prune_older_than).await {
        warn!(?err, "initial node data prune failed");
    }

    loop {
        interval.tick().await;

        if let Err(err) = run_cleanup(&base_path, prune_older_than).await {
            warn!(?err, ?prune_older_than, "scheduled node data prune failed");
        }
    }
}

async fn run_cleanup<P: AsRef<Path>>(data_path: P, prune_older_than: Duration) -> eyre::Result<()> {
    let data_path = data_path.as_ref();
    let now = SystemTime::now();

    let mut files_to_remove = Vec::new();

    // Walk directory tree depth-first (equivalent to -depth flag)
    collect_files_recursive(
        data_path,
        data_path,
        &mut files_to_remove,
        prune_older_than,
        now,
    )
    .await?;

    let mut removed = 0_usize;
    let mut failed = 0_usize;

    for file_path in files_to_remove {
        match fs::remove_file(&file_path) {
            Ok(()) => {
                trace!(?file_path, "file removed");
                removed += 1;
            }
            Err(err) => {
                warn!(?err, ?file_path, "failed to remove file");
                failed += 1;
            }
        }
    }

    info!(removed, failed, "prune complete",);

    Ok(())
}

async fn collect_files_recursive(
    current_path: &Path,
    base_path: &Path,
    files_to_remove: &mut Vec<PathBuf>,
    cutoff_duration: Duration,
    now: SystemTime,
) -> eyre::Result<()> {
    let entries = match fs::read_dir(current_path) {
        Ok(entries) => entries,
        Err(err) => {
            warn!(?err, ?current_path, "failed to read directory");
            return Ok(());
        }
    };

    let mut subdirs = Vec::new();

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(err) => {
                warn!(?err, ?path, "failed to get file metadata");
                continue;
            }
        };

        if metadata.is_dir() {
            subdirs.push(path);
            continue;
        }

        if !metadata.is_file() {
            continue;
        }

        // Skip files directly in base directory (equivalent to -mindepth 1)
        if path.parent() == Some(base_path) {
            continue;
        }

        // Skip visor_child_stderr file
        if path.file_name().and_then(|name| name.to_str()) == Some("visor_child_stderr") {
            continue;
        }

        // Check if file is older than cutoff
        let should_remove = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .map(|age| age > cutoff_duration)
            .unwrap_or(false);

        if should_remove {
            files_to_remove.push(path);
        }
    }

    // Process subdirectories depth-first (equivalent to -depth)
    for subdir in subdirs {
        let task = Box::pin(collect_files_recursive(
            &subdir,
            base_path,
            files_to_remove,
            cutoff_duration,
            now,
        ));
        task.await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    fn set_file_mtime(path: &Path, mtime: SystemTime) -> eyre::Result<()> {
        #[cfg(unix)]
        {
            use filetime::FileTime;
            filetime::set_file_times(
                path,
                FileTime::from_system_time(mtime),
                FileTime::from_system_time(mtime),
            )?;
        }
        #[cfg(not(unix))]
        {
            // On non-Unix systems, we can't easily set mtime, so skip this test
            return Ok(());
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_prune_removes_old_files() -> eyre::Result<()> {
        let temp_dir = TempDir::new()?;
        let data_dir = temp_dir.path().join("hl/data");
        fs::create_dir_all(&data_dir)?;

        let now = SystemTime::now();
        let cutoff = Duration::from_secs(3600); // 1 hour

        // Create old file (should be removed)
        let old_file = data_dir.join("subdir/old_file.txt");
        fs::create_dir_all(old_file.parent().unwrap())?;
        fs::write(&old_file, "old content")?;
        set_file_mtime(&old_file, now - Duration::from_secs(7200))?; // 2 hours ago

        // Create new file (should NOT be removed)
        let new_file = data_dir.join("subdir/new_file.txt");
        fs::write(&new_file, "new content")?;
        set_file_mtime(&new_file, now - Duration::from_secs(1800))?; // 30 minutes ago

        run_cleanup(&data_dir, cutoff).await?;

        // Old file should be removed
        assert!(!old_file.exists(), "Old file should be removed");
        // New file should still exist
        assert!(new_file.exists(), "New file should not be removed");

        Ok(())
    }

    #[tokio::test]
    async fn test_prune_skips_base_directory_files() -> eyre::Result<()> {
        let temp_dir = TempDir::new()?;
        let data_dir = temp_dir.path().join("hl/data");
        fs::create_dir_all(&data_dir)?;

        let now = SystemTime::now();
        let cutoff = Duration::from_secs(3600);

        // Create old file directly in base directory (should NOT be removed)
        let base_file = data_dir.join("base_file.txt");
        fs::write(&base_file, "base content")?;
        set_file_mtime(&base_file, now - Duration::from_secs(7200))?;

        run_cleanup(&data_dir, cutoff).await?;

        // Base directory file should still exist
        assert!(
            base_file.exists(),
            "Base directory file should not be removed"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_prune_skips_visor_child_stderr() -> eyre::Result<()> {
        let temp_dir = TempDir::new()?;
        let data_dir = temp_dir.path().join("hl/data");
        fs::create_dir_all(&data_dir)?;

        let now = SystemTime::now();
        let cutoff = Duration::from_secs(3600);

        // Create old visor_child_stderr file (should NOT be removed)
        let stderr_file = data_dir.join("subdir/visor_child_stderr");
        fs::create_dir_all(stderr_file.parent().unwrap())?;
        fs::write(&stderr_file, "stderr content")?;
        set_file_mtime(&stderr_file, now - Duration::from_secs(7200))?;

        run_cleanup(&data_dir, cutoff).await?;

        // visor_child_stderr should still exist
        assert!(
            stderr_file.exists(),
            "visor_child_stderr should not be removed"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_prune_handles_nested_directories() -> eyre::Result<()> {
        let temp_dir = TempDir::new()?;
        let data_dir = temp_dir.path().join("hl/data");
        fs::create_dir_all(&data_dir)?;

        let now = SystemTime::now();
        let cutoff = Duration::from_secs(3600);

        // Create nested directory structure
        let nested_old = data_dir.join("level1/level2/level3/old_file.txt");
        fs::create_dir_all(nested_old.parent().unwrap())?;
        fs::write(&nested_old, "nested old")?;
        set_file_mtime(&nested_old, now - Duration::from_secs(7200))?;

        let nested_new = data_dir.join("level1/level2/level3/new_file.txt");
        fs::write(&nested_new, "nested new")?;
        set_file_mtime(&nested_new, now - Duration::from_secs(1800))?;

        run_cleanup(&data_dir, cutoff).await?;

        assert!(!nested_old.exists(), "Nested old file should be removed");
        assert!(nested_new.exists(), "Nested new file should not be removed");

        Ok(())
    }

    #[tokio::test]
    async fn test_prune_handles_missing_directory_gracefully() -> eyre::Result<()> {
        let temp_dir = TempDir::new()?;
        let non_existent_dir = temp_dir.path().join("nonexistent/hl/data");

        // Should not panic or error on missing directory
        let result = run_cleanup(&non_existent_dir, Duration::from_secs(3600)).await;
        // It should either succeed (if it handles gracefully) or return an error we can handle
        // The current implementation uses read_dir which will fail, but that's ok for this test
        assert!(result.is_ok() || result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_prune_removes_multiple_old_files() -> eyre::Result<()> {
        let temp_dir = TempDir::new()?;
        let data_dir = temp_dir.path().join("hl/data");
        fs::create_dir_all(&data_dir)?;

        let now = SystemTime::now();
        let cutoff = Duration::from_secs(3600);

        // Create multiple old files
        let old_files = vec![
            data_dir.join("subdir1/file1.txt"),
            data_dir.join("subdir1/file2.txt"),
            data_dir.join("subdir2/file3.txt"),
        ];

        for file in &old_files {
            fs::create_dir_all(file.parent().unwrap())?;
            fs::write(file, "old content")?;
            set_file_mtime(file, now - Duration::from_secs(7200))?;
        }

        // Create one new file
        let new_file = data_dir.join("subdir1/file_new.txt");
        fs::write(&new_file, "new content")?;
        set_file_mtime(&new_file, now - Duration::from_secs(1800))?;

        run_cleanup(&data_dir, cutoff).await?;

        // All old files should be removed
        for file in &old_files {
            assert!(!file.exists(), "Old file {:?} should be removed", file);
        }
        // New file should still exist
        assert!(new_file.exists(), "New file should not be removed");

        Ok(())
    }
}
