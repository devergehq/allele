use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::fs;

/// Base directory for all workspace clones
const CLONE_BASE: &str = ".cc-multiplex/workspaces";

/// Create a clone for a session: uses a short unique session ID as the workspace name.
/// Returns the clone path.
pub fn create_session_clone(source: &Path, project_name: &str, session_id: &str) -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let clone_dir = home.join(CLONE_BASE).join(project_name);
    fs::create_dir_all(&clone_dir)?;

    // Short session ID — first 8 chars of UUID
    let short_id: String = session_id.chars().take(8).collect();
    let clone_path = clone_dir.join(&short_id);

    if clone_path.exists() {
        // Unlikely with UUIDs but handle by appending a random suffix
        return create_clone(source, &format!("{short_id}-alt"));
    }

    let src_cstr = CString::new(source.to_string_lossy().as_bytes())?;
    let dst_cstr = CString::new(clone_path.to_string_lossy().as_bytes())?;

    let result = unsafe {
        libc::clonefile(src_cstr.as_ptr(), dst_cstr.as_ptr(), 0)
    };

    if result != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("clonefile() failed: {err}");
    }

    Ok(clone_path)
}

/// Create an APFS copy-on-write clone of a directory.
///
/// Uses the macOS `clonefile(2)` syscall — near-instant, zero disk cost
/// until files are modified. The clone is a perfect snapshot including
/// untracked files, node_modules, .env, everything.
///
/// Returns the path to the clone.
pub fn create_clone(source: &Path, workspace_name: &str) -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

    // Derive a project name from the source path
    let project_name = source
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let clone_dir = home
        .join(CLONE_BASE)
        .join(project_name);

    // Ensure parent directory exists
    fs::create_dir_all(&clone_dir)?;

    let clone_path = clone_dir.join(workspace_name);

    // clonefile requires destination does NOT exist
    if clone_path.exists() {
        anyhow::bail!(
            "Clone destination already exists: {}",
            clone_path.display()
        );
    }

    // Call clonefile(2) — macOS APFS copy-on-write clone
    let src_cstr = CString::new(source.to_string_lossy().as_bytes())?;
    let dst_cstr = CString::new(clone_path.to_string_lossy().as_bytes())?;

    let result = unsafe {
        libc::clonefile(src_cstr.as_ptr(), dst_cstr.as_ptr(), 0)
    };

    if result != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("clonefile() failed: {err}");
    }

    Ok(clone_path)
}

/// Delete a workspace clone
pub fn delete_clone(clone_path: &Path) -> anyhow::Result<()> {
    if !clone_path.exists() {
        return Ok(());
    }

    // Safety check — only delete paths under our workspace directory
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    let workspace_base = home.join(CLONE_BASE);

    if !clone_path.starts_with(&workspace_base) {
        anyhow::bail!(
            "Refusing to delete path outside workspace directory: {}",
            clone_path.display()
        );
    }

    fs::remove_dir_all(clone_path)?;
    Ok(())
}

/// List all workspace clones for a project
pub fn list_clones(project_name: &str) -> anyhow::Result<Vec<PathBuf>> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;

    let project_dir = home.join(CLONE_BASE).join(project_name);

    if !project_dir.exists() {
        return Ok(Vec::new());
    }

    let mut clones = Vec::new();
    for entry in fs::read_dir(project_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            clones.push(entry.path());
        }
    }

    Ok(clones)
}
