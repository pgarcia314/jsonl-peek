//! Field paths for pulling a value out of a parsed JSON document.
//!
//! A path is a sequence of segments: a bare or dotted key (`meta.source`), a
//! bracketed index (`messages[0]`, `messages[-1]` for "from the end"), or an
//! empty bracket meaning "every element" (`messages[].role`). A path can also
//! start with an index, for documents whose top level is itself an array
//! (`[0].id`).
//!
//! ```
//! use jsonl_peek::{json, FieldPath};
//!
//! let doc = json::parse(r#"{"messages":[{"role":"user"},{"role":"assistant"}]}"#).unwrap();
//! let path = FieldPath::parse("messages[].role").unwrap();
//! let roles: Vec<&str> = path.select(&doc).into_iter().filter_map(json::Value::as_str).collect();
//! assert_eq!(roles, vec!["user", "assistant"]);
//! ```

use std::fmt;

use crate::json::Value;

/// One step of a [`FieldPath`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// An object member, by name.
    Key(String),
    /// An array element. Negative indices count from the end, `-1` being the
    /// last element.
    Index(i64),
    /// Every element of an array.
    Wildcard,
}

/// What went wrong while parsing a field path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathErrorKind {
    /// The path was the empty string.
    EmptyPath,
    /// A `.` or the start of the path was followed by another separator.
    EmptyKey,
    /// A `[` was never closed.
    UnterminatedIndex,
    /// The text between `[` and `]` was not a valid integer.
    InvalidIndex,
    /// A character that cannot appear outside a key, such as a stray `]`.
    UnexpectedChar(char),
}

impl fmt::Display for PathErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathErrorKind::EmptyPath => f.write_str("empty field path"),
            PathErrorKind::EmptyKey => f.write_str("empty key segment"),
            PathErrorKind::UnterminatedIndex => f.write_str("unterminated '[' index"),
            PathErrorKind::InvalidIndex => f.write_str("invalid index inside '[...]'"),
            PathErrorKind::UnexpectedChar(c) => write!(f, "unexpected '{}'", c),
        }
    }
}

/// A parse failure together with the byte offset where it was detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathError {
    /// Zero based byte offset into the parsed path string.
    pub offset: usize,
    /// What went wrong.
    pub kind: PathErrorKind,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "at byte {}: {}", self.offset, self.kind)
    }
}

impl std::error::Error for PathError {}

/// A parsed field path, ready to be matched against many documents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldPath {
    raw: String,
    segments: Vec<Segment>,
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl FieldPath {
    /// Parses a path such as `messages[].role` or `[0].id`.
    pub fn parse(input: &str) -> Result<FieldPath, PathError> {
        if input.is_empty() {
            return Err(PathError {
                offset: 0,
                kind: PathErrorKind::EmptyPath,
            });
        }

        let bytes = input.as_bytes();
        let mut segments = Vec::new();
        let mut i = 0;

        while i < bytes.len() {
            match bytes[i] {
                b'[' => {
                    let bracket_at = i;
                    i += 1;
                    let value_start = i;
                    let relative_close = bytes[value_start..].iter().position(|&b| b == b']');
                    let close = relative_close.map(|p| value_start + p).ok_or_else(|| PathError {
                        offset: bracket_at,
                        kind: PathErrorKind::UnterminatedIndex,
                    })?;
                    let inner = &input[value_start..close];
                    i = close + 1; // past ']'
                    if inner.is_empty() {
                        segments.push(Segment::Wildcard);
                    } else {
                        let index = inner.parse::<i64>().map_err(|_| PathError {
                            offset: value_start,
                            kind: PathErrorKind::InvalidIndex,
                        })?;
                        segments.push(Segment::Index(index));
                    }
                }
                b'.' => {
                    if i == 0 {
                        return Err(PathError {
                            offset: 0,
                            kind: PathErrorKind::EmptyKey,
                        });
                    }
                    i += 1;
                    segments.push(Self::parse_key(input, bytes, &mut i)?);
                }
                b']' => {
                    return Err(PathError {
                        offset: i,
                        kind: PathErrorKind::UnexpectedChar(']'),
                    });
                }
                _ => segments.push(Self::parse_key(input, bytes, &mut i)?),
            }
        }

        Ok(FieldPath {
            raw: input.to_string(),
            segments,
        })
    }

    fn parse_key(input: &str, bytes: &[u8], i: &mut usize) -> Result<Segment, PathError> {
        let start = *i;
        while bytes.get(*i).is_some_and(|&b| !matches!(b, b'.' | b'[' | b']')) {
            *i += 1;
        }
        if *i == start {
            return Err(PathError {
                offset: start,
                kind: PathErrorKind::EmptyKey,
            });
        }
        Ok(Segment::Key(input[start..*i].to_string()))
    }

    /// The path exactly as it was given to [`parse`](FieldPath::parse).
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// The parsed steps, in order.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Every value this path selects out of `document`.
    ///
    /// A wildcard or an index into a non-array yields nothing rather than an
    /// error, since "the field is absent from this record" is the normal,
    /// expected case when profiling a dataset.
    pub fn select<'a>(&self, document: &'a Value) -> Vec<&'a Value> {
        let mut current = vec![document];
        for segment in &self.segments {
            let mut next = Vec::with_capacity(current.len());
            for value in current {
                match segment {
                    Segment::Key(name) => {
                        if let Some(found) = value.get(name) {
                            next.push(found);
                        }
                    }
                    Segment::Index(index) => {
                        if let Some(items) = value.as_array() {
                            if let Some(item) = resolve_index(items, *index) {
                                next.push(item);
                            }
                        }
                    }
                    Segment::Wildcard => {
                        if let Some(items) = value.as_array() {
                            next.extend(items.iter());
                        }
                    }
                }
            }
            current = next;
        }
        current
    }
}

/// Resolves a (possibly negative) index against a slice, the way `[-1]`
/// means the last element.
fn resolve_index(items: &[Value], index: i64) -> Option<&Value> {
    let position = if index >= 0 {
        index as usize
    } else {
        let from_end = index.unsigned_abs() as usize;
        items.len().checked_sub(from_end)?
    };
    items.get(position)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json;

    fn path(s: &str) -> FieldPath {
        FieldPath::parse(s).unwrap()
    }

    fn err(s: &str) -> PathError {
        FieldPath::parse(s).unwrap_err()
    }

    #[test]
    fn parses_a_bare_key() {
        assert_eq!(path("role").segments(), &[Segment::Key("role".into())]);
    }

    #[test]
    fn parses_a_dotted_key() {
        assert_eq!(
            path("meta.source").segments(),
            &[Segment::Key("meta".into()), Segment::Key("source".into())]
        );
    }

    #[test]
    fn parses_an_index() {
        assert_eq!(
            path("messages[0].content").segments(),
            &[
                Segment::Key("messages".into()),
                Segment::Index(0),
                Segment::Key("content".into())
            ]
        );
    }

    #[test]
    fn parses_a_negative_index() {
        assert_eq!(
            path("messages[-1].content").segments(),
            &[
                Segment::Key("messages".into()),
                Segment::Index(-1),
                Segment::Key("content".into())
            ]
        );
    }

    #[test]
    fn parses_a_wildcard() {
        assert_eq!(
            path("messages[].role").segments(),
            &[Segment::Key("messages".into()), Segment::Wildcard, Segment::Key("role".into())]
        );
    }

    #[test]
    fn parses_a_leading_index() {
        assert_eq!(
            path("[0].id").segments(),
            &[Segment::Index(0), Segment::Key("id".into())]
        );
    }

    #[test]
    fn round_trips_through_display() {
        assert_eq!(path("messages[-1].content").to_string(), "messages[-1].content");
    }

    #[test]
    fn rejects_the_empty_path() {
        assert_eq!(err("").kind, PathErrorKind::EmptyPath);
    }

    #[test]
    fn rejects_empty_key_segments() {
        assert_eq!(err("a.").kind, PathErrorKind::EmptyKey);
        assert_eq!(err("a..b").kind, PathErrorKind::EmptyKey);
        assert_eq!(err(".a").kind, PathErrorKind::EmptyKey);
    }

    #[test]
    fn rejects_broken_brackets() {
        assert_eq!(err("a[0").kind, PathErrorKind::UnterminatedIndex);
        assert_eq!(err("a[x]").kind, PathErrorKind::InvalidIndex);
        assert_eq!(err("a[-]").kind, PathErrorKind::InvalidIndex);
        assert_eq!(err("a]").kind, PathErrorKind::UnexpectedChar(']'));
    }

    fn doc(json: &str) -> json::Value {
        json::parse(json).unwrap()
    }

    #[test]
    fn selects_a_top_level_key() {
        let v = doc(r#"{"role":"user"}"#);
        assert_eq!(path("role").select(&v), vec![&json::Value::Str("user".into())]);
    }

    #[test]
    fn selects_a_nested_key() {
        let v = doc(r#"{"meta":{"source":"web"}}"#);
        assert_eq!(
            path("meta.source").select(&v),
            vec![&json::Value::Str("web".into())]
        );
    }

    #[test]
    fn missing_keys_select_nothing() {
        let v = doc(r#"{"meta":{}}"#);
        assert!(path("meta.source").select(&v).is_empty());
        assert!(path("absent").select(&v).is_empty());
    }

    #[test]
    fn selects_an_array_element_by_index() {
        let v = doc(r#"{"messages":[{"role":"user"},{"role":"assistant"}]}"#);
        assert_eq!(
            path("messages[0].role").select(&v),
            vec![&json::Value::Str("user".into())]
        );
        assert_eq!(
            path("messages[-1].role").select(&v),
            vec![&json::Value::Str("assistant".into())]
        );
    }

    #[test]
    fn out_of_range_indices_select_nothing() {
        let v = doc(r#"{"messages":[{"role":"user"}]}"#);
        assert!(path("messages[5].role").select(&v).is_empty());
        assert!(path("messages[-5].role").select(&v).is_empty());
    }

    #[test]
    fn wildcard_selects_every_element() {
        let v = doc(r#"{"messages":[{"role":"user"},{"role":"assistant"},{"role":"system"}]}"#);
        let roles: Vec<&str> = path("messages[].role")
            .select(&v)
            .into_iter()
            .filter_map(json::Value::as_str)
            .collect();
        assert_eq!(roles, vec!["user", "assistant", "system"]);
    }

    #[test]
    fn indexing_a_non_array_selects_nothing() {
        let v = doc(r#"{"messages":"not an array"}"#);
        assert!(path("messages[0]").select(&v).is_empty());
        assert!(path("messages[]").select(&v).is_empty());
    }

    #[test]
    fn a_leading_index_reads_a_top_level_array() {
        let v = doc(r#"[{"id":1},{"id":2}]"#);
        assert_eq!(path("[1].id").select(&v), vec![&json::Value::Int(2)]);
    }
}
