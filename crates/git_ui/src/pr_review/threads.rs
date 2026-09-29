//! Pull request comments grouped into threads, and where each thread belongs.

use bitbucket::reviewer as pulls;
use bitbucket::models::Comment;
use collections::HashMap;

/// Where a thread's root comment is anchored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// A line in the new version of a file, 1-based.
    NewLine { path: String, line: u32 },
    /// A line in the old version of a file (a removed line), 1-based.
    OldLine { path: String, line: u32 },
    /// A comment on a file with no line.
    File { path: String },
    /// A comment on the whole pull request.
    General,
}

#[derive(Clone, Debug)]
pub struct Thread {
    pub root: Comment,
    /// Replies at any depth, oldest first.
    pub replies: Vec<Comment>,
    pub place: Place,
    pub outdated: bool,
}

impl Thread {
    pub fn is_resolved(&self) -> bool {
        self.root.resolution.is_some()
    }

    /// The reviewer tool's finding key, when the root comment carries one.
    pub fn finding_key(&self) -> Option<&str> {
        pulls::footer_key(&self.root.content.raw)
    }

    /// Scores already given to this finding as `SCORE=<n>` replies.
    pub fn scores(&self) -> Vec<u8> {
        self.replies
            .iter()
            .filter_map(|c| pulls::parse_score(&c.content.raw))
            .map(|s| s.value)
            .collect()
    }
}

/// Group `comments` into threads, oldest root first. Deleted comments are left
/// out, except a deleted root that still has live replies, which keeps its
/// place so the replies stay in context.
pub fn group(comments: &[Comment]) -> Vec<Thread> {
    let by_id: HashMap<u64, &Comment> = comments.iter().map(|c| (c.id, c)).collect();

    let mut replies: HashMap<u64, Vec<Comment>> = HashMap::default();
    for c in comments.iter().filter(|c| c.parent.is_some() && !c.deleted) {
        replies.entry(root_of(&by_id, c)).or_default().push(c.clone());
    }

    let mut threads: Vec<Thread> = comments
        .iter()
        .filter(|c| c.parent.is_none())
        .filter_map(|root| {
            let mut replies = replies.remove(&root.id).unwrap_or_default();
            if root.deleted && replies.is_empty() {
                return None;
            }
            replies.sort_by(|a, b| a.created_on.cmp(&b.created_on));
            let (place, outdated) = place_of(root);
            Some(Thread {
                root: root.clone(),
                replies,
                place,
                outdated,
            })
        })
        .collect();
    threads.sort_by(|a, b| a.root.created_on.cmp(&b.root.created_on));
    threads
}

/// The id of the comment at the top of `c`'s reply chain. Bitbucket nests
/// replies arbitrarily deep.
fn root_of<'a>(by_id: &HashMap<u64, &'a Comment>, mut c: &'a Comment) -> u64 {
    let mut hops = 0;
    while let Some(parent) = c.parent.as_ref().and_then(|p| by_id.get(&p.id).copied()) {
        c = parent;
        hops += 1;
        if hops > 1000 {
            break;
        }
    }
    c.id
}

fn place_of(c: &Comment) -> (Place, bool) {
    let Some(inline) = &c.inline else {
        return (Place::General, false);
    };
    let path = inline.path.clone();
    let place = match (inline.to, inline.from) {
        (Some(line), _) => Place::NewLine {
            path,
            line: line as u32,
        },
        (None, Some(line)) => Place::OldLine {
            path,
            line: line as u32,
        },
        (None, None) => Place::File { path },
    };
    (place, inline.outdated)
}

/// The first line of a reviewer finding's body, without Markdown emphasis:
/// `**major** · logic · Off-by-one` becomes `major · logic · Off-by-one`.
pub fn finding_headline(raw: &str) -> String {
    raw.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or_default()
        .replace("**", "")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitbucket::models::{Inline, ParentRef, Rendered};

    fn comment(id: u64, parent: Option<u64>, inline: Option<Inline>, created: &str) -> Comment {
        Comment {
            id,
            content: Rendered {
                raw: format!("c{id}"),
            },
            parent: parent.map(|id| ParentRef { id }),
            inline,
            created_on: Some(created.into()),
            ..Default::default()
        }
    }

    fn at(path: &str, to: Option<u64>, from: Option<u64>) -> Option<Inline> {
        Some(Inline {
            path: path.into(),
            to,
            from,
            outdated: false,
        })
    }

    #[test]
    fn nests_replies_of_replies_under_their_root() {
        let comments = vec![
            comment(1, None, at("a.c", Some(10), None), "1"),
            comment(2, Some(1), None, "2"),
            comment(3, Some(2), None, "3"),
            comment(4, None, None, "4"),
        ];
        let threads = group(&comments);
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].replies.iter().map(|c| c.id).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(threads[0].place, Place::NewLine { path: "a.c".into(), line: 10 });
        assert_eq!(threads[1].place, Place::General);
    }

    #[test]
    fn old_side_and_file_comments() {
        let comments = vec![
            comment(1, None, at("a.c", None, Some(3)), "1"),
            comment(2, None, at("b.c", None, None), "2"),
        ];
        let threads = group(&comments);
        assert_eq!(threads[0].place, Place::OldLine { path: "a.c".into(), line: 3 });
        assert_eq!(threads[1].place, Place::File { path: "b.c".into() });
    }

    #[test]
    fn drops_deleted_comments_but_keeps_a_deleted_root_with_live_replies() {
        let mut dead_root = comment(1, None, None, "1");
        dead_root.deleted = true;
        let mut dead_reply = comment(3, Some(1), None, "3");
        dead_reply.deleted = true;
        let mut lonely_dead_root = comment(4, None, None, "4");
        lonely_dead_root.deleted = true;
        let comments = vec![dead_root, comment(2, Some(1), None, "2"), dead_reply, lonely_dead_root];
        let threads = group(&comments);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].replies.iter().map(|c| c.id).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn reviewer_finding_key_and_scores() {
        let mut root = comment(1, None, at("a.c", Some(1), None), "1");
        root.content.raw = "**major** · logic · Off by one\n\nbody\n\n`0123456789ab`".into();
        let mut reply = comment(2, Some(1), None, "2");
        reply.content.raw = "SCORE=4 Comment=real bug".into();
        let threads = group(&[root, reply]);
        assert_eq!(threads[0].finding_key(), Some("0123456789ab"));
        assert_eq!(threads[0].scores(), vec![4]);
        assert_eq!(finding_headline(&threads[0].root.content.raw), "major · logic · Off by one");
    }
}
