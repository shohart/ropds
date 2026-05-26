pub mod authors;
pub mod books;
pub mod bookshelf;
pub mod catalogs;
pub mod counters;
pub mod genres;
pub mod oauth;
pub mod reading_positions;
pub mod series;
pub mod suppressed;
pub mod users;

/// How the alphabet drill-down matches the typed prefix against entity names.
///
/// Drives both the SQL `LIKE` clauses (count / listing queries) and the
/// in-memory aggregation that turns matching rows into letter-group counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixMode {
    /// Match the prefix at any word boundary in the name — the default. Picks
    /// up rows like "Hakim Abdul Efendi" under the "AB" group via the inner
    /// word "Abdul".
    WordBoundary,
    /// Match the prefix only at the very start of the name. "AB" would NOT
    /// surface "Hakim Abdul Efendi"; it surfaces only rows beginning with
    /// "AB".
    FirstWord,
}

impl PrefixMode {
    /// Resolve a `PrefixMode` from the boolean config knob
    /// `opds.alphabet_first_word_only`.
    pub fn from_first_word_only(first_word_only: bool) -> Self {
        if first_word_only {
            Self::FirstWord
        } else {
            Self::WordBoundary
        }
    }
}

/// SQL `LIKE` fragment for an alphabet-drill-down filter plus the ordered
/// bind values it expects.
///
/// `clause` is meant to be spliced into a larger statement, e.g.
/// `format!("... AND {clause} ORDER BY ...")`. Each `?` in `clause`
/// consumes the next value from `binds`, left to right.
#[derive(Debug, Clone)]
pub struct PrefixFilter {
    pub clause: String,
    pub binds: Vec<String>,
}

/// Match `prefix` at any word boundary inside `column`: either at the very
/// start, or immediately after a space. Two `LIKE` placeholders, two binds.
pub fn any_word_search(column: &str, prefix: &str) -> PrefixFilter {
    PrefixFilter {
        clause: format!("({column} LIKE ? OR {column} LIKE ?)"),
        binds: vec![format!("{prefix}%"), format!("% {prefix}%")],
    }
}

/// Match `prefix` only at the start of `column` (i.e., only the FIRST word
/// of the name can satisfy the prefix). One `LIKE` placeholder, one bind.
pub fn first_word_search(column: &str, prefix: &str) -> PrefixFilter {
    PrefixFilter {
        clause: format!("{column} LIKE ?"),
        binds: vec![format!("{prefix}%")],
    }
}

/// Dispatch to [`any_word_search`] or [`first_word_search`] per `mode`.
pub fn prefix_search(column: &str, prefix: &str, mode: PrefixMode) -> PrefixFilter {
    match mode {
        PrefixMode::WordBoundary => any_word_search(column, prefix),
        PrefixMode::FirstWord => first_word_search(column, prefix),
    }
}

// ── Symmetric SQL builders for lang + prefix queries ─────────────────
//
// authors and series share the same schema shape (a `lang_code` column +
// a single uppercased "search" column). The three builders below cover
// the three query flavors used by the alphabet drill-down, parameterised
// by `table` and `search_col`. The leading `(? = 0 OR lang_code = ?)`
// always expects two `lang_code` binds before the prefix filter's binds.

/// Paginated entity listing — `SELECT *` ordered by `search_col`.
///
/// Bind order: `lang_code`, `lang_code`, prefix filter binds, `limit`,
/// `offset`.
pub fn build_lang_prefix_listing_sql(table: &str, search_col: &str, pf: &PrefixFilter) -> String {
    format!(
        "SELECT * FROM {table} WHERE (? = 0 OR lang_code = ?) \
         AND {clause} ORDER BY {search_col} LIMIT ? OFFSET ?",
        clause = pf.clause
    )
}

/// Count of entities matching the lang + prefix filter.
///
/// Bind order: `lang_code`, `lang_code`, prefix filter binds.
pub fn build_lang_prefix_count_sql(table: &str, pf: &PrefixFilter) -> String {
    format!(
        "SELECT COUNT(*) FROM {table} WHERE (? = 0 OR lang_code = ?) AND {clause}",
        clause = pf.clause
    )
}

/// Source rows for the in-memory prefix-group aggregator — projects just
/// the `search_col` for every matching row.
///
/// Bind order: `lang_code`, `lang_code`, prefix filter binds.
pub fn build_lang_prefix_names_sql(table: &str, search_col: &str, pf: &PrefixFilter) -> String {
    format!(
        "SELECT {search_col} FROM {table} WHERE (? = 0 OR lang_code = ?) AND {clause}",
        clause = pf.clause
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_word_search_produces_two_likes_and_two_binds() {
        let pf = any_word_search("search_full_name", "AB");
        assert_eq!(
            pf.clause,
            "(search_full_name LIKE ? OR search_full_name LIKE ?)"
        );
        assert_eq!(pf.binds, vec!["AB%".to_string(), "% AB%".to_string()]);
    }

    #[test]
    fn first_word_search_produces_one_like_and_one_bind() {
        let pf = first_word_search("search_ser", "AB");
        assert_eq!(pf.clause, "search_ser LIKE ?");
        assert_eq!(pf.binds, vec!["AB%".to_string()]);
    }

    #[test]
    fn prefix_search_dispatches_on_mode() {
        let w = prefix_search("search_title", "AB", PrefixMode::WordBoundary);
        let f = prefix_search("search_title", "AB", PrefixMode::FirstWord);
        assert_eq!(w.binds.len(), 2);
        assert_eq!(f.binds.len(), 1);
        assert_ne!(w.clause, f.clause);
    }

    #[test]
    fn prefix_mode_from_flag() {
        assert_eq!(
            PrefixMode::from_first_word_only(false),
            PrefixMode::WordBoundary
        );
        assert_eq!(
            PrefixMode::from_first_word_only(true),
            PrefixMode::FirstWord
        );
    }

    #[test]
    fn lang_prefix_listing_sql_splices_clause_and_columns() {
        let pf = any_word_search("search_full_name", "AB");
        let sql = build_lang_prefix_listing_sql("authors", "search_full_name", &pf);
        assert!(sql.contains("SELECT * FROM authors"));
        assert!(sql.contains("WHERE (? = 0 OR lang_code = ?)"));
        assert!(sql.contains(&pf.clause));
        assert!(sql.contains("ORDER BY search_full_name"));
        assert!(sql.contains("LIMIT ? OFFSET ?"));
    }

    #[test]
    fn lang_prefix_count_sql_emits_count_star() {
        let pf = first_word_search("search_ser", "AB");
        let sql = build_lang_prefix_count_sql("series", &pf);
        assert!(sql.starts_with("SELECT COUNT(*) FROM series"));
        assert!(sql.contains(&pf.clause));
        assert!(!sql.contains("ORDER BY"));
        assert!(!sql.contains("LIMIT"));
    }

    #[test]
    fn lang_prefix_names_sql_projects_only_search_col() {
        let pf = any_word_search("search_full_name", "A");
        let sql = build_lang_prefix_names_sql("authors", "search_full_name", &pf);
        assert!(sql.starts_with("SELECT search_full_name FROM authors"));
        assert!(!sql.contains("SELECT *"));
        assert!(sql.contains(&pf.clause));
    }
}
