use std::{path::Path, process::Command};

use anyhow::anyhow;

const CHECKPOINT_SAVE: &str = "[subpub] CHECKPOINT_SAVE";
const CHECKPOINT_REVERT: &str = "[subpub] CHECKPOINT_REVERT";

#[derive(PartialEq, Eq)]
pub enum GitCheckpoint {
    Save,
    RevertLater,
}

fn git_checkpoint<P: AsRef<Path>>(root: P, op: GitCheckpoint) -> anyhow::Result<()> {
    let mut cmd = Command::new("git");
    let git_status_output = cmd
        .current_dir(&root)
        .arg("status")
        .arg("--porcelain=v1")
        .output()?;
    if !git_status_output.status.success() {
        return Err(anyhow!(
            "Failed to get git status for {:?}",
            root.as_ref().as_os_str()
        ));
    }

    let git_status_output = String::from_utf8_lossy(&git_status_output.stdout[..]);
    let git_status_output = git_status_output.trim();
    if !git_status_output.is_empty() {
        let mut cmd = Command::new("git");
        if !cmd
            .current_dir(&root)
            .arg("add")
            .arg(".")
            .status()?
            .success()
        {
            return Err(anyhow!(
                "Failed to `git add` files for {:?}",
                root.as_ref().as_os_str()
            ));
        }

        let commit_msg = match op {
            GitCheckpoint::Save => CHECKPOINT_SAVE,
            GitCheckpoint::RevertLater => CHECKPOINT_REVERT,
        };
        let mut cmd = Command::new("git");
        if !cmd
            .current_dir(&root)
            .arg("commit")
            .arg("--quiet")
            .arg("-m")
            .arg(commit_msg)
            .status()?
            .success()
        {
            return Err(anyhow!(
                "Failed to `git commit` files for {:?}",
                root.as_ref().as_os_str()
            ));
        }
    };

    Ok(())
}

pub fn with_git_checkpoint<T, P: AsRef<Path>, F: FnOnce() -> T>(
    root: P,
    op: GitCheckpoint,
    func: F,
) -> anyhow::Result<T> {
    if op != GitCheckpoint::Save {
        git_checkpoint(&root, GitCheckpoint::Save)?;
    };
    let result = func();
    git_checkpoint(&root, op)?;
    Ok(result)
}

pub fn git_checkpoint_revert<P: AsRef<Path>>(root: P) -> anyhow::Result<()> {
    loop {
        let mut cmd = Command::new("git");
        let output = cmd
            .current_dir(&root)
            .arg("log")
            .arg("-1")
            .arg("--pretty=%B")
            .output()?;
        if !output.status.success() {
            return Err(anyhow!(
                "Failed to get the last commit's message for {:?}. Command failed: {:?}",
                root.as_ref(),
                cmd
            ));
        }

        let last_commit_msg = String::from_utf8_lossy(&output.stdout[..]);
        let last_commit_msg = last_commit_msg.trim();
        if last_commit_msg == CHECKPOINT_REVERT {
            let mut cmd = Command::new("git");
            let status = cmd
                .current_dir(&root)
                .arg("reset")
                .arg("--quiet")
                .arg("--hard")
                .arg("HEAD~1")
                .status()?;
            if !status.success() {
                return Err(anyhow!(
                    "Failed to revert checkpoint commit in {:?}. Command failed (exit code {:?}): {:?}",
                    root.as_ref(),
                    status.code(),
                    cmd
                ));
            }
        } else {
            break;
        }
    }
    Ok(())
}

pub fn git_head_sha<P: AsRef<Path>>(root: P) -> anyhow::Result<String> {
    let mut cmd = Command::new("git");
    let output = cmd
        .current_dir(&root)
        .arg("rev-parse")
        .arg("HEAD")
        .output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "Failed to get the HEAD sha of {:?}. Command failed: {:?}",
            root.as_ref(),
            cmd
        ));
    }
    let head_sha = String::from_utf8_lossy(&output.stdout[..])
        .trim()
        .to_string();
    Ok(head_sha)
}

pub fn git_hard_reset<P: AsRef<Path>>(root: P, initial_commit: &str) -> anyhow::Result<()> {
    let mut cmd = Command::new("git");
    if !cmd
        .current_dir(&root)
        .arg("add")
        .arg(".")
        .status()?
        .success()
    {
        return Err(anyhow!(
            "Failed to run `git add` in {:?}. Command failed: {:?}",
            root.as_ref(),
            cmd
        ));
    }

    let mut cmd = Command::new("git");
    if !cmd
        .current_dir(&root)
        .arg("reset")
        .arg("--quiet")
        .arg("--hard")
        .arg(initial_commit)
        .status()?
        .success()
    {
        return Err(anyhow!(
            "Failed to `git reset` the files of {:?}. Command failed: {:?}",
            root.as_ref(),
            cmd
        ));
    }

    Ok(())
}

pub fn git_remote_head_sha<S: AsRef<str>>(remote: S) -> anyhow::Result<String> {
    let mut cmd = Command::new("git");
    let output = cmd.arg("ls-remote").arg(remote.as_ref()).output()?;
    if !output.status.success() {
        return Err(anyhow!(
            "Failed to query the remote HEAD sha of {}. Command failed: {:?}",
            remote.as_ref(),
            cmd
        ));
    }
    let output = String::from_utf8_lossy(&output.stdout[..])
        .trim()
        .to_string();
    for line in output.lines() {
        let line = line.trim();
        if line.ends_with("HEAD") {
            let mut parts = line.split_whitespace();
            if let Some(head_sha) = parts.next() {
                return Ok(head_sha.to_string());
            }
        }
    }
    Err(anyhow!(
        "Failed to parse HEAD sha of {} from the output of {:?}\nOutput:\n{}",
        remote.as_ref(),
        cmd,
        output
    ))
}
