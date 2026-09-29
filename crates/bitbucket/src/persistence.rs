//! Handled marks: per pull request, when it was last marked handled and the
//! snapshot taken then, plus whether a notification has gone out since.

use db::{
    query,
    sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};

pub struct BitbucketDb(ThreadSafeConnection);

impl Domain for BitbucketDb {
    const NAME: &str = stringify!(BitbucketDb);
    const MIGRATIONS: &[&str] = &[sql!(
        CREATE TABLE IF NOT EXISTS pr_handled(
            workspace TEXT NOT NULL,
            slug TEXT NOT NULL,
            pr_id INTEGER NOT NULL,
            handled_at INTEGER NOT NULL,
            snapshot TEXT NOT NULL,
            notified INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(workspace, slug, pr_id)
        ) STRICT;
    )];
}

db::static_connection!(BitbucketDb, []);

/// One row of `pr_handled`: workspace, slug, pull request id, unix time
/// handled, snapshot JSON, notified since.
pub type HandledRow = (String, String, i64, i64, String, bool);

impl BitbucketDb {
    query! {
        pub fn all_handled() -> Result<Vec<HandledRow>> {
            SELECT workspace, slug, pr_id, handled_at, snapshot, notified FROM pr_handled
        }
    }

    query! {
        pub async fn save_handled(workspace: String, slug: String, pr_id: i64, handled_at: i64, snapshot: String) -> Result<()> {
            INSERT INTO pr_handled(workspace, slug, pr_id, handled_at, snapshot, notified)
            VALUES (?1, ?2, ?3, ?4, ?5, 0)
            ON CONFLICT(workspace, slug, pr_id) DO UPDATE SET
                handled_at = excluded.handled_at,
                snapshot = excluded.snapshot,
                notified = 0
        }
    }

    query! {
        pub async fn set_notified(workspace: String, slug: String, pr_id: i64) -> Result<()> {
            UPDATE pr_handled SET notified = 1 WHERE workspace = ?1 AND slug = ?2 AND pr_id = ?3
        }
    }
}
