use anyhow::{Result, bail};
use std::collections::HashSet;

// A session a reference may name: a stored session, or a connected peer's edge. `id` is
// a full session id; a peer that announced none matches by name only.
pub struct Candidate<T> {
    pub id: Option<String>,
    pub name: Option<String>,
    pub item: T,
}

// An exact id wins. Otherwise a short id (a uuid's first 8-hex segment) and a name both
// count, and any ambiguity except several sessions sharing one name is an error. Returns
// the matches in input order, several only when they share the name; empty = no match.
pub fn resolve<T>(reference: &str, candidates: Vec<Candidate<T>>) -> Result<Vec<T>> {
    let short = is_short_id(reference);
    let mut seen = HashSet::new();
    let (mut by_short, mut by_name) = (Vec::new(), Vec::new());
    for c in candidates {
        if let Some(id) = &c.id {
            if !seen.insert(id.clone()) {
                continue;
            }
            if id == reference {
                return Ok(vec![c.item]);
            }
        }
        let short_match = short
            && c.id
                .as_deref()
                .and_then(|id| id.strip_prefix(reference))
                .is_some_and(|rest| rest.starts_with('-'));
        if short_match {
            by_short.push(c);
        } else if c.name.as_deref() == Some(reference) {
            by_name.push(c);
        }
    }
    if by_short.len() > 1 || (by_short.len() == 1 && !by_name.is_empty()) {
        let listed: Vec<String> = by_short.iter().chain(&by_name).map(describe).collect();
        bail!(
            "{reference} matches {} sessions: {}; use the full id",
            listed.len(),
            listed.join(", ")
        );
    }
    Ok(by_short
        .into_iter()
        .chain(by_name)
        .map(|c| c.item)
        .collect())
}

fn describe<T>(c: &Candidate<T>) -> String {
    let id = c.id.as_deref().unwrap_or("(no session id)");
    match &c.name {
        Some(name) => format!("{id} ({name})"),
        None => id.to_string(),
    }
}

fn is_short_id(s: &str) -> bool {
    s.len() == 8 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "0e1b8a08-1111-4000-8000-000000000001";
    const B: &str = "0e1b8a08-2222-4000-8000-000000000002";
    const C: &str = "f4b65f05-3333-4000-8000-000000000003";

    fn c(id: &str, name: Option<&str>) -> Candidate<String> {
        Candidate {
            id: Some(id.into()),
            name: name.map(String::from),
            item: id.into(),
        }
    }

    #[test]
    fn a_full_id_beats_everything() {
        let found = resolve(A, vec![c(C, Some(A)), c(A, None)]).unwrap();
        assert_eq!(found, [A]);
    }

    #[test]
    fn a_short_id_is_exactly_the_first_segment() {
        assert_eq!(
            resolve("f4b65f05", vec![c(A, None), c(C, None)]).unwrap(),
            [C]
        );
        for reference in ["f4b65f0", "f4b65f05-3", "f4b65f0g", "4b65f05-"] {
            assert!(
                resolve(reference, vec![c(C, None)]).unwrap().is_empty(),
                "{reference}"
            );
        }
        let not_a_prefix = "65f05333";
        assert!(resolve(not_a_prefix, vec![c(C, None)]).unwrap().is_empty());
    }

    #[test]
    fn a_name_matches_and_same_named_sessions_all_come_back_in_order() {
        let found = resolve("worker", vec![c(C, Some("worker")), c(A, Some("worker"))]);
        assert_eq!(found.unwrap(), [C, A]);
    }

    #[test]
    fn a_short_id_shared_by_two_sessions_fails_listing_both() {
        let err = resolve("0e1b8a08", vec![c(A, Some("worker")), c(B, None)])
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!("0e1b8a08 matches 2 sessions: {A} (worker), {B}; use the full id")
        );
    }

    #[test]
    fn a_short_id_that_is_another_sessions_name_fails() {
        let err = resolve("f4b65f05", vec![c(C, None), c(A, Some("f4b65f05"))]).unwrap_err();
        assert!(err.to_string().contains("matches 2 sessions"), "{err}");
    }

    #[test]
    fn a_session_named_its_own_short_id_is_one_match() {
        let found = resolve(
            "f4b65f05",
            vec![c(C, Some("f4b65f05")), c(C, Some("f4b65f05"))],
        );
        assert_eq!(found.unwrap(), [C]);
    }

    #[test]
    fn nothing_matching_is_empty() {
        assert!(
            resolve("ghost", vec![c(A, Some("worker"))])
                .unwrap()
                .is_empty()
        );
    }
}
