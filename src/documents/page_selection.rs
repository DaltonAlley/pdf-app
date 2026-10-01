use std::collections::BTreeSet;

use crate::error::{AppError, AppResult};

const MAX_PARSED_PAGES: usize = 10_000;
const MAX_PAGE_EXPRESSION_BYTES: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PageSelection {
    All,
    Pages(Vec<usize>),
}

impl PageSelection {
    pub(crate) fn parse(expr: &str) -> AppResult<Self> {
        let expr = expr.trim();
        if expr.is_empty() {
            return Err(AppError::bad_request("pages cannot be empty"));
        }
        if expr.len() > MAX_PAGE_EXPRESSION_BYTES {
            return Err(AppError::bad_request(format!(
                "page selection cannot exceed {MAX_PAGE_EXPRESSION_BYTES} characters"
            )));
        }
        if expr == "all" {
            return Ok(Self::All);
        }

        let mut pages = BTreeSet::new();
        for part in expr.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err(AppError::bad_request("pages contains an empty segment"));
            }

            if let Some((start, end)) = part.split_once('-') {
                let start = parse_page(part, start)?;
                let end = parse_page(part, end)?;
                if start > end {
                    return Err(AppError::bad_request("page ranges must be ascending"));
                }
                let range_len = end
                    .checked_sub(start)
                    .and_then(|length| length.checked_add(1))
                    .ok_or_else(|| AppError::bad_request("page range is too large"))?;
                if range_len > MAX_PARSED_PAGES {
                    return Err(AppError::bad_request(format!(
                        "page selection cannot contain more than {MAX_PARSED_PAGES} pages"
                    )));
                }
                pages.extend(start..=end);
                if pages.len() > MAX_PARSED_PAGES {
                    return Err(AppError::bad_request(format!(
                        "page selection cannot contain more than {MAX_PARSED_PAGES} pages"
                    )));
                }
            } else {
                pages.insert(parse_page(part, part)?);
                if pages.len() > MAX_PARSED_PAGES {
                    return Err(AppError::bad_request(format!(
                        "page selection cannot contain more than {MAX_PARSED_PAGES} pages"
                    )));
                }
            }
        }

        if pages.is_empty() {
            Err(AppError::bad_request("pages cannot be empty"))
        } else {
            Ok(Self::Pages(pages.into_iter().collect()))
        }
    }

    pub(crate) fn resolve(self, page_count: usize) -> AppResult<Vec<usize>> {
        match self {
            Self::All => Ok((1..=page_count).collect()),
            Self::Pages(pages) if pages.is_empty() => {
                Err(AppError::bad_request("no page numbers specified"))
            }
            Self::Pages(pages) => {
                if let Some(page) = pages.iter().find(|page| **page == 0 || **page > page_count) {
                    if *page == 0 {
                        return Err(AppError::bad_request("page numbers start at 1"));
                    }
                    return Err(AppError::bad_request(format!(
                        "page {page} is out of range; PDF has {page_count} pages"
                    )));
                }
                Ok(pages)
            }
        }
    }
}

fn parse_page(segment: &str, value: &str) -> AppResult<usize> {
    let value = value.trim();
    let page = value
        .parse::<usize>()
        .map_err(|_| AppError::bad_request(format!("invalid page `{segment}`")))?;
    if page == 0 {
        Err(AppError::bad_request("page numbers start at 1"))
    } else {
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_expr_rejects_zero() {
        assert!(PageSelection::parse("0").is_err());
    }

    #[test]
    fn page_expr_rejects_empty_segments() {
        assert!(PageSelection::parse("1,,2").is_err());
    }

    #[test]
    fn page_expr_expands_and_sorts_ranges() {
        assert_eq!(
            PageSelection::parse("3,1-2").unwrap(),
            PageSelection::Pages(vec![1, 2, 3])
        );
    }

    #[test]
    fn page_expr_trims_whitespace_inside_segments() {
        assert_eq!(
            PageSelection::parse(" 1, 3 - 4 ").unwrap(),
            PageSelection::Pages(vec![1, 3, 4])
        );
    }

    #[test]
    fn page_expr_deduplicates_and_sorts_pages() {
        assert_eq!(
            PageSelection::parse("2,1,2").unwrap(),
            PageSelection::Pages(vec![1, 2])
        );
    }

    #[test]
    fn page_expr_rejects_descending_ranges_and_invalid_tokens() {
        assert!(PageSelection::parse("3-1").is_err());
        assert!(PageSelection::parse("abc").is_err());
    }

    #[test]
    fn page_expr_rejects_huge_ranges_without_expanding_them() {
        let error = PageSelection::parse("1-18446744073709551615").unwrap_err();
        assert!(error.to_string().contains("cannot contain more"));
    }

    #[test]
    fn page_expr_rejects_pathologically_long_duplicate_lists() {
        let expression = "1,".repeat(MAX_PAGE_EXPRESSION_BYTES);
        let error = PageSelection::parse(&expression).unwrap_err();
        assert!(error.to_string().contains("cannot exceed"));
    }

    #[test]
    fn page_expr_accepts_all() {
        assert_eq!(PageSelection::parse("all").unwrap(), PageSelection::All);
    }

    #[test]
    fn page_expr_all_is_lowercase_only() {
        assert!(PageSelection::parse("ALL").is_err());
    }

    #[test]
    fn all_resolves_to_empty_for_empty_documents() {
        assert_eq!(PageSelection::All.resolve(0).unwrap(), Vec::<usize>::new());
    }

    #[test]
    fn explicitly_constructed_pages_are_validated_when_resolved() {
        assert_eq!(
            PageSelection::Pages(vec![0])
                .resolve(2)
                .unwrap_err()
                .to_string(),
            "page numbers start at 1"
        );
        assert_eq!(
            PageSelection::Pages(vec![3])
                .resolve(2)
                .unwrap_err()
                .to_string(),
            "page 3 is out of range; PDF has 2 pages"
        );
    }
}
