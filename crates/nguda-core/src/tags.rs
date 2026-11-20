//! Tag-name rules (CAP-8, AD-9: tags are a user-owned field of a plan item).
//!
//! Tag names compare case-insensitively everywhere: an item never holds two tags
//! that differ only in case, and a name that matches a known tag takes the known
//! spelling, so memory and the store (which resolves names `NOCASE`) always agree.

/// The case-insensitive comparison key of a tag name.
fn fold(name: &str) -> String {
    name.to_lowercase()
}

/// Normalises a requested tag list against the `known` names: trims each tag, drops
/// empty ones and case-insensitive duplicates (keeping the first), and replaces a
/// tag that matches a known name case-insensitively with the known spelling. Input
/// order is kept.
pub fn normalize_tags(input: &[String], known: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in input {
        let tag = raw.trim();
        if tag.is_empty() {
            continue;
        }
        let key = fold(tag);
        if out.iter().any(|t| fold(t) == key) {
            continue;
        }
        let spelled = known
            .iter()
            .find(|k| fold(k) == key)
            .map_or_else(|| tag.to_owned(), Clone::clone);
        out.push(spelled);
    }
    out
}

/// Adds every name in `new` that `known` does not already hold (case-insensitively)
/// to `known`, then sorts it case-insensitively. Existing spellings win.
pub fn merge_tag_names<'a>(known: &mut Vec<String>, new: impl IntoIterator<Item = &'a String>) {
    for name in new {
        let key = fold(name);
        if !known.iter().any(|k| fold(k) == key) {
            known.push(name.clone());
        }
    }
    sort_tag_names(known);
}

/// Sorts tag names case-insensitively, breaking ties by exact spelling.
pub fn sort_tag_names(names: &mut [String]) {
    names.sort_by(|a, b| fold(a).cmp(&fold(b)).then_with(|| a.cmp(b)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn normalising_trims_drops_empties_and_case_insensitive_duplicates() {
        let input = strings(&[" deep ", "", "Deep", "admin", "  "]);
        assert_eq!(normalize_tags(&input, &[]), strings(&["deep", "admin"]));
    }

    #[test]
    fn a_tag_matching_a_known_name_takes_the_known_spelling() {
        let known = strings(&["Work"]);
        assert_eq!(
            normalize_tags(&strings(&["work"]), &known),
            strings(&["Work"])
        );
        assert_eq!(known, strings(&["Work"]));
    }

    #[test]
    fn normalising_keeps_input_order() {
        let input = strings(&["b", "a", "c"]);
        assert_eq!(normalize_tags(&input, &[]), strings(&["b", "a", "c"]));
    }

    #[test]
    fn merging_adds_only_new_names_and_sorts_case_insensitively() {
        let mut known = strings(&["work", "Admin"]);
        merge_tag_names(&mut known, &strings(&["WORK", "deep", "beta"]));
        assert_eq!(known, strings(&["Admin", "beta", "deep", "work"]));
    }
}
