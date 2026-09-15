//! Private anonymous pipes, bounded frames, and a reaping child supervisor.
//! Never derive Debug for payloads: Init/Play/Reply contain credentials.
use crate::{TuneError, streaming::AuthStatus};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::watch;

const MAX_FRAME: usize = 8 * 1024 * 1024;
pub(super) const DEADLINE: Duration = Duration::from_secs(45);

#[derive(Serialize, Deserialize)]
pub(super) enum Operation {
    Init {
        tokens: Value,
    },
    Status,
    Pair,
    Search {
        query: String,
        limit: usize,
    },
    Track {
        id: String,
    },
    Album {
        id: String,
    },
    AlbumTracks {
        id: String,
    },
    Artist {
        id: String,
    },
    ArtistAlbums {
        id: String,
    },
    Playlist {
        id: String,
    },
    PlaylistTracks {
        id: String,
    },
    UserPlaylists,
    UserTracks,
    UserAlbums,
    UserArtists,
    PlaylistLibrary,
    Play {
        tokens: Value,
        id: String,
        seek_ms: u32,
    },
}

#[derive(Serialize, Deserialize)]
pub(super) struct Failure {
    message: String,
    unsupported: bool,
}
impl Failure {
    pub fn from_tune(error: TuneError) -> Self {
        Self {
            unsupported: matches!(error, TuneError::Unsupported(_)),
            message: error.to_string(),
        }
    }
    pub fn into_tune(self) -> TuneError {
        if self.unsupported {
            TuneError::Unsupported(self.message)
        } else {
            self.message.into()
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct Reply {
    pub result: Result<Value, Failure>,
    pub status: AuthStatus,
    pub details: Value,
    pub tokens: Value,
}

pub(super) async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|_| "Spotify IPC serialization failed")?;
    if bytes.len() > MAX_FRAME {
        return Err("Spotify IPC frame exceeds limit".into());
    }
    writer
        .write_u32(bytes.len() as u32)
        .await
        .map_err(|_| "Spotify worker pipe closed")?;
    writer
        .write_all(&bytes)
        .await
        .map_err(|_| "Spotify worker pipe closed")?;
    writer
        .flush()
        .await
        .map_err(|_| "Spotify worker pipe closed".into())
}

pub(super) async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(
    reader: &mut R,
) -> Result<T, String> {
    let length = reader
        .read_u32()
        .await
        .map_err(|_| "Spotify worker stopped or closed its pipe")? as usize;
    if length == 0 || length > MAX_FRAME {
        return Err("Spotify IPC frame exceeds limit".into());
    }
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(|_| "Spotify worker reply was truncated")?;
    serde_json::from_slice(&bytes).map_err(|_| "Spotify worker reply is invalid".into())
}

pub(super) struct ChildProcess {
    pub input: ChildStdin,
    pub output: BufReader<ChildStdout>,
    pub life: Arc<AtomicBool>,
    pub kill: watch::Sender<bool>,
    exit: watch::Receiver<Option<i32>>,
}

impl ChildProcess {
    pub fn spawn(mode: &str) -> Result<Self, TuneError> {
        let executable = std::env::current_exe()
            .map_err(|_| TuneError::from("Cannot locate Spotify worker executable".to_owned()))?;
        let mut command = Command::new(executable);
        command.arg("--spotify-native-worker").arg(mode);
        Self::from_command(command).map_err(TuneError::from)
    }
    fn from_command(mut command: Command) -> Result<Self, String> {
        // No credentials in argv, environment, filesystem or child stderr.
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|_| "Cannot start Spotify worker")?;
        let pid = child.id();
        let input = child
            .stdin
            .take()
            .ok_or("Spotify worker has no input pipe")?;
        let output = BufReader::new(
            child
                .stdout
                .take()
                .ok_or("Spotify worker has no output pipe")?,
        );
        let life = Arc::new(AtomicBool::new(true));
        let alive = life.clone();
        let (kill, mut stop) = watch::channel(false);
        let (finished, exit) = watch::channel(None);
        tokio::spawn(async move {
            let result = tokio::select! {
                result = child.wait() => result,
                _ = stop.changed() => {
                    // Mandatory even when stdout is backpressured or the child
                    // ignores EOF. Always reap it; no orphan decoder or zombie.
                    let _ = child.kill().await;
                    child.wait().await
                }
            };
            let code = result.ok().and_then(|status| status.code()).unwrap_or(-1);
            alive.store(false, Ordering::Release);
            let _ = finished.send(Some(code));
            tracing::debug!(?pid, code, "spotify_worker_reaped");
        });
        tracing::debug!(?pid, "spotify_worker_started");
        Ok(Self {
            input,
            output,
            life,
            kill,
            exit,
        })
    }
    pub fn alive(&self) -> bool {
        self.life.load(Ordering::Acquire)
    }
    pub async fn rpc(&mut self, operation: &Operation) -> Result<Reply, TuneError> {
        tokio::time::timeout(DEADLINE, async {
            write_frame(&mut self.input, operation).await?;
            read_frame(&mut self.output).await
        })
        .await
        .map_err(|_| TuneError::from("Spotify worker request timed out".to_owned()))?
        .map_err(TuneError::from)
    }
    pub async fn successful_exit(&mut self) -> Result<(), String> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(code) = *self.exit.borrow() {
                    return if code == 0 {
                        Ok(())
                    } else {
                        Err(format!("Spotify worker exited with status {code}"))
                    };
                }
                self.exit
                    .changed()
                    .await
                    .map_err(|_| "Spotify worker supervisor stopped".to_owned())?;
            }
        })
        .await
        .map_err(|_| "Spotify worker did not exit after EOF".to_owned())?
    }
}
impl Drop for ChildProcess {
    fn drop(&mut self) {
        self.life.store(false, Ordering::Release);
        let _ = self.kill.send(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn native_ipc_rejects_oversized_frame_before_allocating_payload() {
        let mut input = std::io::Cursor::new(((MAX_FRAME + 1) as u32).to_be_bytes());
        assert!(
            read_frame::<_, Value>(&mut input)
                .await
                .unwrap_err()
                .contains("exceeds limit")
        );
    }
    #[tokio::test]
    async fn native_ipc_roundtrip_preserves_payload_without_log_text() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, &serde_json::json!({"credentials": "fixture"}))
            .await
            .unwrap();
        let result: Value = read_frame(&mut b).await.unwrap();
        assert_eq!(result["credentials"], "fixture");
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn native_worker_drop_kills_and_reaps_a_child_ignoring_stdin() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 30"]);
        let child = ChildProcess::from_command(command).unwrap();
        let mut exit = child.exit.clone();
        drop(child);
        tokio::time::timeout(Duration::from_secs(2), exit.changed())
            .await
            .expect("Spotify worker survived cancellation; the server must kill and reap it")
            .unwrap();
        assert!(exit.borrow().is_some());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn native_worker_nonzero_exit_is_an_error_not_a_server_exit() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 17"]);
        let mut child = ChildProcess::from_command(command).unwrap();
        assert!(child.successful_exit().await.unwrap_err().contains("17"));
        assert!(!child.alive());
    }
}
