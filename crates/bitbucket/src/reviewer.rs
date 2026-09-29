//! The reviewer tool's marks in pull request comments: the finding key in a
//! finding's footer, and `SCORE=<1-5> Comment=<text>` feedback replies.

use serde::Serialize;

/// One reviewer feedback score, parsed from a reply comment's body.
#[derive(Debug, PartialEq, Serialize)]
pub struct Score {
    pub value: u8,
    pub comment: String,
}

/// Parses a reply of the form `SCORE=<1-5> Comment=<text>` (keys
/// case-insensitive, `reviewer`'s footer writes them upper-case). `None` for
/// anything else, including a `SCORE=` with no `Comment=` after it, or a value
/// outside 1-5.
pub fn parse_score(text: &str) -> Option<Score> {
    let lower = text.to_ascii_lowercase();
    let score_at = lower.find("score=")?;
    let value = text[score_at + "score=".len()..].chars().next()?.to_digit(10)? as u8;
    if !(1..=5).contains(&value) {
        return None;
    }
    let comment_at = lower.find("comment=")?;
    if comment_at <= score_at {
        return None;
    }
    let comment = text[comment_at + "comment=".len()..].trim().to_string();
    Some(Score { value, comment })
}

/// The finding this comment belongs to, if it carries `reviewer`'s key: 12 hex
/// characters inside backticks, at the end of the comment. The older Go tool
/// wrote the same shape, so its comments are recognised too.
pub fn footer_key(text: &str) -> Option<&str> {
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('`') else { return None };
        let candidate = &rest[..end];
        if candidate.len() == 12 && candidate.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(candidate);
        }
        rest = &rest[end + 1..];
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_score() {
        let score = parse_score("SCORE=4 Comment=caught a real off-by-one").unwrap();
        assert_eq!(score, Score { value: 4, comment: "caught a real off-by-one".into() });
    }

    #[test]
    fn parses_lower_case_keys() {
        let score = parse_score("score=1 comment=noise").unwrap();
        assert_eq!(score, Score { value: 1, comment: "noise".into() });
    }

    #[test]
    fn rejects_a_score_outside_one_to_five() {
        assert!(parse_score("SCORE=9 Comment=x").is_none());
        assert!(parse_score("SCORE=0 Comment=x").is_none());
    }

    #[test]
    fn rejects_a_score_with_no_comment() {
        assert!(parse_score("SCORE=5").is_none());
        assert!(parse_score("just a normal reply").is_none());
    }

    #[test]
    fn finds_the_footer_key_among_other_backtick_text() {
        assert_eq!(footer_key("see `foo()` · `a1b2c3d4e5f6`"), Some("a1b2c3d4e5f6"));
        assert_eq!(footer_key("no key here"), None);
    }
}
