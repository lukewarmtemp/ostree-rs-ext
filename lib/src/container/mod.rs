//! # APIs bridging OSTree and container images
//!
//! This module contains APIs to bidirectionally map between a single OSTree commit and a container image wrapping it.
//! Because container images are just layers of tarballs, this builds on the [`crate::tar`] module.
//!
//! To emphasize this, the current high level model is that this is a one-to-one mapping - an ostree commit
//! can be exported (wrapped) into a container image, which will have exactly one layer.  Upon import
//! back into an ostree repository, all container metadata except for its digested checksum will be discarded.
//!
//! ## Signatures
//!
//! OSTree supports GPG and ed25519 signatures natively, and it's expected by default that
//! when booting from a fetched container image, one verifies ostree-level signatures.
//! For ostree, a signing configuration is specified via an ostree remote.  In order to
//! pair this configuration together, this library defines a "URL-like" string schema:
//!
//! `ostree-remote-registry:<remotename>:<containerimage>`
//!
//! A concrete instantiation might be e.g.: `ostree-remote-registry:fedora:quay.io/coreos/fedora-coreos:stable`
//!
//! To parse and generate these strings, see [`OstreeImageReference`].
//!
//! ## Layering
//!
//! A key feature of container images is support for layering.  At the moment, support
//! for this is [planned but not implemented](https://github.com/ostreedev/ostree-rs-ext/issues/12).

use anyhow::anyhow;
use containers_image_proxy::oci_spec;
use ostree::glib;

use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Deref;
use std::str::FromStr;

/// The label injected into a container image that contains the ostree commit SHA-256.
pub const OSTREE_COMMIT_LABEL: &str = "ostree.commit";

/// The name of an annotation attached to a layer which names the packages/components
/// which are part of it.
pub(crate) const CONTENT_ANNOTATION: &str = "ostree.components";
/// The character we use to separate values in [`CONTENT_ANNOTATION`].
pub(crate) const COMPONENT_SEPARATOR: char = ',';

/// Our generic catchall fatal error, expected to be converted
/// to a string to output to a terminal or logs.
type Result<T> = anyhow::Result<T>;

/// A backend/transport for OCI/Docker images.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Transport {
    /// A remote Docker/OCI registry (`registry:` or `docker://`)
    Registry,
    /// A local OCI directory (`oci:`)
    OciDir,
    /// A local OCI archive tarball (`oci-archive:`)
    OciArchive,
    /// Local container storage (`containers-storage:`)
    ContainerStorage,
}

/// Combination of a remote image reference and transport.
///
/// For example,
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReference {
    /// The storage and transport for the image
    pub transport: Transport,
    /// The image name (e.g. `quay.io/somerepo/someimage:latest`)
    pub name: String,
}

/// Policy for signature verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureSource {
    /// Fetches will use the named ostree remote for signature verification of the ostree commit.
    OstreeRemote(String),
    /// Fetches will defer to the `containers-policy.json`, but we make a best effort to reject `default: insecureAcceptAnything` policy.
    ContainerPolicy,
    /// NOT RECOMMENDED.  Fetches will defer to the `containers-policy.json` default which is usually `insecureAcceptAnything`.
    ContainerPolicyAllowInsecure,
}

/// A commonly used pre-OCI label for versions.
pub const LABEL_VERSION: &str = "version";

/// Combination of a signature verification mechanism, and a standard container image reference.
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OstreeImageReference {
    /// The signature verification mechanism.
    pub sigverify: SignatureSource,
    /// The container image reference.
    pub imgref: ImageReference,
}

impl TryFrom<&str> for Transport {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        Ok(match value {
            "registry" | "docker" => Self::Registry,
            "oci" => Self::OciDir,
            "oci-archive" => Self::OciArchive,
            "containers-storage" => Self::ContainerStorage,
            o => return Err(anyhow!("Unknown transport '{}'", o)),
        })
    }
}

impl TryFrom<&str> for ImageReference {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        let (transport_name, mut name) = value
            .split_once(':')
            .ok_or_else(|| anyhow!("Missing ':' in {}", value))?;
        let transport: Transport = transport_name.try_into()?;
        if name.is_empty() {
            return Err(anyhow!("Invalid empty name in {}", value));
        }
        if transport_name == "docker" {
            name = name
                .strip_prefix("//")
                .ok_or_else(|| anyhow!("Missing // in docker:// in {}", value))?;
        }
        Ok(Self {
            transport,
            name: name.to_string(),
        })
    }
}

impl FromStr for ImageReference {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::try_from(s)
    }
}

impl TryFrom<&str> for SignatureSource {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        match value {
            "ostree-image-signed" => Ok(Self::ContainerPolicy),
            "ostree-unverified-image" => Ok(Self::ContainerPolicyAllowInsecure),
            o => match o.strip_prefix("ostree-remote-image:") {
                Some(rest) => Ok(Self::OstreeRemote(rest.to_string())),
                _ => Err(anyhow!("Invalid signature source: {}", o)),
            },
        }
    }
}

impl FromStr for SignatureSource {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::try_from(s)
    }
}

impl TryFrom<&str> for OstreeImageReference {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        let (first, second) = value
            .split_once(':')
            .ok_or_else(|| anyhow!("Missing ':' in {}", value))?;
        let (sigverify, rest) = match first {
            "ostree-image-signed" => (SignatureSource::ContainerPolicy, Cow::Borrowed(second)),
            "ostree-unverified-image" => (
                SignatureSource::ContainerPolicyAllowInsecure,
                Cow::Borrowed(second),
            ),
            // Shorthand for ostree-unverified-image:registry:
            "ostree-unverified-registry" => (
                SignatureSource::ContainerPolicyAllowInsecure,
                Cow::Owned(format!("registry:{second}")),
            ),
            // This is a shorthand for ostree-remote-image with registry:
            "ostree-remote-registry" => {
                let (remote, rest) = second
                    .split_once(':')
                    .ok_or_else(|| anyhow!("Missing second ':' in {}", value))?;
                (
                    SignatureSource::OstreeRemote(remote.to_string()),
                    Cow::Owned(format!("registry:{rest}")),
                )
            }
            "ostree-remote-image" => {
                let (remote, rest) = second
                    .split_once(':')
                    .ok_or_else(|| anyhow!("Missing second ':' in {}", value))?;
                (
                    SignatureSource::OstreeRemote(remote.to_string()),
                    Cow::Borrowed(rest),
                )
            }
            o => {
                return Err(anyhow!("Invalid ostree image reference scheme: {}", o));
            }
        };
        let imgref = rest.deref().try_into()?;
        Ok(Self { sigverify, imgref })
    }
}

impl FromStr for OstreeImageReference {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::try_from(s)
    }
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            // TODO once skopeo supports this, canonicalize as registry:
            Self::Registry => "docker://",
            Self::OciArchive => "oci-archive:",
            Self::OciDir => "oci:",
            Self::ContainerStorage => "containers-storage:",
        };
        f.write_str(s)
    }
}

impl std::fmt::Display for ImageReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.transport, self.name)
    }
}

impl std::fmt::Display for SignatureSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignatureSource::OstreeRemote(r) => write!(f, "ostree-remote-image:{r}"),
            SignatureSource::ContainerPolicy => write!(f, "ostree-image-signed"),
            SignatureSource::ContainerPolicyAllowInsecure => {
                write!(f, "ostree-unverified-image")
            }
        }
    }
}

impl std::fmt::Display for OstreeImageReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.sigverify, self.imgref)
    }
}

/// Represents the difference in layer/blob content between two OCI image manifests.
#[derive(Debug)]
pub struct ManifestDiff<'a> {
    /// The source container image manifest.
    pub from: &'a oci_spec::image::ImageManifest,
    /// The target container image manifest.
    pub to: &'a oci_spec::image::ImageManifest,
    /// Layers which are present in the old image but not the new image.
    pub removed: Vec<&'a oci_spec::image::Descriptor>,
    /// Layers which are present in the new image but not the old image.
    pub added: Vec<&'a oci_spec::image::Descriptor>,
}

impl<'a> ManifestDiff<'a> {
    /// Compute the layer difference between two OCI image manifests.
    pub fn new(
        src: &'a oci_spec::image::ImageManifest,
        dest: &'a oci_spec::image::ImageManifest,
    ) -> Self {
        let src_layers = src
            .layers()
            .iter()
            .map(|l| (l.digest(), l))
            .collect::<HashMap<_, _>>();
        let dest_layers = dest
            .layers()
            .iter()
            .map(|l| (l.digest(), l))
            .collect::<HashMap<_, _>>();
        let mut removed = Vec::new();
        let mut added = Vec::new();
        for (blobid, &descriptor) in src_layers.iter() {
            if !dest_layers.contains_key(blobid) {
                removed.push(descriptor);
            }
        }
        removed.sort_by(|a, b| a.digest().cmp(b.digest()));
        for (blobid, &descriptor) in dest_layers.iter() {
            if !src_layers.contains_key(blobid) {
                added.push(descriptor);
            }
        }
        added.sort_by(|a, b| a.digest().cmp(b.digest()));
        ManifestDiff {
            from: src,
            to: dest,
            removed,
            added,
        }
    }
}

impl<'a> ManifestDiff<'a> {
    /// Prints the total, removed and added content between two OCI images
    pub fn print(&self) {
        fn layersum<'a, I: Iterator<Item = &'a oci_spec::image::Descriptor>>(layers: I) -> u64 {
            layers.map(|layer| layer.size() as u64).sum()
        }
        let new_total = self.to.layers().len();
        let new_total_size = glib::format_size(layersum(self.to.layers().iter()));
        let n_removed = self.removed.len();
        let n_added = self.added.len();
        let removed_size = layersum(self.removed.iter().copied());
        let removed_size_str = glib::format_size(removed_size);
        let added_size = layersum(self.added.iter().copied());
        let added_size_str = glib::format_size(added_size);
        println!("Total new layers: {new_total:<4}  Size: {new_total_size}");
        println!("Removed layers:   {n_removed:<4}  Size: {removed_size_str}");
        println!("Added layers:     {n_added:<4}  Size: {added_size_str}");
    }

    /// Prints the total, removed and added content between two OCI images
    pub fn test(&self) -> String {
        fn layersum<'a, I: Iterator<Item = &'a oci_spec::image::Descriptor>>(layers: I) -> u64 {
            layers.map(|layer| layer.size() as u64).sum()
        }
        let new_total = self.to.layers().len();
        let new_total_size = glib::format_size(layersum(self.to.layers().iter()));
        let n_removed = self.removed.len();
        let n_added = self.added.len();
        let removed_size = layersum(self.removed.iter().copied());
        let removed_size_str = glib::format_size(removed_size);
        let added_size = layersum(self.added.iter().copied());
        let added_size_str = glib::format_size(added_size);
        let result = format!("Total new layers: {new_total:<4}  Size: {new_total_size}\nRemoved layers:   {n_removed:<4}  Size: {removed_size_str}\nAdded layers:     {n_added:<4}  Size: {added_size_str}");
        println!("Total new layers: {new_total:<4}  Size: {new_total_size}");
        println!("Removed layers:   {n_removed:<4}  Size: {removed_size_str}");
        println!("Added layers:     {n_added:<4}  Size: {added_size_str}");
        return result
    }
}

/// Apply default configuration for container image pulls to an existing configuration.
/// For example, if `authfile` is not set, and `auth_anonymous` is `false`, and a global configuration file exists, it will be used.
///
/// If there is no configured explicit subprocess for skopeo, and the process is running
/// as root, then a default isolation of running the process via `nobody` will be applied.
pub fn merge_default_container_proxy_opts(
    config: &mut containers_image_proxy::ImageProxyConfig,
) -> Result<()> {
    let user = rustix::process::getuid()
        .is_root()
        .then_some(isolation::DEFAULT_UNPRIVILEGED_USER);
    merge_default_container_proxy_opts_with_isolation(config, user)
}

/// Apply default configuration for container image pulls, with optional support
/// for isolation as an unprivileged user.
pub fn merge_default_container_proxy_opts_with_isolation(
    config: &mut containers_image_proxy::ImageProxyConfig,
    isolation_user: Option<&str>,
) -> Result<()> {
    let auth_specified =
        config.auth_anonymous || config.authfile.is_some() || config.auth_data.is_some();
    if !auth_specified {
        config.authfile = crate::globals::get_global_authfile_path()?;
        // If there's no authfile, then force on anonymous pulls to ensure
        // that the container stack doesn't try to find it in the standard
        // container paths.
        if config.authfile.is_none() {
            config.auth_anonymous = true;
        }
    }
    // By default, drop privileges, unless the higher level code
    // has configured the skopeo command explicitly.
    let isolation_user = config
        .skopeo_cmd
        .is_none()
        .then_some(isolation_user.as_ref())
        .flatten();
    if let Some(user) = isolation_user {
        // Read the default authfile if it exists and pass it via file descriptor
        // which will ensure it's readable when we drop privileges.
        if let Some(authfile) = config.authfile.take() {
            config.auth_data = Some(std::fs::File::open(authfile)?);
        }
        let cmd = crate::isolation::unprivileged_subprocess("skopeo", user);
        config.skopeo_cmd = Some(cmd);
    }
    Ok(())
}

/// Convenience helper to return the labels, if present.
pub(crate) fn labels_of(
    config: &oci_spec::image::ImageConfiguration,
) -> Option<&HashMap<String, String>> {
    config.config().as_ref().and_then(|c| c.labels().as_ref())
}

/// Retrieve the version number from an image configuration.
pub fn version_for_config(config: &oci_spec::image::ImageConfiguration) -> Option<&str> {
    if let Some(labels) = labels_of(config) {
        for k in [oci_spec::image::ANNOTATION_VERSION, LABEL_VERSION] {
            if let Some(v) = labels.get(k) {
                return Some(v.as_str());
            }
        }
    }
    None
}

pub mod deploy;
mod encapsulate;
pub use encapsulate::*;
mod unencapsulate;
pub use unencapsulate::*;
// We have this trick of compiling ourself with integration testing
// enabled, which uses a lot of the code here.   See the
// `ostree-ext = { path = ".", features = ["internal-testing-api"] }`
// bit in Cargo.toml.
//
// But that isn't turned on for other crates that use this, and correctly gating all
// of it is a little tedious.  So let's just use the big hammer for now to
// quiet the dead code warnings.
#[cfg(feature = "internal-testing-api")]
pub mod ocidir;
#[cfg(not(feature = "internal-testing-api"))]
#[allow(dead_code)]
mod ocidir;
mod skopeo;
pub mod store;
mod update_detachedmeta;
pub use update_detachedmeta::*;

use crate::isolation;

#[cfg(test)]
mod tests {
    use std::process::Command;

    use containers_image_proxy::ImageProxyConfig;

    use super::*;

    const INVALID_IRS: &[&str] = &["", "foo://", "docker:blah", "registry:", "foo:bar"];
    const VALID_IRS: &[&str] = &[
        "containers-storage:localhost/someimage",
        "docker://quay.io/exampleos/blah:sometag",
    ];

    #[test]
    fn test_imagereference() {
        let ir: ImageReference = "registry:quay.io/exampleos/blah".try_into().unwrap();
        assert_eq!(ir.transport, Transport::Registry);
        assert_eq!(ir.name, "quay.io/exampleos/blah");
        assert_eq!(ir.to_string(), "docker://quay.io/exampleos/blah");

        for &v in VALID_IRS {
            ImageReference::try_from(v).unwrap();
        }

        for &v in INVALID_IRS {
            if ImageReference::try_from(v).is_ok() {
                panic!("Should fail to parse: {}", v)
            }
        }
        let ir: ImageReference = "oci:somedir".try_into().unwrap();
        assert_eq!(ir.transport, Transport::OciDir);
        assert_eq!(ir.name, "somedir");
    }

    #[test]
    fn test_ostreeimagereference() {
        // Test both long form `ostree-remote-image:$myremote:registry` and the
        // shorthand `ostree-remote-registry:$myremote`.
        let ir_s = "ostree-remote-image:myremote:registry:quay.io/exampleos/blah";
        let ir_registry = "ostree-remote-registry:myremote:quay.io/exampleos/blah";
        for &ir_s in &[ir_s, ir_registry] {
            let ir: OstreeImageReference = ir_s.try_into().unwrap();
            assert_eq!(
                ir.sigverify,
                SignatureSource::OstreeRemote("myremote".to_string())
            );
            assert_eq!(ir.imgref.transport, Transport::Registry);
            assert_eq!(ir.imgref.name, "quay.io/exampleos/blah");
            assert_eq!(
                ir.to_string(),
                "ostree-remote-image:myremote:docker://quay.io/exampleos/blah"
            );
        }

        // Also verify our FromStr impls

        let ir: OstreeImageReference = ir_s.try_into().unwrap();
        assert_eq!(ir, OstreeImageReference::from_str(ir_s).unwrap());
        // test our Eq implementation
        assert_eq!(&ir, &OstreeImageReference::try_from(ir_registry).unwrap());

        let ir_s = "ostree-image-signed:docker://quay.io/exampleos/blah";
        let ir: OstreeImageReference = ir_s.try_into().unwrap();
        assert_eq!(ir.sigverify, SignatureSource::ContainerPolicy);
        assert_eq!(ir.imgref.transport, Transport::Registry);
        assert_eq!(ir.imgref.name, "quay.io/exampleos/blah");
        assert_eq!(
            ir.to_string(),
            "ostree-image-signed:docker://quay.io/exampleos/blah"
        );

        let ir_s = "ostree-unverified-image:docker://quay.io/exampleos/blah";
        let ir: OstreeImageReference = ir_s.try_into().unwrap();
        assert_eq!(ir.sigverify, SignatureSource::ContainerPolicyAllowInsecure);
        assert_eq!(ir.imgref.transport, Transport::Registry);
        assert_eq!(ir.imgref.name, "quay.io/exampleos/blah");
        assert_eq!(
            ir.to_string(),
            "ostree-unverified-image:docker://quay.io/exampleos/blah"
        );
        let ir_shorthand =
            OstreeImageReference::try_from("ostree-unverified-registry:quay.io/exampleos/blah")
                .unwrap();
        assert_eq!(&ir_shorthand, &ir);
    }

    #[test]
    fn test_merge_authopts() {
        // Verify idempotence of authentication processing
        let mut c = ImageProxyConfig::default();
        let authf = std::fs::File::open("/dev/null").unwrap();
        c.auth_data = Some(authf);
        super::merge_default_container_proxy_opts_with_isolation(&mut c, None).unwrap();
        assert!(!c.auth_anonymous);
        assert!(c.authfile.is_none());
        assert!(c.auth_data.is_some());
        assert!(c.skopeo_cmd.is_none());
        super::merge_default_container_proxy_opts_with_isolation(&mut c, None).unwrap();
        assert!(!c.auth_anonymous);
        assert!(c.authfile.is_none());
        assert!(c.auth_data.is_some());
        assert!(c.skopeo_cmd.is_none());

        // Verify interaction with explicit isolation
        let mut c = ImageProxyConfig {
            skopeo_cmd: Some(Command::new("skopeo")),
            ..Default::default()
        };
        super::merge_default_container_proxy_opts_with_isolation(&mut c, Some("foo")).unwrap();
        assert_eq!(c.skopeo_cmd.unwrap().get_program(), "skopeo");
    }
}
