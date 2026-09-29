//! Private file writes.

use std::path::Path;

use crate::error::Result;

/// Write `contents` to `path` atomically, with owner-only permissions.
///
/// The temp file is created in the same directory so the rename stays on one
/// filesystem. Permissions are tightened *before* any content is written, so a
/// secret is never briefly world-readable.
pub fn write_private_file(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_mode(parent, 0o700);
    }

    let temp = path.with_extension("tmp");
    {
        let mut file = std::fs::File::create(&temp)?;
        set_mode(&temp, 0o600);
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&temp, path)?;
    Ok(())
}

/// Best-effort permission tightening. A no-op on platforms without POSIX modes.
fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_create_parent_directories_and_leave_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested").join("secret");

        write_private_file(&target, "token").unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "token");
        assert!(!target.with_extension("tmp").exists());
    }

    #[test]
    fn files_are_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("secret");

        write_private_file(&target, "token").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn overwriting_an_existing_file_keeps_it_private() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("secret");
        write_private_file(&target, "first").unwrap();

        write_private_file(&target, "second").unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a rewrite must not widen permissions");
        }
    }
}
