//! An independent segment-tree matcher for Axum's routing interface.
//!
//! Static segments take precedence over captures; terminal catch-all captures
//! match a nonempty remainder. Parameters borrow the registered names and the
//! request path. No upstream matcher implementation or Unicode data is included.
#![forbid(unsafe_code)]

use std::{collections::BTreeMap, error::Error, fmt};

#[derive(Clone, Debug)]
pub struct Router<T> {
    root: Node<T>,
}

#[derive(Clone, Debug)]
struct Node<T> {
    endpoint: Option<Endpoint<T>>,
    literals: BTreeMap<String, Node<T>>,
    captures: BTreeMap<(String, String), Node<T>>,
    remainder: Option<Box<Node<T>>>,
}

#[derive(Clone, Debug)]
struct Endpoint<T> {
    value: T,
    route: String,
    names: Vec<String>,
}

impl<T> Default for Node<T> {
    fn default() -> Self {
        Self {
            endpoint: None,
            literals: BTreeMap::new(),
            captures: BTreeMap::new(),
            remainder: None,
        }
    }
}

impl<T> Default for Router<T> {
    fn default() -> Self {
        Self {
            root: Node::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InsertError {
    Conflict { with: String },
    InvalidParam,
    InvalidParamSegment,
    InvalidCatchAll,
}

impl fmt::Display for InsertError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict { with } => write!(formatter, "route conflicts with {with:?}"),
            Self::InvalidParam => formatter.write_str("invalid parameter name or braces"),
            Self::InvalidParamSegment => {
                formatter.write_str("one parameter is allowed per segment")
            }
            Self::InvalidCatchAll => formatter.write_str("catch-all must occupy the last segment"),
        }
    }
}

impl Error for InsertError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchError {
    NotFound,
}

impl fmt::Display for MatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("no matching route")
    }
}

impl Error for MatchError {}

#[derive(Clone, Debug, Default)]
pub struct Params<'names, 'path> {
    entries: Vec<(&'names str, &'path str)>,
}

impl<'names, 'path> Params<'names, 'path> {
    pub fn iter(&self) -> impl Iterator<Item = (&'names str, &'path str)> + '_ {
        self.entries.iter().copied()
    }

    pub fn get(&self, name: &str) -> Option<&'path str> {
        self.iter()
            .find_map(|(key, value)| (key == name).then_some(value))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Debug)]
pub struct Match<'names, 'path, T> {
    pub value: T,
    pub params: Params<'names, 'path>,
}

#[derive(Debug)]
enum Segment {
    Literal(String),
    Capture {
        prefix: String,
        name: String,
        suffix: String,
    },
    Remainder(String),
}

fn parse_segment(raw: &str, last: bool) -> Result<Segment, InsertError> {
    let mut literal = String::new();
    let mut capture = None;
    let mut characters = raw.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '{' if characters.peek() == Some(&'{') => {
                characters.next();
                literal.push('{');
            }
            '}' if characters.peek() == Some(&'}') => {
                characters.next();
                literal.push('}');
            }
            '{' => {
                if capture.is_some() {
                    return Err(InsertError::InvalidParamSegment);
                }
                let mut name = String::new();
                let mut closed = false;
                for character in characters.by_ref() {
                    if character == '}' {
                        closed = true;
                        break;
                    }
                    if character == '{' {
                        return Err(InsertError::InvalidParam);
                    }
                    name.push(character);
                }
                if !closed || name.is_empty() {
                    return Err(InsertError::InvalidParam);
                }
                let prefix = std::mem::take(&mut literal);
                capture = Some((prefix, name));
            }
            '}' => return Err(InsertError::InvalidParam),
            character => literal.push(character),
        }
    }
    match capture {
        None => Ok(Segment::Literal(literal)),
        Some((prefix, name)) if name.starts_with('*') => {
            if !last || !prefix.is_empty() || !literal.is_empty() || name.len() == 1 {
                return Err(InsertError::InvalidCatchAll);
            }
            Ok(Segment::Remainder(name[1..].to_owned()))
        }
        Some((prefix, name)) => Ok(Segment::Capture {
            prefix,
            name,
            suffix: literal,
        }),
    }
}

impl<T> Router<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, route: impl Into<String>, value: T) -> Result<(), InsertError> {
        let route = route.into();
        let raw: Vec<_> = route.split('/').collect();
        let segments = raw
            .iter()
            .enumerate()
            .map(|(index, segment)| parse_segment(segment, index + 1 == raw.len()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut names = Vec::new();
        let mut node = &mut self.root;
        for segment in segments {
            node = match segment {
                Segment::Literal(literal) => node.literals.entry(literal).or_default(),
                Segment::Capture {
                    prefix,
                    name,
                    suffix,
                } => {
                    if names.contains(&name) {
                        return Err(InsertError::InvalidParam);
                    }
                    names.push(name);
                    node.captures.entry((prefix, suffix)).or_default()
                }
                Segment::Remainder(name) => {
                    if names.contains(&name) {
                        return Err(InsertError::InvalidParam);
                    }
                    names.push(name);
                    node.remainder
                        .get_or_insert_with(|| Box::new(Node::default()))
                }
            };
        }
        if let Some(endpoint) = &node.endpoint {
            return Err(InsertError::Conflict {
                with: endpoint.route.clone(),
            });
        }
        node.endpoint = Some(Endpoint {
            value,
            route,
            names,
        });
        Ok(())
    }

    pub fn at<'names, 'path>(
        &'names self,
        path: &'path str,
    ) -> Result<Match<'names, 'path, &'names T>, MatchError> {
        let mut captures = Vec::new();
        let endpoint = self
            .root
            .find(path, &mut captures)
            .ok_or(MatchError::NotFound)?;
        let entries = endpoint
            .names
            .iter()
            .map(String::as_str)
            .zip(captures)
            .collect();
        Ok(Match {
            value: &endpoint.value,
            params: Params { entries },
        })
    }
}

impl<T> Node<T> {
    fn find<'names, 'path>(
        &'names self,
        path: &'path str,
        captures: &mut Vec<&'path str>,
    ) -> Option<&'names Endpoint<T>> {
        let (segment, remainder) = match path.split_once('/') {
            Some((segment, remainder)) => (segment, Some(remainder)),
            None => (path, None),
        };
        if let Some(literal) = self.literals.get(segment)
            && let Some(endpoint) = literal.finish(remainder, captures)
        {
            return Some(endpoint);
        }
        // Affixes are tried before a plain capture, in decreasing specificity.
        let mut branches: Vec<_> = self.captures.iter().collect();
        branches.sort_by(
            |((left_prefix, left_suffix), _), ((right_prefix, right_suffix), _)| {
                (right_prefix.len(), right_suffix.len())
                    .cmp(&(left_prefix.len(), left_suffix.len()))
            },
        );
        for ((prefix, suffix), child) in branches {
            let Some(value) = segment
                .strip_prefix(prefix)
                .and_then(|v| v.strip_suffix(suffix))
            else {
                continue;
            };
            if value.is_empty() {
                continue;
            }
            captures.push(value);
            if let Some(endpoint) = child.finish(remainder, captures) {
                return Some(endpoint);
            }
            captures.pop();
        }
        if !path.is_empty()
            && let Some(endpoint) = self
                .remainder
                .as_ref()
                .and_then(|child| child.endpoint.as_ref())
        {
            captures.push(path);
            return Some(endpoint);
        }
        None
    }

    fn finish<'names, 'path>(
        &'names self,
        remainder: Option<&'path str>,
        captures: &mut Vec<&'path str>,
    ) -> Option<&'names Endpoint<T>> {
        match remainder {
            Some(path) => self.find(path, captures),
            None => self.endpoint.as_ref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_routes_win_and_incomplete_static_branches_backtrack() {
        let mut router = Router::new();
        router.insert("/Items/Latest", 1).unwrap();
        router.insert("/Items/{id}/File", 2).unwrap();
        router.insert("/Items/Latest/Images", 3).unwrap();
        assert_eq!(*router.at("/Items/Latest").unwrap().value, 1);
        let matched = router.at("/Items/Latest/File").unwrap();
        assert_eq!(*matched.value, 2);
        assert_eq!(matched.params.get("id"), Some("Latest"));
        assert!(router.at("/Items/Latest/").is_err());
        assert!(router.at("/Items//File").is_err());
    }

    #[test]
    fn route_names_are_retained_at_the_leaf() {
        let mut router = Router::new();
        router.insert("/Items/{item_id}/Images/{kind}", 1).unwrap();
        router.insert("/Items/{id}/File", 2).unwrap();
        let matched = router.at("/Items/caf%C3%A9/Images/Primary").unwrap();
        assert_eq!(
            matched.params.iter().collect::<Vec<_>>(),
            vec![("item_id", "caf%C3%A9"), ("kind", "Primary")]
        );
        assert_eq!(
            router.at("/Items/123/File").unwrap().params.get("id"),
            Some("123")
        );
    }

    #[test]
    fn catch_all_is_nonempty_and_static_paths_win() {
        let mut router = Router::new();
        router.insert("/web/{*rest}", 1).unwrap();
        router.insert("/web/index.html", 2).unwrap();
        router.insert("/web/", 3).unwrap();
        assert_eq!(*router.at("/web/index.html").unwrap().value, 2);
        assert_eq!(*router.at("/web/").unwrap().value, 3);
        assert_eq!(
            router.at("/web/vendor/a.js").unwrap().params.get("rest"),
            Some("vendor/a.js")
        );
        assert!(router.at("/web").is_err());
        assert!(router.insert("/a/{*rest}/b", 4).is_err());
        assert!(router.insert("/a/x{*rest}", 4).is_err());
    }

    #[test]
    fn affixes_and_literal_braces_are_supported() {
        let mut router = Router::new();
        router.insert("/sub/{index}.vtt", 1).unwrap();
        router.insert("/sub/{index}", 2).unwrap();
        router.insert("/{{literal}}", 3).unwrap();
        assert_eq!(
            router.at("/sub/12.vtt").unwrap().params.get("index"),
            Some("12")
        );
        assert_eq!(*router.at("/sub/12.vtt").unwrap().value, 1);
        assert_eq!(*router.at("/{literal}").unwrap().value, 3);
    }

    #[test]
    fn duplicate_and_invalid_patterns_do_not_replace_routes() {
        let mut router = Router::new();
        router.insert("/a/{id}", 1).unwrap();
        assert!(matches!(
            router.insert("/a/{other}", 2),
            Err(InsertError::Conflict { .. })
        ));
        for pattern in [
            "/a/{",
            "/a/}",
            "/a/{}",
            "/a/{id}/{id}",
            "/a/{x}{y}",
            "/a/{*}",
        ] {
            assert!(router.insert(pattern, 2).is_err(), "{pattern}");
        }
        assert_eq!(*router.at("/a/123").unwrap().value, 1);
        assert_eq!(*router.clone().at("/a/123").unwrap().value, 1);
    }
}
