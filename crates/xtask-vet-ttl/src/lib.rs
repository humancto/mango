// Parse cargo-vet `supply-chain/config.toml` and extract TTL metadata
// from `[[exemptions.<crate>]]` entries carrying a `review-by:
// YYYY-MM-DD` token inside their `notes` string.
//
// See `docs/supply-chain-policy.md` for the convention. The parser is
// intentionally lenient about surrounding text: contributors write
// free-form rationale in `notes`, and we only pluck the ISO-8601 date
// that follows a `review-by:` label.

// Internal supply-chain helper; `publish = false`. Deliberately outside
// the `crates/mango-*` missing_docs gate per docs/documentation-policy.md
// — but declared allowed explicitly so a future `rustc` default flip
// from `allow` to `warn` for `missing_docs` cannot retroactively red
// the `doc` CI job. One line of cheap insurance.
#![allow(missing_docs)]

use std::fmt;

use time::format_description::FormatItem;
use time::macros::format_description;
use time::Date;

/// The single-source format for our TTL dates. ISO-8601 with
/// zero-padded fields so that lex comparison and semantic
/// comparison agree.
pub const DATE_FORMAT: &[FormatItem<'_>] = format_description!("[year]-[month]-[day]");

/// Literal tag that labels the TTL date inside an exemption's
/// `notes` string.
pub const REVIEW_BY_LABEL: &str = "review-by:";

/// One exemption row carrying a TTL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExemptionReview {
    pub crate_name: String,
    pub version: String,
    pub review_by: Date,
}

/// Failures surfaced to the CLI. Each variant maps to an exit code
/// discipline documented in `main.rs`.
#[derive(Debug)]
pub enum ParseError {
    /// The config file failed TOML syntax.
    TomlSyntax(toml::de::Error),
    /// `[exemptions]` was present but the shape did not match what
    /// cargo-vet emits (array-of-tables keyed by crate name).
    ShapeMismatch(String),
    /// A `[[exemptions.<crate>]]` table lacks the `notes` string or
    /// `notes` does not contain `review-by:`.
    MissingReviewBy { crate_name: String, version: String },
    /// `notes` contains `review-by:` but the date that follows is not
    /// a syntactically valid `YYYY-MM-DD`.
    MalformedDate {
        crate_name: String,
        version: String,
        raw: String,
    },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TomlSyntax(e) => write!(f, "config.toml syntax error: {e}"),
            Self::ShapeMismatch(s) => write!(f, "unexpected [exemptions] shape: {s}"),
            Self::MissingReviewBy {
                crate_name,
                version,
            } => write!(
                f,
                "exemption for `{crate_name}@{version}` has no `review-by:` in notes"
            ),
            Self::MalformedDate {
                crate_name,
                version,
                raw,
            } => write!(
                f,
                "exemption for `{crate_name}@{version}`: malformed review-by date `{raw}` (want YYYY-MM-DD)"
            ),
        }
    }
}

impl std::error::Error for ParseError {}

impl From<toml::de::Error> for ParseError {
    fn from(e: toml::de::Error) -> Self {
        Self::TomlSyntax(e)
    }
}

/// Extract the `YYYY-MM-DD` that follows the first occurrence of
/// `review-by:` in `notes`. Whitespace around the label and the date
/// is tolerated; anything after the 10-character date is ignored (so
/// `review-by: 2026-10-23; reason follows.` works).
///
/// Returns `Ok(None)` when the label is absent (caller decides if
/// that's fatal). Returns `Err(..)` when the label is present but the
/// date that follows is not a valid ISO-8601 `YYYY-MM-DD`.
pub fn parse_review_by(notes: &str) -> Result<Option<Date>, time::error::Parse> {
    let Some(after_label) = notes.find(REVIEW_BY_LABEL).map(|i| {
        // BOUND: `find` returns a valid UTF-8 index; adding the
        // label's known-ASCII length stays within bounds.
        i.saturating_add(REVIEW_BY_LABEL.len())
    }) else {
        return Ok(None);
    };
    let tail = notes.get(after_label..).unwrap_or("").trim_start();
    // Take exactly 10 chars `YYYY-MM-DD`. Anything shorter fails
    // parsing; anything longer is ignored and the caller's free-form
    // rationale text goes after.
    let date_slice: String = tail.chars().take(10).collect();
    let date = Date::parse(&date_slice, DATE_FORMAT)?;
    Ok(Some(date))
}

/// Parse a full `supply-chain/config.toml` string and extract every
/// `[[exemptions.<crate>]]` entry that carries a `review-by:` date.
///
/// Entries without any `notes` field, or with a `notes` field that
/// lacks `review-by:`, are returned as `MissingReviewBy` errors in
/// the second return vector. Callers decide whether missing dates are
/// fatal — the workflow currently treats them as non-fatal warnings
/// so contributors can add exemptions without an immediate TTL.
///
/// The split return is deliberate: the `Vec<ExemptionReview>` feeds
/// the date-compare step, and the `Vec<ParseError>` surfaces the
/// "present but unparseable" cases (malformed date) plus "present but
/// no TTL" (missing review-by). Shape mismatches and TOML syntax
/// errors are hard-fail and come back as `Err`.
pub fn extract_exemption_reviews(
    config_toml: &str,
) -> Result<(Vec<ExemptionReview>, Vec<ParseError>), ParseError> {
    let table: toml::Table = toml::from_str(config_toml)?;

    let Some(exemptions) = table.get("exemptions") else {
        // No [exemptions] block at all — empty result, not an error.
        return Ok((Vec::new(), Vec::new()));
    };

    let exemptions_table = exemptions
        .as_table()
        .ok_or_else(|| ParseError::ShapeMismatch("`exemptions` is not a table".to_string()))?;

    let mut reviews = Vec::new();
    let mut soft_errors = Vec::new();

    for (crate_name, entries_value) in exemptions_table {
        let entries = entries_value.as_array().ok_or_else(|| {
            ParseError::ShapeMismatch(format!("exemptions.{crate_name} is not an array of tables"))
        })?;
        for entry in entries {
            let entry_table = entry.as_table().ok_or_else(|| {
                ParseError::ShapeMismatch(format!(
                    "exemptions.{crate_name}[..] element is not a table"
                ))
            })?;
            let version = entry_table
                .get("version")
                .and_then(toml::Value::as_str)
                .unwrap_or("<no-version>")
                .to_string();
            let notes = entry_table
                .get("notes")
                .and_then(toml::Value::as_str)
                .unwrap_or("");
            match parse_review_by(notes) {
                Ok(Some(date)) => reviews.push(ExemptionReview {
                    crate_name: crate_name.clone(),
                    version,
                    review_by: date,
                }),
                Ok(None) => soft_errors.push(ParseError::MissingReviewBy {
                    crate_name: crate_name.clone(),
                    version,
                }),
                Err(_) => {
                    let raw = extract_raw_date_slice(notes);
                    soft_errors.push(ParseError::MalformedDate {
                        crate_name: crate_name.clone(),
                        version,
                        raw,
                    });
                }
            }
        }
    }

    Ok((reviews, soft_errors))
}

/// Pull out the 10-char slice that `parse_review_by` tried to parse,
/// for error reporting. Duplicates the slice extraction logic in
/// `parse_review_by`; kept narrow so the happy path stays zero-copy.
fn extract_raw_date_slice(notes: &str) -> String {
    let Some(after_label) = notes
        .find(REVIEW_BY_LABEL)
        .map(|i| i.saturating_add(REVIEW_BY_LABEL.len()))
    else {
        return String::new();
    };
    let tail = notes.get(after_label..).unwrap_or("").trim_start();
    tail.chars().take(10).collect()
}

/// Partition the reviews into `(expired, current)` relative to
/// `today`. `expired` means `review_by < today`.
#[must_use]
pub fn partition_by_date(
    reviews: Vec<ExemptionReview>,
    today: Date,
) -> (Vec<ExemptionReview>, Vec<ExemptionReview>) {
    let mut expired = Vec::new();
    let mut current = Vec::new();
    for r in reviews {
        if r.review_by < today {
            expired.push(r);
        } else {
            current.push(r);
        }
    }
    (expired, current)
}

// Tests intentionally use unwrap/expect/indexing/panic for concise
// assertions. `reason = "..."` on `#[allow]` is 1.81+; the workspace
// MSRV is 1.80.
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
mod tests {
    use super::*;
    use time::macros::date;

    #[test]
    fn parse_review_by_simple() {
        let got = parse_review_by("review-by: 2026-10-23").unwrap().unwrap();
        assert_eq!(got, date!(2026 - 10 - 23));
    }

    #[test]
    fn parse_review_by_with_trailing_rationale() {
        let got = parse_review_by("review-by: 2026-10-23; no audit yet, small crate")
            .unwrap()
            .unwrap();
        assert_eq!(got, date!(2026 - 10 - 23));
    }

    #[test]
    fn parse_review_by_with_leading_text() {
        let got = parse_review_by("TBD audit. review-by: 2027-01-01 will bump when ring lands")
            .unwrap()
            .unwrap();
        assert_eq!(got, date!(2027 - 01 - 01));
    }

    #[test]
    fn parse_review_by_extra_whitespace() {
        let got = parse_review_by("review-by:    2026-12-31")
            .unwrap()
            .unwrap();
        assert_eq!(got, date!(2026 - 12 - 31));
    }

    #[test]
    fn parse_review_by_absent_returns_none() {
        let got = parse_review_by("no ttl here, just rationale").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn parse_review_by_empty_returns_none() {
        let got = parse_review_by("").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn parse_review_by_malformed_errors() {
        assert!(
            parse_review_by("review-by: 2026-13-01").is_err(),
            "bad month"
        );
        assert!(
            parse_review_by("review-by: 2026/10/23").is_err(),
            "wrong separator"
        );
        assert!(
            parse_review_by("review-by: not-a-date").is_err(),
            "not numeric"
        );
        assert!(parse_review_by("review-by: 2026-10").is_err(), "too short");
    }

    #[test]
    fn extract_empty_config() {
        let (reviews, soft) = extract_exemption_reviews("").unwrap();
        assert!(reviews.is_empty());
        assert!(soft.is_empty());
    }

    #[test]
    fn extract_no_exemptions_section() {
        let cfg = r#"
[cargo-vet]
version = "0.10.2"
"#;
        let (reviews, soft) = extract_exemption_reviews(cfg).unwrap();
        assert!(reviews.is_empty());
        assert!(soft.is_empty());
    }

    #[test]
    fn extract_single_exemption_happy() {
        let cfg = r#"
[[exemptions.some-crate]]
version = "1.2.3"
criteria = "safe-to-deploy"
notes = "review-by: 2026-10-23; no audit yet"
"#;
        let (reviews, soft) = extract_exemption_reviews(cfg).unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].crate_name, "some-crate");
        assert_eq!(reviews[0].version, "1.2.3");
        assert_eq!(reviews[0].review_by, date!(2026 - 10 - 23));
        assert!(soft.is_empty());
    }

    #[test]
    fn extract_multiple_versions_same_crate() {
        let cfg = r#"
[[exemptions.dual-ver]]
version = "1.0.0"
criteria = "safe-to-deploy"
notes = "review-by: 2026-10-23"

[[exemptions.dual-ver]]
version = "2.0.0"
criteria = "safe-to-deploy"
notes = "review-by: 2026-11-30"
"#;
        let (reviews, soft) = extract_exemption_reviews(cfg).unwrap();
        assert_eq!(reviews.len(), 2);
        assert!(soft.is_empty());
        let versions: Vec<&str> = reviews.iter().map(|r| r.version.as_str()).collect();
        assert!(versions.contains(&"1.0.0"));
        assert!(versions.contains(&"2.0.0"));
    }

    #[test]
    fn extract_missing_review_by_becomes_soft_error() {
        let cfg = r#"
[[exemptions.silent-crate]]
version = "0.1.0"
criteria = "safe-to-deploy"
notes = "no ttl label at all"
"#;
        let (reviews, soft) = extract_exemption_reviews(cfg).unwrap();
        assert!(reviews.is_empty());
        assert_eq!(soft.len(), 1);
        // Variant classification is load-bearing: the xtask's exit
        // codes depend on MissingReviewBy vs MalformedDate. A bare
        // `matches!(...)` would type-check but discard the boolean;
        // `assert!(matches!(...))` is what actually verifies.
        assert!(matches!(
            soft[0],
            ParseError::MissingReviewBy { ref crate_name, .. }
                if crate_name == "silent-crate"
        ));
    }

    #[test]
    fn extract_malformed_date_becomes_soft_error() {
        let cfg = r#"
[[exemptions.bad-date]]
version = "0.1.0"
criteria = "safe-to-deploy"
notes = "review-by: 2026-13-40; oops"
"#;
        let (reviews, soft) = extract_exemption_reviews(cfg).unwrap();
        assert!(reviews.is_empty());
        assert_eq!(soft.len(), 1);
        assert!(matches!(
            soft[0],
            ParseError::MalformedDate { ref crate_name, .. }
                if crate_name == "bad-date"
        ));
    }

    #[test]
    fn extract_shape_mismatch_is_hard_fail() {
        let cfg = r#"
[exemptions]
not-an-array = "scalar"
"#;
        let res = extract_exemption_reviews(cfg);
        assert!(
            res.is_err(),
            "scalar under [exemptions.<crate>] must be rejected"
        );
    }

    #[test]
    fn extract_no_notes_field_becomes_soft_error() {
        let cfg = r#"
[[exemptions.bare]]
version = "0.1.0"
criteria = "safe-to-deploy"
"#;
        let (reviews, soft) = extract_exemption_reviews(cfg).unwrap();
        assert!(reviews.is_empty());
        assert_eq!(soft.len(), 1);
    }

    #[test]
    fn partition_by_date_splits_correctly() {
        let reviews = vec![
            ExemptionReview {
                crate_name: "past".into(),
                version: "1.0".into(),
                review_by: date!(2020 - 01 - 01),
            },
            ExemptionReview {
                crate_name: "future".into(),
                version: "1.0".into(),
                review_by: date!(2099 - 01 - 01),
            },
            ExemptionReview {
                crate_name: "today".into(),
                version: "1.0".into(),
                review_by: date!(2026 - 04 - 23),
            },
        ];
        let today = date!(2026 - 04 - 23);
        let (expired, current) = partition_by_date(reviews, today);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].crate_name, "past");
        assert_eq!(current.len(), 2);
        // Today's date is treated as NOT expired (boundary: >= today).
        assert!(current.iter().any(|r| r.crate_name == "today"));
        assert!(current.iter().any(|r| r.crate_name == "future"));
    }
}
