//! Optional local provider, not an APK bundled into Tune. Anonymous framed IPC
//! only: the provider never receives Spotify credentials, URLs, or audio bytes.
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;
use tokio::{io::BufReader, process::Command};

const DEADLINE: Duration = Duration::from_secs(20);

#[derive(Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum Request<'a> {
    Info {
        protocol: u8,
    },
    Derive {
        protocol: u8,
        file_id: &'a str,
        obfuscated_key: &'a str,
    },
}
// Never derive Debug: these replies contain a protocol token / a content key.
#[derive(Deserialize)]
struct Info {
    protocol: u8,
    playplay_version: u8,
    token: String,
}
#[derive(Deserialize)]
struct Derived {
    protocol: u8,
    key: String,
}

pub(super) struct KeyProvider {
    child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: BufReader<tokio::process::ChildStdout>,
    pub(super) token: [u8; 16],
}

pub(super) fn hex16(value: &str) -> Result<[u8; 16], String> {
    let mut bytes = [0; 16];
    hex::decode_to_slice(value, &mut bytes)
        .map_err(|_| "Spotify key provider returned an invalid 16-byte value")?;
    Ok(bytes)
}

impl KeyProvider {
    pub(super) async fn open() -> Result<Self, String> {
        if !cfg!(unix) {
            return Err(
                "Spotify lossless external provider currently requires Unix process supervision"
                    .into(),
            );
        }
        let path = std::env::var_os("TUNE_SPOTIFY_PLAYPLAY_HELPER")
            .ok_or("Spotify lossless requires TUNE_SPOTIFY_PLAYPLAY_HELPER")?;
        if !Path::new(&path).is_absolute() {
            return Err("Spotify key provider must be an absolute executable path".into());
        }
        Self::from_command(Command::new(path)).await
    }

    async fn from_command(mut command: Command) -> Result<Self, String> {
        let mut child = command
            // One configured executable, no shell interpolation or key in argv.
            // Inherits the audio worker's process group. The server kills that
            // group on cancellation, including this helper during a derivation.
            // Do not inherit DB/cloud/account environment values from Tune.
            // A script provider needs an absolute interpreter in its shebang.
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| "Spotify key provider could not start")?;
        let input = child
            .stdin
            .take()
            .ok_or("Spotify key provider has no input")?;
        let output = BufReader::new(
            child
                .stdout
                .take()
                .ok_or("Spotify key provider has no output")?,
        );
        let mut provider = Self {
            child,
            input,
            output,
            token: [0; 16],
        };
        let result = provider
            .call::<Info>(&Request::Info { protocol: 1 })
            .await
            .and_then(|info| {
                if info.protocol != 1 || info.playplay_version != 5 {
                    return Err("Spotify key provider protocol is unsupported".into());
                }
                hex16(&info.token)
            });
        match result {
            Ok(token) => provider.token = token,
            Err(error) => {
                let _ = provider.child.kill().await;
                let _ = provider.child.wait().await;
                return Err(error);
            }
        }
        Ok(provider)
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &mut self,
        request: &Request<'_>,
    ) -> Result<T, String> {
        tokio::time::timeout(DEADLINE, async {
            super::super::ipc::write_frame(&mut self.input, request).await?;
            super::super::ipc::read_frame_limited(&mut self.output, 4096).await
        })
        .await
        .map_err(|_| "Spotify key provider timed out")?
    }

    pub(super) async fn derive(
        mut self,
        file_id: &[u8; 20],
        obfuscated: &[u8; 16],
    ) -> Result<[u8; 16], String> {
        let result = self
            .call::<Derived>(&Request::Derive {
                protocol: 1,
                file_id: &hex::encode(file_id),
                obfuscated_key: &hex::encode(obfuscated),
            })
            .await
            .and_then(|reply| {
                if reply.protocol != 1 {
                    return Err("Spotify key provider reply version is invalid".into());
                }
                hex16(&reply.key)
            });
        // Reap even on a malformed reply. No helper remains for the audio phase.
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        result
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(replies: &[serde_json::Value]) -> Command {
        let mut bytes = Vec::new();
        for reply in replies {
            let json = serde_json::to_vec(reply).unwrap();
            bytes.extend_from_slice(&(json.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&json);
        }
        let octal: String = bytes.iter().map(|byte| format!("\\{byte:03o}")).collect();
        let mut command = Command::new("/bin/sh");
        command.env("TUNE_PROVIDER_TEST_SECRET", "synthetic-only");
        // Every byte is an octal escape, including JSON's punctuation. Only
        // synthetic test values; no inherited user input is interpreted here.
        command.args(["-c", &format!("test -z \"${{TUNE_PROVIDER_TEST_SECRET+x}}\" || exit 19; printf '{octal}'; exec cat >/dev/null")]);
        command
    }
    #[tokio::test]
    async fn native_lossless_provider_derives_over_pipes_and_is_reaped() {
        let provider = KeyProvider::from_command(fixture(&[
            json!({"protocol":1,"playplay_version":5,"token":"11".repeat(16)}),
            json!({"protocol":1,"key":"22".repeat(16)}),
        ]))
        .await
        .unwrap();
        assert_eq!(provider.token, [0x11; 16]);
        let pid = provider.child.id().unwrap();
        let key = provider.derive(&[0x33; 20], &[0x44; 16]).await.unwrap();
        assert_eq!(key, [0x22; 16]);
        assert_eq!(
            unsafe { libc::kill(pid as i32, 0) },
            -1,
            "Provider must be reaped before decoding starts"
        );
    }
    #[tokio::test]
    async fn native_lossless_provider_refuses_incompatible_and_malformed_replies() {
        for info in [
            json!({"protocol":2,"playplay_version":5,"token":"11".repeat(16)}),
            json!({"protocol":1,"playplay_version":4,"token":"11".repeat(16)}),
            json!({"protocol":1,"playplay_version":5,"token":"short"}),
        ] {
            assert!(KeyProvider::from_command(fixture(&[info])).await.is_err());
        }
        let provider = KeyProvider::from_command(fixture(&[
            json!({"protocol":1,"playplay_version":5,"token":"11".repeat(16)}),
            json!({"protocol":1,"key":"not a key"}),
        ]))
        .await
        .unwrap();
        assert!(provider.derive(&[0; 20], &[0; 16]).await.is_err());
    }
}
