//! `workspace/slug` repository references, and reading them from git remotes.

use serde::Serialize;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct RepoRef {
    pub workspace: String,
    pub slug: String,
}

impl RepoRef {
    pub fn new(workspace: impl Into<String>, slug: impl Into<String>) -> Self {
        Self {
            workspace: workspace.into(),
            slug: slug.into(),
        }
    }

    /// The repository URL on bitbucket.org.
    pub fn web_url(&self) -> String {
        format!("https://bitbucket.org/{}/{}", self.workspace, self.slug)
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.workspace, self.slug)
    }
}

impl FromStr for RepoRef {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.split_once('/') {
            Some((ws, slug)) if valid_part(ws) && valid_part(slug) => Ok(Self::new(ws, slug)),
            _ => Err(format!("expected WORKSPACE/REPO, got {s:?}")),
        }
    }
}

fn valid_part(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The workspace and slug in a bitbucket.org remote URL, in any of the forms
/// git accepts: `git@bitbucket.org:ws/slug.git`, `ssh://git@bitbucket.org/ws/slug.git`,
/// `https://[user@]bitbucket.org/ws/slug[.git]`. `None` for other hosts.
pub fn parse_remote_url(url: &str) -> Option<RepoRef> {
    let url = url.trim();
    let path = if let Some(rest) = url.strip_prefix("git@bitbucket.org:") {
        rest
    } else {
        let without_scheme = url
            .strip_prefix("ssh://")
            .or_else(|| url.strip_prefix("https://"))
            .or_else(|| url.strip_prefix("http://"))?;
        let host_and_path = without_scheme.rsplit_once('@').map_or(without_scheme, |(_, rest)| rest);
        let (host, path) = host_and_path.split_once('/')?;
        let host = host.split(':').next()?;
        if !host.eq_ignore_ascii_case("bitbucket.org") {
            return None;
        }
        path
    };
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let (ws, slug) = (parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    RepoRef::from_str(&format!("{ws}/{slug}")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_bitbucket_remote_forms() {
        let want = RepoRef::new("lundinova-phoenix", "phoenix_application");
        for url in [
            "git@bitbucket.org:lundinova-phoenix/phoenix_application.git",
            "git@bitbucket.org:lundinova-phoenix/phoenix_application",
            "ssh://git@bitbucket.org/lundinova-phoenix/phoenix_application.git",
            "https://osa@bitbucket.org/lundinova-phoenix/phoenix_application.git",
            "https://bitbucket.org/lundinova-phoenix/phoenix_application/",
            "https://x-token-auth:secret@bitbucket.org/lundinova-phoenix/phoenix_application.git",
        ] {
            assert_eq!(parse_remote_url(url), Some(want.clone()), "{url}");
        }
    }

    #[test]
    fn ignores_other_hosts_and_paths() {
        assert_eq!(parse_remote_url("git@github.com:obirchman/lundinova-ai.git"), None);
        assert_eq!(parse_remote_url("https://github.com/a/b.git"), None);
        assert_eq!(parse_remote_url("https://bitbucket.org/ws/repo/extra"), None);
        assert_eq!(parse_remote_url("https://bitbucket.org.evil.example/ws/repo.git"), None);
    }

    #[test]
    fn repo_ref_from_str() {
        assert_eq!("ws/repo".parse::<RepoRef>().unwrap(), RepoRef::new("ws", "repo"));
        assert!("ws".parse::<RepoRef>().is_err());
        assert!("ws/".parse::<RepoRef>().is_err());
        assert!("a/b/c".parse::<RepoRef>().is_err());
    }
}
