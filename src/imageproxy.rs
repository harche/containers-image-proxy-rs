//! Run skopeo as a subprocess to fetch container images.
//!
//! This allows fetching a container image manifest and layers in a streaming fashion.
//!
//! More information: <https://github.com/containers/skopeo/pull/1476>

use anyhow::{anyhow, Context, Result};
use cap_std_ext::prelude::CapStdExtCommandExt;
use cap_std_ext::{cap_std, cap_tempfile};
use futures_util::Future;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::ops::Range;
use std::os::fd::OwnedFd;
use std::os::unix::prelude::CommandExt;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufRead, AsyncReadExt};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinError;
use tracing::instrument;

/// Re-export because we use this in our public APIs
pub use oci_spec;

pub const OCI_TYPE_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
pub const OCI_TYPE_LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";

/// File descriptor range which is reserved for passing data down into the proxy;
/// avoid configuring the command to use files in this range.  (Also, stdin is
/// reserved)
pub const RESERVED_FD_RANGE: Range<i32> = 100..200;

// This is defined in skopeo; maximum size of JSON we will read/write.
// Note that payload data (non-metadata) should go over a pipe file descriptor.
const MAX_MSG_SIZE: usize = 32 * 1024;

// Introduced in https://github.com/containers/skopeo/pull/1523
static BASE_PROTO_VERSION: Lazy<semver::VersionReq> =
    Lazy::new(|| semver::VersionReq::parse("0.2.3").unwrap());
static LAYER_INFO_PROTO_VERSION: Lazy<semver::VersionReq> =
    Lazy::new(|| semver::VersionReq::parse("0.2.5").unwrap());

#[derive(Serialize)]
struct Request {
    method: String,
    args: Vec<serde_json::Value>,
}

impl Request {
    fn new<T, I>(method: &str, args: T) -> Self
    where
        T: IntoIterator<Item = I>,
        I: Into<serde_json::Value>,
    {
        let args: Vec<_> = args.into_iter().map(|v| v.into()).collect();
        Self {
            method: method.to_string(),
            args,
        }
    }

    fn new_bare(method: &str) -> Self {
        Self {
            method: method.to_string(),
            args: vec![],
        }
    }
}

#[derive(Deserialize)]
struct Reply {
    success: bool,
    error: String,
    pipeid: u32,
    value: serde_json::Value,
}

type ChildFuture = Pin<
    Box<dyn Future<Output = std::result::Result<std::io::Result<std::process::Output>, JoinError>>>,
>;

/// Manage a child process proxy to fetch container images.
pub struct ImageProxy {
    sockfd: Arc<Mutex<OwnedFd>>,
    childwait: Arc<AsyncMutex<ChildFuture>>,
    protover: semver::Version,
}

impl std::fmt::Debug for ImageProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageProxy").finish()
    }
}

/// Opaque identifier for an image
#[derive(Debug, PartialEq, Eq)]
pub struct OpenedImage(u32);

/// Configuration for the proxy.
#[derive(Debug, Default)]
pub struct ImageProxyConfig {
    /// Path to container auth file; equivalent to `skopeo --authfile`.
    /// This conflicts with [`auth_data`].
    pub authfile: Option<PathBuf>,

    /// Data stream for container auth.  This conflicts with [`authfile`].
    pub auth_data: Option<File>,

    /// Do not use default container authentication paths; equivalent to `skopeo --no-creds`.
    ///
    /// Defaults to `false`; in other words, use the default file paths from `man containers-auth.json`.
    pub auth_anonymous: bool,

    // Directory with certificates (*.crt, *.cert, *.key) used to connect to registry
    // Equivalent to `skopeo --cert-dir`
    pub certificate_directory: Option<PathBuf>,

    /// Decryption keys to decrypt an encrypted container image.
    /// equivalent to `skopeo copy --decryption-key <path_to_decryption_key> `
    pub decryption_keys: Option<Vec<PathBuf>>,

    /// If set, disable TLS verification.  Equivalent to `skopeo --tls-verify=false`.
    pub insecure_skip_tls_verification: Option<bool>,

    /// Provide a configured [`std::process::Command`] instance.
    ///
    /// This allows configuring aspects of the resulting child `skopeo` process.
    /// The intention of this hook is to allow the caller to use e.g.
    /// `systemd-run` or equivalent containerization tools.  For example you
    /// can set up a command whose arguments are `systemd-run -Pq -p DynamicUser=yes -- skopeo`.
    /// You can also set up arbitrary aspects of the child via e.g.
    /// [`current_dir`] [`pre_exec`].
    ///
    /// [`current_dir`]: https://doc.rust-lang.org/std/process/struct.Command.html#method.current_dir
    /// [`pre_exec`]: https://doc.rust-lang.org/std/os/unix/process/trait.CommandExt.html#tymethod.pre_exec
    ///
    /// The default is to wrap via util-linux `setpriv --pdeathsig SIGTERM -- skopeo`,
    /// which on Linux binds the lifecycle of the child process to the parent.
    ///
    /// Note that you *must* add `skopeo` as the primary argument or
    /// indirectly.  However, all other command line options including
    /// `experimental-image-proxy` will be injected by this library.
    /// You may use a different command name from `skopeo` if your
    /// application has set up a compatible copy, e.g. `/usr/lib/myapp/my-private-skopeo`/
    pub skopeo_cmd: Option<Command>,
}

impl TryFrom<ImageProxyConfig> for Command {
    type Error = anyhow::Error;

    fn try_from(config: ImageProxyConfig) -> Result<Self, Self::Error> {
        let mut allocated_fds = RESERVED_FD_RANGE.clone();
        let mut alloc_fd = || {
            allocated_fds
                .next()
                .ok_or_else(|| anyhow::anyhow!("Ran out of reserved file descriptors for child"))
        };

        // By default, we set up pdeathsig to "lifecycle bind" the child process to us.
        let mut c = config.skopeo_cmd.unwrap_or_else(|| {
            let mut c = std::process::Command::new("skopeo");
            unsafe {
                c.pre_exec(|| {
                    rustix::process::set_parent_process_death_signal(Some(
                        rustix::process::Signal::Term,
                    ))
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
                });
            }
            c
        });
        c.arg("experimental-image-proxy");
        let auth_option_count = [
            config.authfile.is_some(),
            config.auth_data.is_some(),
            config.auth_anonymous,
        ]
        .into_iter()
        .filter(|&x| x)
        .count();
        if auth_option_count > 1 {
            // This is a programmer error really
            anyhow::bail!("Conflicting authentication options");
        }
        if let Some(authfile) = config.authfile {
            c.arg("--authfile");
            c.arg(authfile);
        } else if let Some(mut auth_data) = config.auth_data.map(std::io::BufReader::new) {
            // If we get the authentication data as a file, we always copy it to a new temporary file under
            // the assumption that the caller provided it this way to aid in privilege separation where
            // the file is only readable to privileged code.
            let target_fd = alloc_fd()?;
            let tmpd = &cap_std::fs::Dir::open_ambient_dir("/tmp", cap_std::ambient_authority())?;
            let mut tempfile = cap_tempfile::TempFile::new_anonymous(tmpd)
                .context("Creating temporary file for auth data")
                .map(std::io::BufWriter::new)?;
            std::io::copy(&mut auth_data, &mut tempfile)?;
            let tempfile = tempfile.into_inner()?.into_std();
            let fd = std::sync::Arc::new(tempfile.into());
            c.take_fd_n(fd, target_fd);
            c.arg("--authfile");
            c.arg(format!("/proc/self/fd/{target_fd}"));
        } else if config.auth_anonymous {
            c.arg("--no-creds");
        }

        if let Some(certificate_directory) = config.certificate_directory {
            c.arg("--cert-dir");
            c.arg(certificate_directory);
        }

        if let Some(decryption_keys) = config.decryption_keys {
            for decryption_key in &decryption_keys {
                c.arg("--decryption-key");
                c.arg(decryption_key);
            }
        }

        if config.insecure_skip_tls_verification.unwrap_or_default() {
            c.arg("--tls-verify=false");
        }
        c.stdout(Stdio::null()).stderr(Stdio::piped());
        Ok(c)
    }
}

/// BlobInfo collects known information about a blob
#[derive(Debug, serde::Deserialize)]
pub struct ConvertedLayerInfo {
    /// Uncompressed digest of a layer; for more information, see
    /// https://github.com/opencontainers/image-spec/blob/main/config.md#layer-diffid
    pub digest: String,

    /// Size of blob
    pub size: i64,

    /// Mediatype of blob
    pub media_type: oci_spec::image::MediaType,
}

impl ImageProxy {
    /// Create an image proxy that fetches the target image, using default configuration.
    pub async fn new() -> Result<Self> {
        Self::new_with_config(Default::default()).await
    }

    /// Create an image proxy that fetches the target image
    #[instrument]
    pub async fn new_with_config(config: ImageProxyConfig) -> Result<Self> {
        let mut c = Command::try_from(config)?;
        let (mysock, theirsock) = rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::SEQPACKET,
            rustix::net::SocketFlags::CLOEXEC,
            None,
        )?;
        c.stdin(Stdio::from(theirsock));
        let child = c.spawn().context("Failed to spawn skopeo")?;
        tracing::debug!("Spawned skopeo pid={:?}", child.id());
        // Here we use std sync API via thread because tokio installs
        // a SIGCHLD handler which can conflict with e.g. the glib one
        // which may also be in process.
        // xref https://github.com/tokio-rs/tokio/issues/3520#issuecomment-968985861
        let childwait = tokio::task::spawn_blocking(move || child.wait_with_output());
        let sockfd = Arc::new(Mutex::new(mysock));

        let mut r = Self {
            sockfd,
            childwait: Arc::new(AsyncMutex::new(Box::pin(childwait))),
            protover: semver::Version::new(0, 0, 0),
        };

        // Verify semantic version
        let protover = r.impl_request::<String, _, ()>("Initialize", []).await?.0;
        tracing::debug!("Remote protocol version: {protover}");
        let protover = semver::Version::parse(protover.as_str())?;
        // Previously we had a feature to opt-in to requiring newer versions using `if cfg!()`.
        let supported = &*BASE_PROTO_VERSION;
        if !supported.matches(&protover) {
            return Err(anyhow!(
                "Unsupported protocol version {} (compatible: {})",
                protover,
                supported
            ));
        }
        r.protover = protover;

        Ok(r)
    }

    async fn impl_request_raw<T: serde::de::DeserializeOwned + Send + 'static>(
        sockfd: Arc<Mutex<OwnedFd>>,
        req: Request,
    ) -> Result<(T, Option<(OwnedFd, u32)>)> {
        tracing::trace!("sending request {}", req.method.as_str());
        // TODO: Investigate https://crates.io/crates/uds for SOCK_SEQPACKET tokio
        let r = tokio::task::spawn_blocking(move || {
            let sockfd = sockfd.lock().unwrap();
            let sendbuf = serde_json::to_vec(&req)?;
            let sockfd = &*sockfd;
            rustix::net::send(sockfd, &sendbuf, rustix::net::SendFlags::empty())?;
            drop(sendbuf);
            let mut buf = [0u8; MAX_MSG_SIZE];
            let mut cmsg_space = vec![0; rustix::cmsg_space!(ScmRights(1))];
            let mut cmsg_buffer = rustix::net::RecvAncillaryBuffer::new(&mut cmsg_space);
            let iov = std::io::IoSliceMut::new(buf.as_mut());
            let mut iov = [iov];
            let nread = rustix::net::recvmsg(
                sockfd,
                &mut iov,
                &mut cmsg_buffer,
                rustix::net::RecvFlags::CMSG_CLOEXEC,
            )?
            .bytes;
            let fdret = cmsg_buffer
                .drain()
                .filter_map(|m| match m {
                    rustix::net::RecvAncillaryMessage::ScmRights(f) => Some(f),
                    _ => None,
                })
                .flatten()
                .next();
            let buf = &buf[..nread];
            let reply: Reply = serde_json::from_slice(buf).context("Deserializing reply")?;
            if !reply.success {
                return Err(anyhow!("remote error: {}", reply.error));
            }
            let fdret = match (fdret, reply.pipeid) {
                (Some(fd), n) => {
                    if n == 0 {
                        return Err(anyhow!("got fd but no pipeid"));
                    }
                    Some((fd, n))
                }
                (None, n) => {
                    if n != 0 {
                        return Err(anyhow!("got no fd with pipeid {}", n));
                    }
                    None
                }
            };
            let reply = serde_json::from_value(reply.value).context("Deserializing value")?;
            Ok((reply, fdret))
        })
        .await??;
        tracing::trace!("completed request");
        Ok(r)
    }

    #[instrument(skip(args))]
    async fn impl_request<R: serde::de::DeserializeOwned + Send + 'static, T, I>(
        &self,
        method: &str,
        args: T,
    ) -> Result<(R, Option<(OwnedFd, u32)>)>
    where
        T: IntoIterator<Item = I>,
        I: Into<serde_json::Value>,
    {
        let req = Self::impl_request_raw(Arc::clone(&self.sockfd), Request::new(method, args));
        let mut childwait = self.childwait.lock().await;
        tokio::select! {
            r = req => {
                Ok(r.with_context(|| format!("Failed to invoke skopeo proxy method {method}"))?)
            }
            r = childwait.as_mut() => {
                let r = r??;
                let stderr = String::from_utf8_lossy(&r.stderr);
                Err(anyhow::anyhow!("skopeo proxy unexpectedly exited during request method {}: {}\n{}", method, r.status, stderr))
            }
        }
    }

    #[instrument]
    async fn finish_pipe(&self, pipeid: u32) -> Result<()> {
        tracing::debug!("closing pipe");
        let (r, fd) = self.impl_request("FinishPipe", [pipeid]).await?;
        if fd.is_some() {
            return Err(anyhow!("Unexpected fd in finish_pipe reply"));
        }
        Ok(r)
    }

    #[instrument]
    pub async fn open_image(&self, imgref: &str) -> Result<OpenedImage> {
        tracing::debug!("opening image");
        let (imgid, _) = self
            .impl_request::<u32, _, _>("OpenImage", [imgref])
            .await?;
        Ok(OpenedImage(imgid))
    }

    #[instrument]
    #[cfg(feature = "proxy_v0_2_4")]
    pub async fn open_image_optional(&self, imgref: &str) -> Result<Option<OpenedImage>> {
        tracing::debug!("opening image");
        let (imgid, _) = self
            .impl_request::<u32, _, _>("OpenImageOptional", [imgref])
            .await?;
        if imgid == 0 {
            Ok(None)
        } else {
            Ok(Some(OpenedImage(imgid)))
        }
    }

    #[instrument]
    pub async fn close_image(&self, img: &OpenedImage) -> Result<()> {
        tracing::debug!("closing image");
        let (r, _) = self.impl_request("CloseImage", [img.0]).await?;
        Ok(r)
    }

    async fn read_all_fd(&self, fd: Option<(OwnedFd, u32)>) -> Result<Vec<u8>> {
        let (fd, pipeid) = fd.ok_or_else(|| anyhow!("Missing fd from reply"))?;
        let fd = tokio::fs::File::from_std(std::fs::File::from(fd));
        let mut fd = tokio::io::BufReader::new(fd);
        let mut r = Vec::new();
        let reader = fd.read_to_end(&mut r);
        let (nbytes, finish) = tokio::join!(reader, self.finish_pipe(pipeid));
        finish?;
        assert_eq!(nbytes?, r.len());
        Ok(r)
    }

    /// Fetch the manifest as raw bytes, converted to OCI if necessary.
    /// The original digest of the unconverted manifest is also returned.
    /// For more information on OCI manifests, see <https://github.com/opencontainers/image-spec/blob/main/manifest.md>
    pub async fn fetch_manifest_raw_oci(&self, img: &OpenedImage) -> Result<(String, Vec<u8>)> {
        let (digest, fd) = self.impl_request("GetManifest", [img.0]).await?;
        Ok((digest, self.read_all_fd(fd).await?))
    }

    /// Fetch the manifest.
    /// For more information on OCI manifests, see <https://github.com/opencontainers/image-spec/blob/main/manifest.md>
    pub async fn fetch_manifest(
        &self,
        img: &OpenedImage,
    ) -> Result<(String, oci_spec::image::ImageManifest)> {
        let (digest, raw) = self.fetch_manifest_raw_oci(img).await?;
        let manifest =
            serde_json::from_slice(&raw).context("Deserializing manifest from skopeo")?;
        Ok((digest, manifest))
    }

    /// Fetch the config.
    /// For more information on OCI config, see <https://github.com/opencontainers/image-spec/blob/main/config.md>
    pub async fn fetch_config_raw(&self, img: &OpenedImage) -> Result<Vec<u8>> {
        let (_, fd) = self
            .impl_request::<(), _, _>("GetFullConfig", [img.0])
            .await?;
        self.read_all_fd(fd).await
    }

    /// Fetch the config.
    /// For more information on OCI config, see <https://github.com/opencontainers/image-spec/blob/main/config.md>
    pub async fn fetch_config(
        &self,
        img: &OpenedImage,
    ) -> Result<oci_spec::image::ImageConfiguration> {
        let raw = self.fetch_config_raw(img).await?;
        serde_json::from_slice(&raw).context("Deserializing config from skopeo")
    }

    /// Fetch a blob identified by e.g. `sha256:<digest>`.
    /// <https://github.com/opencontainers/image-spec/blob/main/descriptor.md>
    ///
    /// The requested size and digest are verified (by the proxy process).
    #[instrument]
    pub async fn get_blob(
        &self,
        img: &OpenedImage,
        digest: &str,
        size: u64,
    ) -> Result<(
        impl AsyncBufRead + Send + Unpin,
        impl Future<Output = Result<()>> + Unpin + '_,
    )> {
        // For previous discussion on digest/size verification, see
        // https://github.com/cgwalters/container-image-proxy/issues/1#issuecomment-926712009
        tracing::debug!("fetching blob");
        let args: Vec<serde_json::Value> =
            vec![img.0.into(), digest.to_string().into(), size.into()];
        let (_bloblen, fd) = self.impl_request::<i64, _, _>("GetBlob", args).await?;
        let (fd, pipeid) = fd.ok_or_else(|| anyhow!("Missing fd from reply"))?;
        let fd = tokio::fs::File::from_std(std::fs::File::from(fd));
        let fd = tokio::io::BufReader::new(fd);
        let finish = Box::pin(self.finish_pipe(pipeid));
        Ok((fd, finish))
    }

    ///Returns data that can be used to find the "diffid" corresponding to a particular layer.
    #[instrument]
    pub async fn get_layer_info(
        &self,
        img: &OpenedImage,
    ) -> Result<Option<Vec<ConvertedLayerInfo>>> {
        tracing::debug!("Getting layer info");
        if !LAYER_INFO_PROTO_VERSION.matches(&self.protover) {
            return Ok(None);
        }
        let reply = self.impl_request("GetLayerInfo", [img.0]).await?;
        let layers: Vec<ConvertedLayerInfo> = reply.0;
        Ok(Some(layers))
    }

    /// Close the connection and wait for the child process to exit successfully.
    #[instrument]
    pub async fn finalize(self) -> Result<()> {
        let _ = &self;
        let req = Request::new_bare("Shutdown");
        let sendbuf = serde_json::to_vec(&req)?;
        // SAFETY: Only panics if a worker thread already panic'd
        let sockfd = Arc::try_unwrap(self.sockfd).unwrap().into_inner().unwrap();
        rustix::net::send(sockfd, &sendbuf, rustix::net::SendFlags::empty())?;
        drop(sendbuf);
        tracing::debug!("sent shutdown request");
        let mut childwait = self.childwait.lock().await;
        let output = childwait.as_mut().await??;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("proxy failed: {}\n{}", output.status, stderr)
        }
        tracing::debug!("proxy exited successfully");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, Write};

    use super::*;

    fn validate(c: Command, contains: &[&str], not_contains: &[&str]) {
        // Format via debug, because
        // https://doc.rust-lang.org/std/process/struct.Command.html#method.get_args
        // is experimental
        let d = format!("{:?}", c);
        for c in contains {
            assert!(d.contains(c), "{} missing {}", d, c);
        }
        for c in not_contains {
            assert!(!d.contains(c), "{} should not contain {}", d, c);
        }
    }

    #[test]
    fn proxy_configs() {
        let tmpd = &cap_tempfile::tempdir(cap_std::ambient_authority()).unwrap();

        let c = Command::try_from(ImageProxyConfig::default()).unwrap();
        validate(
            c,
            &["experimental-image-proxy"],
            &["--no-creds", "--tls-verify", "--authfile"],
        );

        let c = Command::try_from(ImageProxyConfig {
            authfile: Some(PathBuf::from("/path/to/authfile")),
            ..Default::default()
        })
        .unwrap();
        validate(c, &[r"--authfile", "/path/to/authfile"], &[]);

        let c = Command::try_from(ImageProxyConfig {
            decryption_keys: Some(vec![PathBuf::from("/path/to/decryption_key")]),
            ..Default::default()
        })
        .unwrap();
        validate(c, &[r"--decryption-key", "/path/to/decryption_key"], &[]);

        let c = Command::try_from(ImageProxyConfig {
            certificate_directory: Some(PathBuf::from("/path/to/certs")),
            ..Default::default()
        })
        .unwrap();
        validate(c, &[r"--cert-dir", "/path/to/certs"], &[]);

        let c = Command::try_from(ImageProxyConfig {
            insecure_skip_tls_verification: Some(true),
            ..Default::default()
        })
        .unwrap();
        validate(c, &[r"--tls-verify=false"], &[]);

        let mut tmpf = cap_tempfile::TempFile::new_anonymous(tmpd).unwrap();
        tmpf.write_all(r#"{ "auths": {} "#.as_bytes()).unwrap();
        tmpf.seek(std::io::SeekFrom::Start(0)).unwrap();
        let c = Command::try_from(ImageProxyConfig {
            auth_data: Some(tmpf.into_std()),
            ..Default::default()
        })
        .unwrap();
        validate(c, &["--authfile", "/proc/self/fd/100"], &[]);
    }
}
