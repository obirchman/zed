use settings::{RegisterSetting, Settings};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, RegisterSetting)]
pub struct BitbucketSettings {
    pub workspaces: Vec<String>,
    pub poll_interval: Duration,
    pub full_scan_interval: Duration,
    pub notifications: bool,
    pub clone_roots: Vec<PathBuf>,
}

impl Settings for BitbucketSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let bitbucket = content.bitbucket.as_ref().unwrap();
        Self {
            workspaces: bitbucket.workspaces.clone().unwrap(),
            // Bitbucket allows about 1,000 requests an hour; a cycle costs a few dozen.
            poll_interval: Duration::from_secs(bitbucket.poll_interval_seconds.unwrap().max(30)),
            full_scan_interval: Duration::from_secs(bitbucket.full_scan_minutes.unwrap().max(10) * 60),
            notifications: bitbucket.notifications.unwrap(),
            clone_roots: bitbucket
                .clone_roots
                .clone()
                .unwrap()
                .iter()
                .map(|root| match root.strip_prefix("~/") {
                    Some(rest) => paths::home_dir().join(rest),
                    None => PathBuf::from(root),
                })
                .collect(),
        }
    }
}
