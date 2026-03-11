use crate::model::{WorktreeInfo, first_non_empty, generate_slug};
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct WorktreeManager {
    repo_root: PathBuf,
    base_branch: String,
    worktrees_dir: PathBuf,
}

impl WorktreeManager {
    pub fn new(repo_root: PathBuf, base_branch: String, worktrees_dir: String) -> Result<Self> {
        let manager = Self {
            worktrees_dir: repo_root.join(worktrees_dir),
            repo_root,
            base_branch,
        };
        manager.verify_git_repo()?;
        fs::create_dir_all(&manager.worktrees_dir)?;
        Ok(manager)
    }

    pub fn create_worktree(&self) -> Result<WorktreeInfo> {
        let base = self.resolve_base_branch()?;
        for _ in 0..20 {
            let slug = generate_slug();
            let branch = format!("agent/{slug}");
            let path = self.worktrees_dir.join(&slug);

            let (ok, out, err) = self.run_git(
                [
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    &path.to_string_lossy(),
                    &base,
                ],
                &self.repo_root,
            )?;

            if ok {
                return Ok(WorktreeInfo {
                    slug,
                    branch,
                    path,
                    base_branch: base.clone(),
                });
            }

            let msg = first_non_empty(&err, &out);
            if !msg.contains("already exists") && !msg.contains("is already checked out") {
                bail!("failed to create worktree: {msg}");
            }
        }
        bail!("failed to create unique worktree after multiple attempts")
    }

    pub fn cleanup_if_safe(&self, info: &WorktreeInfo) -> Result<(bool, String)> {
        if !info.path.exists() {
            return Ok((true, "worktree already removed".to_string()));
        }

        let (ok_status, out_status, err_status) =
            self.run_git(["status", "--porcelain"], &info.path)?;
        if !ok_status {
            return Ok((false, first_non_empty(&err_status, &out_status)));
        }
        if !out_status.trim().is_empty() {
            return Ok((false, "worktree has uncommitted changes".to_string()));
        }

        let (removable, reason) = self.is_branch_merged_or_closed(info)?;
        if !removable {
            return Ok((false, reason));
        }

        let (ok_remove, out_remove, err_remove) = self.run_git(
            ["worktree", "remove", &info.path.to_string_lossy()],
            &self.repo_root,
        )?;
        if !ok_remove {
            return Ok((false, first_non_empty(&err_remove, &out_remove)));
        }

        let _ = self.run_git(["branch", "-d", &info.branch], &self.repo_root)?;
        Ok((true, format!("worktree deleted ({reason})")))
    }

    fn verify_git_repo(&self) -> Result<()> {
        let (ok, out, err) =
            self.run_git(["rev-parse", "--is-inside-work-tree"], &self.repo_root)?;
        if !ok {
            bail!(
                "not a git repository: {} ({})",
                self.repo_root.display(),
                first_non_empty(&err, &out)
            );
        }
        Ok(())
    }

    fn resolve_base_branch(&self) -> Result<String> {
        let (ok, _, _) = self.run_git(
            ["rev-parse", "--verify", &self.base_branch],
            &self.repo_root,
        )?;
        if ok {
            Ok(self.base_branch.clone())
        } else {
            Ok("HEAD".to_string())
        }
    }

    fn is_branch_merged_or_closed(&self, info: &WorktreeInfo) -> Result<(bool, String)> {
        let (ok_merged, out_merged, _) =
            self.run_git(["branch", "--merged", &info.base_branch], &self.repo_root)?;
        if ok_merged {
            let found = out_merged
                .lines()
                .map(|l| l.trim().trim_start_matches("* "))
                .any(|b| b == info.branch);
            if found {
                return Ok((true, "merged".to_string()));
            }
        }

        let (ok_div, out_div, _) = self.run_git(
            [
                "rev-list",
                "--count",
                &format!("{}..{}", info.base_branch, info.branch),
            ],
            &self.repo_root,
        )?;
        if ok_div && out_div.trim() == "0" {
            return Ok((true, "closed".to_string()));
        }

        Ok((false, "not merged".to_string()))
    }

    fn run_git<const N: usize>(
        &self,
        args: [&str; N],
        cwd: &Path,
    ) -> Result<(bool, String, String)> {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("failed to execute git in {}", cwd.display()))?;

        Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        ))
    }
}
