//! APIs to write a tarball stream into an OSTree commit.
//!
//! This functionality already exists in libostree mostly,
//! this API adds a higher level, more ergonomic Rust frontend
//! to it.
//!
//! In the future, this may also evolve into parsing the tar
//! stream in Rust, not in C.

use crate::Result;
use anyhow::{anyhow, Context};
use camino::{Utf8Component, Utf8Path, Utf8PathBuf};

use cap_std::io_lifetimes;
use cap_std_ext::cmdext::CapStdExtCommandExt;
use cap_std_ext::{cap_std, cap_tempfile};
use once_cell::unsync::OnceCell;
use ostree::gio;
use ostree::prelude::FileExt;
use std::collections::{BTreeMap, HashMap};
use std::io::{BufWriter, Seek, Write};
use std::path::Path;
use std::process::Stdio;

use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tracing::instrument;

/// Copy a tar entry to a new tar archive, optionally using a different filesystem path.
pub(crate) fn copy_entry(
    entry: tar::Entry<impl std::io::Read>,
    dest: &mut tar::Builder<impl std::io::Write>,
    path: Option<&Path>,
) -> Result<()> {
    // Make copies of both the header and path, since that's required for the append APIs
    let path = if let Some(path) = path {
        path.to_owned()
    } else {
        (*entry.path()?).to_owned()
    };
    let mut header = entry.header().clone();

    // Need to use the entry.link_name() not the header.link_name()
    // api as the header api does not handle long paths:
    // https://github.com/alexcrichton/tar-rs/issues/192
    match entry.header().entry_type() {
        tar::EntryType::Link | tar::EntryType::Symlink => {
            let target = entry.link_name()?.ok_or_else(|| anyhow!("Invalid link"))?;
            dest.append_link(&mut header, path, target)
        }
        _ => dest.append_data(&mut header, path, entry),
    }
    .map_err(Into::into)
}

/// Configuration for tar layer commits.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct WriteTarOptions {
    /// Base ostree commit hash
    pub base: Option<String>,
    /// Enable SELinux labeling from the base commit
    /// Requires the `base` option.
    pub selinux: bool,
}

/// The result of writing a tar stream.
///
/// This includes some basic data on the number of files that were filtered
/// out because they were not in `/usr`.
#[derive(Debug, Default)]
pub struct WriteTarResult {
    /// The resulting OSTree commit SHA-256.
    pub commit: String,
    /// Number of paths in a prefix (e.g. `/var` or `/boot`) which were discarded.
    pub filtered: BTreeMap<String, u32>,
}

// Copy of logic from https://github.com/ostreedev/ostree/pull/2447
// to avoid waiting for backport + releases
fn sepolicy_from_base(repo: &ostree::Repo, base: &str) -> Result<tempfile::TempDir> {
    let cancellable = gio::Cancellable::NONE;
    let policypath = "usr/etc/selinux";
    let tempdir = tempfile::tempdir()?;
    let (root, _) = repo.read_commit(base, cancellable)?;
    let policyroot = root.resolve_relative_path(policypath);
    if policyroot.query_exists(cancellable) {
        let policydest = tempdir.path().join(policypath);
        std::fs::create_dir_all(policydest.parent().unwrap())?;
        let opts = ostree::RepoCheckoutAtOptions {
            mode: ostree::RepoCheckoutMode::User,
            subpath: Some(Path::new(policypath).to_owned()),
            ..Default::default()
        };
        repo.checkout_at(Some(&opts), ostree::AT_FDCWD, policydest, base, cancellable)?;
    }
    Ok(tempdir)
}

#[derive(Debug)]
enum NormalizedPathResult<'a> {
    Filtered(&'a str),
    Normal(Utf8PathBuf),
}

fn normalize_validate_path(path: &Utf8Path) -> Result<NormalizedPathResult<'_>> {
    // This converts e.g. `foo//bar/./baz` into `foo/bar/baz`.
    let mut components = path
        .components()
        .map(|part| {
            match part {
                // Convert absolute paths to relative
                camino::Utf8Component::RootDir => Ok(camino::Utf8Component::CurDir),
                // Allow ./ and regular parts
                camino::Utf8Component::Normal(_) | camino::Utf8Component::CurDir => Ok(part),
                // Barf on Windows paths as well as Unix path uplinks `..`
                _ => Err(anyhow!("Invalid path: {}", path)),
            }
        })
        .peekable();
    let mut ret = Utf8PathBuf::new();
    // Insert a leading `./` if not present
    if let Some(Ok(camino::Utf8Component::Normal(_))) = components.peek() {
        ret.push(camino::Utf8Component::CurDir);
    }
    let mut found_first = false;
    for part in components {
        let part = part?;
        if !found_first {
            if let Utf8Component::Normal(part) = part {
                found_first = true;
                // Now, rewrite /etc -> /usr/etc, and discard everything not in /usr.
                match part {
                    "usr" => ret.push(part),
                    "etc" => {
                        ret.push("usr/etc");
                    }
                    o => return Ok(NormalizedPathResult::Filtered(o)),
                }
            } else {
                ret.push(part);
            }
        } else {
            ret.push(part);
        }
    }

    Ok(NormalizedPathResult::Normal(ret))
}

/// Perform various filtering on imported tar archives.
///  - Move /etc to /usr/etc
///  - Entirely drop files not in /usr
///
/// This also acts as a Rust "pre-parser" of the tar archive, hopefully
/// catching anything corrupt that might be exploitable from the C libarchive side.
/// Remember that we're parsing this while we're downloading it, and in order
/// to verify integrity we rely on the total sha256 of the blob, so all content
/// written before then must be considered untrusted.
pub(crate) fn filter_tar(
    src: impl std::io::Read,
    dest: impl std::io::Write,
) -> Result<BTreeMap<String, u32>> {
    let src = std::io::BufReader::new(src);
    let mut src = tar::Archive::new(src);
    let dest = BufWriter::new(dest);
    let mut dest = tar::Builder::new(dest);
    let mut filtered = BTreeMap::new();

    let ents = src.entries()?;

    // Lookaside data for dealing with hardlinked files into /sysroot; see below.
    let mut changed_sysroot_objects = HashMap::new();
    let mut new_sysroot_link_targets = HashMap::<Utf8PathBuf, Utf8PathBuf>::new();
    // A temporary directory if needed
    let tmpdir = OnceCell::new();

    for entry in ents {
        let mut entry = entry?;
        let header = entry.header();
        let path = entry.path()?;
        let path: &Utf8Path = (&*path).try_into()?;

        let is_modified = header.mtime().unwrap_or_default() > 0;
        let is_regular = header.entry_type() == tar::EntryType::Regular;
        if path.strip_prefix(crate::tar::REPO_PREFIX).is_ok() {
            // If it's a modified file in /sysroot, it may be a target for future hardlinks.
            // In that case, we copy the data off to a temporary file.  Then the first hardlink
            // to it becomes instead the real file, and any *further* hardlinks refer to that
            // file instead.
            if is_modified && is_regular {
                tracing::debug!("Processing modified sysroot file {path}");
                // Lazily allocate a temporary directory
                let tmpdir = tmpdir.get_or_try_init(|| {
                    let vartmp = &cap_std::fs::Dir::open_ambient_dir(
                        "/var/tmp",
                        cap_std::ambient_authority(),
                    )?;
                    cap_tempfile::tempdir_in(vartmp)
                })?;
                // Create an O_TMPFILE (anonymous file) to use as a temporary store for the file data
                let mut tmpf = cap_tempfile::TempFile::new_anonymous(tmpdir).map(BufWriter::new)?;
                let path = path.to_owned();
                let header = header.clone();
                std::io::copy(&mut entry, &mut tmpf)?;
                let mut tmpf = tmpf.into_inner()?;
                tmpf.seek(std::io::SeekFrom::Start(0))?;
                // Cache this data, indexed by the file path
                changed_sysroot_objects.insert(path, (header, tmpf));
                continue;
            }
        } else if header.entry_type() == tar::EntryType::Link && is_modified {
            let target = header
                .link_name()?
                .ok_or_else(|| anyhow!("Invalid empty hardlink"))?;
            let target: &Utf8Path = (&*target).try_into()?;
            // If this is a hardlink into /sysroot...
            if target.strip_prefix(crate::tar::REPO_PREFIX).is_ok() {
                // And we found a previously processed modified file there
                if let Some((mut header, data)) = changed_sysroot_objects.remove(target) {
                    tracing::debug!("Making {path} canonical for sysroot link {target}");
                    // Make *this* entry the canonical one, consuming the temporary file data
                    dest.append_data(&mut header, path, data)?;
                    // And cache this file path as the new link target
                    new_sysroot_link_targets.insert(target.to_owned(), path.to_owned());
                } else if let Some(real_target) = new_sysroot_link_targets.get(target) {
                    tracing::debug!("Relinking {path} to {real_target}");
                    // We found a 2nd (or 3rd, etc.) link into /sysroot; rewrite the link
                    // target to be the first file outside of /sysroot we found.
                    let mut header = header.clone();
                    dest.append_link(&mut header, path, real_target)?;
                } else {
                    tracing::debug!("Found unhandled modified link from {path} to {target}");
                }
                continue;
            }
        }

        let normalized = match normalize_validate_path(path)? {
            NormalizedPathResult::Filtered(path) => {
                if let Some(v) = filtered.get_mut(path) {
                    *v += 1;
                } else {
                    filtered.insert(path.to_string(), 1);
                }
                continue;
            }
            NormalizedPathResult::Normal(path) => path,
        };

        copy_entry(entry, &mut dest, Some(normalized.as_std_path()))?;
    }
    dest.into_inner()?.flush()?;
    Ok(filtered)
}

/// Asynchronous wrapper for filter_tar()
async fn filter_tar_async(
    src: impl AsyncRead + Send + 'static,
    mut dest: impl AsyncWrite + Send + Unpin,
) -> Result<BTreeMap<String, u32>> {
    let (tx_buf, mut rx_buf) = tokio::io::duplex(8192);
    // The source must be moved to the heap so we know it is stable for passing to the worker thread
    let src = Box::pin(src);
    let tar_transformer = tokio::task::spawn_blocking(move || {
        let mut src = tokio_util::io::SyncIoBridge::new(src);
        let dest = tokio_util::io::SyncIoBridge::new(tx_buf);
        let r = filter_tar(&mut src, dest);
        // Pass ownership of the input stream back to the caller - see below.
        (r, src)
    });
    let copier = tokio::io::copy(&mut rx_buf, &mut dest);
    let (r, v) = tokio::join!(tar_transformer, copier);
    let _v: u64 = v?;
    let (r, src) = r?;
    // Note that the worker thread took temporary ownership of the input stream; we only close
    // it at this point, after we're sure we've done all processing of the input.  The reason
    // for this is that both the skopeo process *or* us could encounter an error (see join_fetch).
    // By ensuring we hold the stream open as long as possible, it ensures that we're going to
    // see a remote error first, instead of the remote skopeo process seeing us close the pipe
    // because we found an error.
    drop(src);
    // And pass back the result
    r
}

/// Write the contents of a tarball as an ostree commit.
#[allow(unsafe_code)] // For raw fd bits
#[instrument(level = "debug", skip_all)]
pub async fn write_tar(
    repo: &ostree::Repo,
    src: impl tokio::io::AsyncRead + Send + Unpin + 'static,
    refname: &str,
    options: Option<WriteTarOptions>,
) -> Result<WriteTarResult> {
    let repo = repo.clone();
    let options = options.unwrap_or_default();
    let sepolicy = if options.selinux {
        if let Some(base) = options.base {
            Some(sepolicy_from_base(&repo, &base).context("tar: Preparing sepolicy")?)
        } else {
            None
        }
    } else {
        None
    };
    let mut c = std::process::Command::new("ostree");
    let repofd = repo.dfd_as_file()?;
    let repofd: Arc<io_lifetimes::OwnedFd> = Arc::new(repofd.into());
    {
        let c = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args(["commit"]);
        c.take_fd_n(repofd.clone(), 3);
        c.arg("--repo=/proc/self/fd/3");
        if let Some(sepolicy) = sepolicy.as_ref() {
            c.arg("--selinux-policy");
            c.arg(sepolicy.path());
        }
        c.arg(&format!(
            "--add-metadata-string=ostree.importer.version={}",
            env!("CARGO_PKG_VERSION")
        ));
        c.args([
            "--no-bindings",
            "--tar-autocreate-parents",
            "--tree=tar=/proc/self/fd/0",
            "--branch",
            refname,
        ]);
    }
    let mut c = tokio::process::Command::from(c);
    c.kill_on_drop(true);
    let mut r = c.spawn()?;
    tracing::trace!("Spawned ostree child process");
    // Safety: We passed piped() for all of these
    let child_stdin = r.stdin.take().unwrap();
    let mut child_stdout = r.stdout.take().unwrap();
    let mut child_stderr = r.stderr.take().unwrap();
    // Copy the filtered tar stream to child stdin
    let filtered_result = filter_tar_async(src, child_stdin);
    let output_copier = async move {
        // Gather stdout/stderr to buffers
        let mut child_stdout_buf = String::new();
        let mut child_stderr_buf = String::new();
        let (_a, _b) = tokio::try_join!(
            child_stdout.read_to_string(&mut child_stdout_buf),
            child_stderr.read_to_string(&mut child_stderr_buf)
        )?;
        Ok::<_, anyhow::Error>((child_stdout_buf, child_stderr_buf))
    };

    // We must convert the child exit status here to an error to
    // ensure we break out of the try_join! below.
    let status = async move {
        let status = r.wait().await?;
        if !status.success() {
            return Err(anyhow!("Failed to commit tar: {:?}", status));
        }
        anyhow::Ok(())
    };
    tracing::debug!("Waiting on child process");
    let (filtered_result, child_stdout) =
        match tokio::try_join!(status, filtered_result).context("Processing tar via ostree") {
            Ok(((), filtered_result)) => {
                let (child_stdout, _) = output_copier.await.context("Copying child output")?;
                (filtered_result, child_stdout)
            }
            Err(e) => {
                if let Ok((_, child_stderr)) = output_copier.await {
                    // Avoid trailing newline
                    let child_stderr = child_stderr.trim();
                    Err(e.context(format!("{child_stderr}")))?
                } else {
                    Err(e)?
                }
            }
        };
    drop(sepolicy);

    tracing::trace!("tar written successfully");
    // TODO: trim string in place
    let s = child_stdout.trim();
    Ok(WriteTarResult {
        commit: s.to_string(),
        filtered: filtered_result,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_normalize_path() {
        let valid = &[
            ("/usr/bin/blah", "./usr/bin/blah"),
            ("usr/bin/blah", "./usr/bin/blah"),
            ("usr///share/.//blah", "./usr/share/blah"),
            ("./", "."),
        ];
        for &(k, v) in valid {
            let r = normalize_validate_path(k.into()).unwrap();
            match r {
                NormalizedPathResult::Filtered(o) => {
                    panic!("Case {} should not be filtered as {}", k, o)
                }
                NormalizedPathResult::Normal(p) => {
                    assert_eq!(v, p.as_str());
                }
            }
        }
        let filtered = &[
            ("/boot/vmlinuz", "boot"),
            ("var/lib/blah", "var"),
            ("./var/lib/blah", "var"),
        ];
        for &(k, v) in filtered {
            match normalize_validate_path(k.into()).unwrap() {
                NormalizedPathResult::Filtered(f) => {
                    assert_eq!(v, f);
                }
                NormalizedPathResult::Normal(_) => {
                    panic!("{} should be filtered", k)
                }
            }
        }
        let errs = &["usr/foo/../../bar"];
        for &k in errs {
            assert!(normalize_validate_path(k.into()).is_err());
        }
    }

    #[tokio::test]
    async fn tar_filter() -> Result<()> {
        let tempd = tempfile::tempdir()?;
        let rootfs = &tempd.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("etc/systemd/system"))?;
        std::fs::write(rootfs.join("etc/systemd/system/foo.service"), "fooservice")?;
        std::fs::write(rootfs.join("blah"), "blah")?;
        let rootfs_tar_path = &tempd.path().join("rootfs.tar");
        let rootfs_tar = std::fs::File::create(rootfs_tar_path)?;
        let mut rootfs_tar = tar::Builder::new(rootfs_tar);
        rootfs_tar.append_dir_all(".", rootfs)?;
        let _ = rootfs_tar.into_inner()?;
        let mut dest = Vec::new();
        let src = tokio::io::BufReader::new(tokio::fs::File::open(rootfs_tar_path).await?);
        filter_tar_async(src, &mut dest).await?;
        let dest = dest.as_slice();
        let mut final_tar = tar::Archive::new(Cursor::new(dest));
        let destdir = &tempd.path().join("destdir");
        final_tar.unpack(destdir)?;
        assert!(destdir.join("usr/etc/systemd/system/foo.service").exists());
        assert!(!destdir.join("blah").exists());
        Ok(())
    }
}
