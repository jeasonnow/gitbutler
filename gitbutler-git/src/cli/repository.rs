use super::executor::{AskpassServer, GitExecutor, Pid, Socket};
use crate::RefSpec;
use futures::{select, FutureExt};
use rand::Rng;
use std::{collections::HashMap, path::Path, time::Duration};

/// The number of characters in the secret used for checking
/// askpass invocations by ssh/git when connecting to our process.
const ASKPASS_SECRET_LENGTH: usize = 24;

/// Higher level errors that can occur when interacting with the CLI.
///
/// You probably don't want to use this type. Use [`Error`] instead.
#[derive(Debug, thiserror::Error)]
pub enum RepositoryError<
    Eexec: std::error::Error + core::fmt::Debug + Send + Sync + 'static,
    Easkpass: std::error::Error + core::fmt::Debug + Send + Sync + 'static,
    Esocket: std::error::Error + core::fmt::Debug + Send + Sync + 'static,
> {
    #[error("failed to execute git command: {0}")]
    Exec(Eexec),
    #[error("failed to create askpass server: {0}")]
    AskpassServer(Easkpass),
    #[error("i/o error communicating with askpass utility: {0}")]
    AskpassIo(Esocket),
    #[error(
        "git command exited with non-zero exit code {status}: {args:?}\n\nSTDOUT:\n{stdout}\n\nSTDERR:\n{stderr}"
    )]
    Failed {
        status: usize,
        args: Vec<String>,
        stdout: String,
        stderr: String,
    },
    #[error("failed to determine path to this executable: {0}")]
    NoSelfExe(std::io::Error),
    #[error("askpass secret mismatch")]
    AskpassSecretMismatch,
    #[error("git requires authorization credentials but none were provided: prompt was {0:?}")]
    NeedsAuthorization(String),
    #[error("unable to determine PID of askpass peer: {0}")]
    NoPid(Esocket),
    #[cfg(unix)]
    #[error("unable to determine UID of askpass peer: {0}")]
    NoUid(Esocket),
    #[error("failed to perform askpass security check; no such PID: {0}")]
    NoSuchPid(Pid),
    #[error("failed to perform askpass security check; device mismatch")]
    AskpassDeviceMismatch,
    #[error("failed to perform askpass security check; executable mismatch")]
    AskpassExecutableMismatch,
}

/// Higher level errors that can occur when interacting with the CLI.
pub type Error<E> = RepositoryError<
    <E as GitExecutor>::Error,
    <<E as GitExecutor>::ServerHandle as AskpassServer>::Error,
    <<<E as GitExecutor>::ServerHandle as AskpassServer>::SocketHandle as Socket>::Error,
>;

#[cold]
async fn execute_with_auth_harness<P, F, Fut, E>(
    repo_path: P,
    executor: E,
    args: &[&str],
    envs: Option<HashMap<String, String>>,
    on_prompt: F,
) -> Result<(usize, String, String), Error<E>>
where
    P: AsRef<Path>,
    E: GitExecutor,
    F: Fn(&str) -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    let path = std::env::current_exe().map_err(Error::<E>::NoSelfExe)?;

    // TODO(qix-): Get parent PID of connecting processes to make sure they're us.
    //let our_pid = std::process::id();

    // TODO(qix-): This is a bit of a hack. Under a test environment,
    // TODO(qix-): Cargo is running a test runner with a quasi-random
    // TODO(qix-): suffix. The actual executables live in the parent directory.
    // TODO(qix-): Thus, we have to do this under test. It's not ideal, but
    // TODO(qix-): it works for now.
    #[cfg(test)]
    let path = path.parent().unwrap();

    let askpath_path = path
        .with_file_name("gitbutler-git-askpass")
        .to_string_lossy()
        .into_owned();

    #[cfg(not(target_os = "windows"))]
    let setsid_path = path
        .with_file_name("gitbutler-git-setsid")
        .to_string_lossy()
        .into_owned();

    let askpath_stat = executor
        .stat(&askpath_path)
        .await
        .map_err(Error::<E>::Exec)?;

    #[cfg(not(target_os = "windows"))]
    let setsid_stat = executor
        .stat(&setsid_path)
        .await
        .map_err(Error::<E>::Exec)?;

    #[allow(unsafe_code)]
    let sock_server = unsafe { executor.create_askpass_server() }
        .await
        .map_err(Error::<E>::Exec)?;

    // FIXME(qix-): This is probably not cryptographically secure, did this in a bit
    // FIXME(qix-): of a hurry. We should probably use a proper CSPRNG here, but this
    // FIXME(qix-): is probably fine for now (as this security mechanism is probably
    // FIXME(qix-): overkill to begin with).
    let secret = rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(ASKPASS_SECRET_LENGTH)
        .map(char::from)
        .collect::<String>();

    let mut envs = envs.unwrap_or_default();
    envs.insert("GITBUTLER_ASKPASS_PIPE".into(), sock_server.to_string());
    envs.insert("GITBUTLER_ASKPASS_SECRET".into(), secret.clone());
    envs.insert("SSH_ASKPASS".into(), askpath_path);

    // DISPLAY is required by SSH to check SSH_ASKPASS.
    // Please don't ask us why, it's unclear.
    if !std::env::var("DISPLAY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        envs.insert("DISPLAY".into(), ":".into());
    }

    envs.insert(
        "GIT_SSH_COMMAND".into(),
        format!(
            "{}{} -o StrictHostKeyChecking=accept-new -o KbdInteractiveAuthentication=no{}",
            {
                #[cfg(not(target_os = "windows"))]
                {
                    format!("{setsid_path} ")
                }
                #[cfg(target_os = "windows")]
                {
                    ""
                }
            },
            envs.get("GIT_SSH_COMMAND").unwrap_or(&"ssh".into()),
            {
                // In test environments, we don't want to pollute the user's known hosts file.
                // So, we just use /dev/null instead.
                #[cfg(test)]
                {
                    " -o UserKnownHostsFile=/dev/null"
                }
                #[cfg(not(test))]
                {
                    ""
                }
            }
        ),
    );

    let mut child_process = core::pin::pin! {
        async {
            executor
                .execute(args, repo_path, Some(envs))
                .await
                .map_err(Error::<E>::Exec)
        }.fuse()
    };

    loop {
        select! {
            res = child_process => {
                return res;
            },
            res = sock_server.accept(Some(Duration::from_secs(60))).fuse() => {
                let mut sock = res.map_err(Error::<E>::AskpassServer)?;

                // get the PID of the peer
                let peer_pid = sock.pid().map_err(Error::<E>::NoPid)?;

                // get the full image path of the peer id; this is pretty expensive at the moment.
                // TODO(qix-): see if dropping sysinfo for a more bespoke implementation is worth it.
                let mut system = sysinfo::System::new();
                system.refresh_processes();
                let peer_path = system
                    .process(sysinfo::Pid::from_u32(peer_pid.try_into().map_err(|_| Error::<E>::NoSuchPid(peer_pid))?))
                    .and_then(|p| p.exe().map(|exe| exe.to_string_lossy().into_owned()))
                    .ok_or(Error::<E>::NoSuchPid(peer_pid))?;

                // stat the askpass executable that is being invoked
                let peer_stat = executor.stat(&peer_path).await.map_err(Error::<E>::Exec)?;

                if peer_stat.ino == askpath_stat.ino {
                    if peer_stat.dev != askpath_stat.dev {
                        return Err(Error::<E>::AskpassDeviceMismatch)?;
                    }
                } else if peer_stat.ino == setsid_stat.ino {
                    if peer_stat.dev != setsid_stat.dev {
                        return Err(Error::<E>::AskpassDeviceMismatch)?;
                    }
                } else {
                    return Err(Error::<E>::AskpassExecutableMismatch)?;
                }

                // await for peer to send secret
                let peer_secret = sock.read_line().await.map_err(Error::<E>::AskpassIo)?;

                // check the secret
                if peer_secret.trim() != secret {
                    return Err(Error::<E>::AskpassSecretMismatch)?;
                }

                // get the prompt
                let prompt = sock.read_line().await.map_err(Error::<E>::AskpassIo)?;

                // call the prompt handler
                let response = on_prompt(&prompt).await;
                if let Some(response) = response {
                    sock.write_line(&response).await.map_err(Error::<E>::AskpassIo)?;
                } else {
                    return Err(Error::<E>::NeedsAuthorization(prompt));
                }
            }
        }
    }
}

/// Fetches the given refspec from the given remote in the repository
/// at the given path. Any prompts for the user are passed to the asynchronous
/// callback `on_promp` which should return the user's response or `None` if the
/// operation should be aborted, in which case an `Err` value is returned from this
/// function.
pub async fn fetch<P, F, Fut, E>(
    repo_path: P,
    executor: E,
    remote: &str,
    refspec: RefSpec,
    on_prompt: F,
) -> Result<(), crate::Error<Error<E>>>
where
    P: AsRef<Path>,
    E: GitExecutor,
    F: Fn(&str) -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    let mut args = vec!["fetch", "--quiet", "--no-write-fetch-head"];

    let refspec = refspec.to_string();

    args.push(remote);
    args.push(&refspec);

    let (status, stdout, stderr) =
        execute_with_auth_harness(repo_path, executor, &args, None, on_prompt).await?;

    if status == 0 {
        Ok(())
    } else {
        // Was the ref not found?
        if let Some(refname) = stderr
            .lines()
            .find(|line| line.to_lowercase().contains("couldn't find remote ref"))
            .map(|line| line.split_whitespace().last().unwrap_or_default())
        {
            Err(crate::Error::RefNotFound(refname.to_owned()))?
        } else if stderr.to_lowercase().contains("permission denied") {
            Err(crate::Error::AuthorizationFailed(Error::<E>::Failed {
                status,
                args: args.into_iter().map(Into::into).collect(),
                stdout,
                stderr,
            }))?
        } else {
            Err(Error::<E>::Failed {
                status,
                args: args.into_iter().map(Into::into).collect(),
                stdout,
                stderr,
            })?
        }
    }
}
