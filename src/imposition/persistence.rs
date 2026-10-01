use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use serde::{de::DeserializeOwned, Serialize};

use crate::{AppError, AppResult};

pub(crate) const MAX_METADATA_BYTES: u64 = 2 * 1024 * 1024;
pub(crate) const MAX_SAVED_RECORDS: usize = 100;

pub(crate) fn read_json_or_default<T>(path: &Path, label: &str) -> AppResult<T>
where
    T: DeserializeOwned + Default,
{
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(error) => {
            return Err(AppError::internal_cause(
                format!("could not open {label} metadata at `{}`", path.display()),
                error,
            ));
        }
    };
    let mut bytes = Vec::new();
    file.take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            AppError::internal_cause(
                format!("could not read {label} metadata at `{}`", path.display()),
                error,
            )
        })?;
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        return Err(AppError::Internal(format!(
            "{label} metadata exceeds the {} MB limit",
            MAX_METADATA_BYTES / 1024 / 1024
        )));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        AppError::internal_cause(
            format!("could not parse {label} metadata at `{}`", path.display()),
            error,
        )
    })
}

pub(crate) fn write_json_atomic<T>(path: &Path, value: &T, label: &str) -> AppResult<()>
where
    T: Serialize + ?Sized,
{
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| {
        AppError::internal_cause(
            format!(
                "could not create the {label} metadata directory `{}`",
                parent.display()
            ),
            error,
        )
    })?;
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| {
        AppError::internal_cause(format!("could not encode {label} metadata"), error)
    })?;
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        return Err(AppError::Internal(format!(
            "{label} metadata exceeds the {} MB limit",
            MAX_METADATA_BYTES / 1024 / 1024
        )));
    }

    let (temp_path, mut temp_file) = create_temp_file(path, label)?;
    if let Err(error) = temp_file.write_all(&bytes) {
        drop(temp_file);
        let _ = fs::remove_file(&temp_path);
        return Err(AppError::internal_cause(
            format!(
                "could not write temporary {label} metadata at `{}`",
                temp_path.display()
            ),
            error,
        ));
    }
    if let Err(error) = temp_file.sync_all() {
        drop(temp_file);
        let _ = fs::remove_file(&temp_path);
        return Err(AppError::internal_cause(
            format!(
                "could not flush temporary {label} metadata at `{}`",
                temp_path.display()
            ),
            error,
        ));
    }
    drop(temp_file);
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(AppError::internal_cause(
            format!(
                "could not commit {label} metadata from `{}` to `{}`",
                temp_path.display(),
                path.display()
            ),
            error,
        ));
    }
    sync_parent_directory(parent, label)?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path, label: &str) -> AppResult<()> {
    let directory = File::open(parent).map_err(|error| {
        AppError::internal_cause(
            format!(
                "could not open the {label} metadata directory `{}` for synchronization",
                parent.display()
            ),
            error,
        )
    })?;
    directory.sync_all().map_err(|error| {
        AppError::internal_cause(
            format!(
                "could not synchronize the {label} metadata directory `{}`",
                parent.display()
            ),
            error,
        )
    })
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path, _label: &str) -> AppResult<()> {
    // Rust does not expose a portable way to open and flush a directory handle.
    Ok(())
}

fn create_temp_file(path: &Path, label: &str) -> AppResult<(PathBuf, File)> {
    const MAX_TEMP_FILE_ATTEMPTS: usize = 4;

    for _ in 0..MAX_TEMP_FILE_ATTEMPTS {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).map_err(|error| {
            AppError::internal_cause(format!("could not prepare {label}"), error)
        })?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let temp_path = path.with_extension(format!("tmp-{suffix}"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(AppError::internal_cause(
                    format!(
                        "could not create temporary {label} metadata at `{}`",
                        temp_path.display()
                    ),
                    error,
                ));
            }
        }
    }
    Err(AppError::Internal(format!(
        "could not allocate a temporary {label} metadata file"
    )))
}

pub(crate) fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory(label: &str) -> PathBuf {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        std::env::temp_dir().join(format!("pdf-tools-{label}-{suffix}"))
    }

    #[test]
    fn identifiers_are_safe_for_metadata_and_artifact_names() {
        assert!(valid_identifier("export-123_2"));
        assert!(!valid_identifier("../export"));
        assert!(!valid_identifier("bad/name"));
        assert!(!valid_identifier(""));
    }

    #[test]
    fn atomic_json_write_commits_replacement_and_removes_temporary_file() {
        let directory = test_directory("atomic-json");
        let path = directory.join("settings.json");

        write_json_atomic(&path, &vec!["old"], "test settings").unwrap();
        write_json_atomic(&path, &vec!["new"], "test settings").unwrap();

        let saved: Vec<String> = read_json_or_default(&path, "test settings").unwrap();
        assert_eq!(saved, vec!["new"]);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn parent_directory_sync_failure_preserves_operation_and_path_context() {
        let missing = test_directory("missing-sync-parent");

        let error = sync_parent_directory(&missing, "test settings").unwrap_err();

        assert!(error
            .to_string()
            .contains("could not open the test settings metadata directory"));
        assert!(error.to_string().contains(&missing.display().to_string()));
    }
}
