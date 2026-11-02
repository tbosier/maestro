//! The stash: listing, saving and popping.

use crate::{Access, GitError, Repository, Result};

/// One stash entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
    /// Position in the stash list: `stash@{index}`.
    pub index: usize,
    /// The stash commit's id.
    pub id: String,
    /// The stash's description (`WIP on main: 1234567 subject`).
    pub message: String,
}

pub(crate) fn parse_stashes(out: &[u8]) -> Vec<Stash> {
    String::from_utf8_lossy(out)
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\0');
            let name = f.next()?;
            let id = f.next()?;
            let message = f.next().unwrap_or_default();
            let index = name
                .strip_prefix("stash@{")?
                .strip_suffix('}')?
                .parse()
                .ok()?;
            Some(Stash {
                index,
                id: id.to_string(),
                message: message.to_string(),
            })
        })
        .collect()
}

impl Repository {
    /// The stash list, newest first. The stash is shared by every worktree
    /// of the repository.
    pub fn stashes(&self) -> Result<Vec<Stash>> {
        let out = self.run(
            &["stash", "list", "--no-color", "--format=%gd%x00%H%x00%gs"],
            Access::ReadOnly,
        )?;
        Ok(parse_stashes(&out))
    }

    /// Stashes staged and unstaged changes to tracked files (untracked
    /// files stay), leaving the working tree at `HEAD`. Returns false when
    /// there was nothing to stash.
    pub fn stash_push(&self, message: Option<&str>) -> Result<bool> {
        let status = self.status()?;
        if status.uncommitted().next().is_none() {
            return Ok(false);
        }
        if status.conflicted().next().is_some() {
            return Err(GitError::InvalidArgument(
                "files are in conflict; resolve them before stashing".into(),
            ));
        }
        let before = self.stashes()?.first().map(|s| s.id.clone());
        let mut args = vec!["stash", "push", "--quiet"];
        if let Some(m) = message {
            args.push("--message");
            args.push(m);
        }
        self.run(&args, Access::Write)?;
        let after = self.stashes()?.first().map(|s| s.id.clone());
        Ok(after != before)
    }

    /// Applies stash `index` and drops it, but only if it is still the
    /// stash with id `id` (another program may have changed the list since
    /// it was shown). On a conflict git keeps the stash and reports it.
    pub fn stash_pop(&self, index: usize, id: &str) -> Result<()> {
        let name = format!("stash@{{{index}}}");
        let now = self
            .stashes()?
            .into_iter()
            .find(|s| s.index == index)
            .map(|s| s.id);
        if now.as_deref() != Some(id) {
            return Err(GitError::InvalidArgument(format!(
                "{name} is no longer the stash that was shown; refresh and try again"
            )));
        }
        self.run(&["stash", "pop", "--quiet", &name], Access::Write)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_stash_list() {
        let s = parse_stashes(
            b"stash@{0}\0aaaa\0WIP on main: 1 x\nstash@{12}\0bbbb\0On b: msg\0with nul\ngarbage\n",
        );
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].index, s[0].id.as_str()), (0, "aaaa"));
        assert_eq!(s[0].message, "WIP on main: 1 x");
        assert_eq!(s[1].index, 12);
        assert_eq!(s[1].message, "On b: msg\0with nul");
    }
}
