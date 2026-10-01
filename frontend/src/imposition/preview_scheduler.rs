//! Pure scheduling for artwork preview rendering.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::PreviewViewIdentity;

const MAX_BATCH_PAGES: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreviewFailureKind {
    Retryable,
    Deterministic,
}

pub(crate) fn http_failure_kind(status: u16) -> PreviewFailureKind {
    if matches!(status, 408 | 425 | 429) || (500..=599).contains(&status) {
        PreviewFailureKind::Retryable
    } else {
        PreviewFailureKind::Deterministic
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PreviewCacheLimits {
    pub(crate) pages: usize,
    pub(crate) bytes: usize,
}

/// Computes deterministic LRU evictions without touching browser-owned URLs.
///
/// Protected entries are the complete requested and presented views. Their limits are hard:
/// callers must surface an error instead of silently growing the cache beyond its budget.
pub(crate) fn preview_cache_evictions(
    sizes: &BTreeMap<usize, usize>,
    recent: &VecDeque<usize>,
    protected: &BTreeSet<usize>,
    limits: PreviewCacheLimits,
) -> Result<Vec<usize>, String> {
    let protected_pages = sizes.keys().filter(|page| protected.contains(page)).count();
    let protected_bytes = sizes
        .iter()
        .filter(|(page, _)| protected.contains(page))
        .try_fold(0_usize, |total, (_, bytes)| total.checked_add(*bytes))
        .ok_or_else(|| "Artwork preview cache size overflowed.".to_string())?;
    if protected_pages > limits.pages || protected_bytes > limits.bytes {
        return Err(format!(
            "This sheet needs {protected_pages} artwork pages ({protected_bytes} bytes), which exceeds the browser preview cache limit."
        ));
    }

    let mut remaining = sizes.clone();
    let mut page_count = remaining.len();
    let mut byte_count = remaining
        .values()
        .try_fold(0_usize, |total, bytes| total.checked_add(*bytes))
        .ok_or_else(|| "Artwork preview cache size overflowed.".to_string())?;
    let mut evicted = Vec::new();
    for page in recent {
        if page_count <= limits.pages && byte_count <= limits.bytes {
            break;
        }
        if protected.contains(page) {
            continue;
        }
        if let Some(bytes) = remaining.remove(page) {
            page_count = page_count.saturating_sub(1);
            byte_count = byte_count.saturating_sub(bytes);
            evicted.push(*page);
        }
    }
    if page_count > limits.pages || byte_count > limits.bytes {
        return Err("The browser preview cache could not release enough artwork safely.".into());
    }
    Ok(evicted)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreviewPlan {
    token: u64,
    view: PreviewViewIdentity,
    foreground: Vec<Vec<usize>>,
    prefetch: Vec<Vec<usize>>,
}

impl PreviewPlan {
    pub(crate) fn foreground(&self) -> &[Vec<usize>] {
        &self.foreground
    }

    pub(crate) fn prefetch(&self) -> &[Vec<usize>] {
        &self.prefetch
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PreviewScheduler {
    generation: u64,
    active: Option<(u64, PreviewViewIdentity)>,
}

impl PreviewScheduler {
    pub(crate) fn schedule(
        &mut self,
        view: PreviewViewIdentity,
        missing_foreground: &[usize],
        missing_prefetch: &[usize],
    ) -> PreviewPlan {
        self.generation = self.generation.saturating_add(1);
        let token = self.generation;
        self.active = Some((token, view.clone()));
        let foreground_pages = unique_pages(missing_foreground);
        let foreground_set = foreground_pages.iter().copied().collect::<BTreeSet<_>>();
        let prefetch_pages = unique_pages(missing_prefetch)
            .into_iter()
            .filter(|page| !foreground_set.contains(page))
            .collect::<Vec<_>>();
        PreviewPlan {
            token,
            view,
            foreground: request_batches(&foreground_pages),
            prefetch: request_batches(&prefetch_pages),
        }
    }

    pub(crate) fn accepts(&self, plan: &PreviewPlan) -> bool {
        self.active.as_ref() == Some(&(plan.token, plan.view.clone()))
    }

    pub(crate) fn invalidate(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.active = None;
    }

    pub(crate) fn finish(&mut self, plan: &PreviewPlan) -> bool {
        if !self.accepts(plan) {
            return false;
        }
        self.active = None;
        true
    }
}

/// Tracks the single retry allowed for one request batch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BatchAttempt {
    requested: BTreeSet<usize>,
    retry_available: bool,
}

impl BatchAttempt {
    pub(crate) fn new(pages: &[usize]) -> Self {
        Self {
            requested: pages.iter().copied().collect(),
            retry_available: true,
        }
    }

    /// Returns only pages from the original batch that are still missing, once.
    pub(crate) fn retry_missing(&mut self, missing: &[usize]) -> Option<Vec<usize>> {
        if !self.retry_available {
            return None;
        }
        self.retry_available = false;
        let pages = unique_pages(missing)
            .into_iter()
            .filter(|page| self.requested.contains(page))
            .collect::<Vec<_>>();
        (!pages.is_empty()).then_some(pages)
    }
}

fn unique_pages(pages: &[usize]) -> Vec<usize> {
    let mut seen = BTreeSet::new();
    pages
        .iter()
        .copied()
        .filter(|page| seen.insert(*page))
        .collect()
}

pub(crate) fn request_batches(pages: &[usize]) -> Vec<Vec<usize>> {
    let Some((priority, remaining)) = pages.split_first() else {
        return Vec::new();
    };
    std::iter::once(vec![*priority])
        .chain(remaining.chunks(MAX_BATCH_PAGES).map(<[usize]>::to_vec))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imposition::PreviewSide;

    fn view(pages: Vec<usize>, sheet_index: usize) -> PreviewViewIdentity {
        PreviewViewIdentity {
            source_id: "source".into(),
            layout_identity: "layout-a".into(),
            pages,
            retry_generation: 0,
            side: PreviewSide::Front,
            sheet_index,
            sheet_count: 4,
        }
    }

    #[test]
    fn pages_74_through_90_use_five_bounded_foreground_requests() {
        let pages = (74..=90).collect::<Vec<_>>();
        let mut scheduler = PreviewScheduler::default();
        let plan = scheduler.schedule(view(pages.clone(), 0), &pages, &[]);

        assert_eq!(plan.foreground().len(), 5);
        assert_eq!(plan.foreground()[0], vec![74]);
        assert!(plan.foreground().iter().all(|batch| batch.len() <= 4));
        assert_eq!(plan.foreground().concat(), pages);
        assert!(plan.prefetch().is_empty());
    }

    #[test]
    fn latest_navigation_wins_and_stale_completion_is_ignored() {
        let mut scheduler = PreviewScheduler::default();
        let stale = scheduler.schedule(view(vec![1, 2], 0), &[1, 2], &[3, 4]);
        let current = scheduler.schedule(view(vec![5, 6], 1), &[5, 6], &[7, 8]);

        assert!(!scheduler.accepts(&stale));
        assert!(!scheduler.finish(&stale));
        assert!(scheduler.accepts(&current));
        assert!(scheduler.finish(&current));

        let cancelled = scheduler.schedule(view(vec![9], 2), &[9], &[]);
        scheduler.invalidate();
        assert!(!scheduler.accepts(&cancelled));
    }

    #[test]
    fn foreground_precedes_bounded_adjacent_prefetch_without_duplicates() {
        let mut scheduler = PreviewScheduler::default();
        let plan = scheduler.schedule(view(vec![74, 75], 0), &[74, 75], &[75, 76, 77, 78, 79, 80]);

        assert_eq!(plan.foreground(), &[vec![74], vec![75]]);
        assert_eq!(plan.prefetch(), &[vec![76], vec![77, 78, 79, 80]]);
        assert!(plan
            .foreground()
            .iter()
            .chain(plan.prefetch())
            .all(|batch| batch.len() <= 4));
    }

    #[test]
    fn retry_is_once_and_contains_only_missing_pages_from_the_failed_batch() {
        let mut attempt = BatchAttempt::new(&[74, 75, 76, 77]);

        assert_eq!(attempt.retry_missing(&[73, 75, 77, 78]), Some(vec![75, 77]));
        assert_eq!(attempt.retry_missing(&[75]), None);
    }

    #[test]
    fn repeated_failures_cannot_amplify_into_a_request_storm() {
        let pages = (74..=90).collect::<Vec<_>>();
        let mut scheduler = PreviewScheduler::default();
        let plan = scheduler.schedule(view(pages.clone(), 0), &pages, &[]);
        let mut requests = 0;
        for batch in plan.foreground() {
            requests += 1;
            let mut attempt = BatchAttempt::new(batch);
            if attempt.retry_missing(batch).is_some() {
                requests += 1;
            }
            assert!(attempt.retry_missing(batch).is_none());
        }

        assert_eq!(requests, 10);
    }

    #[test]
    fn retry_classification_retries_transient_statuses_only() {
        assert_eq!(http_failure_kind(408), PreviewFailureKind::Retryable);
        assert_eq!(http_failure_kind(429), PreviewFailureKind::Retryable);
        assert_eq!(http_failure_kind(503), PreviewFailureKind::Retryable);
        assert_eq!(http_failure_kind(400), PreviewFailureKind::Deterministic);
        assert_eq!(http_failure_kind(404), PreviewFailureKind::Deterministic);
        assert_eq!(http_failure_kind(422), PreviewFailureKind::Deterministic);
    }

    #[test]
    fn cache_eviction_obeys_byte_and_page_limits_in_lru_order() {
        let sizes = BTreeMap::from([(1, 40), (2, 40), (3, 40), (4, 10)]);
        let recent = VecDeque::from([1, 2, 3, 4]);
        let protected = BTreeSet::from([3]);

        assert_eq!(
            preview_cache_evictions(
                &sizes,
                &recent,
                &protected,
                PreviewCacheLimits {
                    pages: 3,
                    bytes: 70,
                },
            ),
            Ok(vec![1, 2])
        );
    }

    #[test]
    fn cache_rejects_one_protected_view_larger_than_its_byte_budget() {
        let sizes = BTreeMap::from([(1, 70), (2, 40)]);
        let protected = BTreeSet::from([1, 2]);

        let result = preview_cache_evictions(
            &sizes,
            &VecDeque::from([1, 2]),
            &protected,
            PreviewCacheLimits {
                pages: 24,
                bytes: 100,
            },
        );

        assert!(result.is_err());
    }

    #[test]
    fn one_valid_sheet_can_protect_more_than_twenty_four_pages() {
        let sizes = (1..=32).map(|page| (page, 1)).collect::<BTreeMap<_, _>>();
        let recent = (1..=32).collect::<VecDeque<_>>();
        let protected = (1..=32).collect::<BTreeSet<_>>();

        assert_eq!(
            preview_cache_evictions(
                &sizes,
                &recent,
                &protected,
                PreviewCacheLimits {
                    pages: 512,
                    bytes: 64,
                },
            ),
            Ok(Vec::new())
        );
    }

    #[test]
    fn incremental_admission_rejects_the_first_over_budget_asset_without_mutating_the_plan() {
        let admitted = BTreeMap::from([(1, 60)]);
        let protected = BTreeSet::from([1, 2]);
        let limits = PreviewCacheLimits {
            pages: 512,
            bytes: 100,
        };
        assert_eq!(
            preview_cache_evictions(&admitted, &VecDeque::from([1]), &protected, limits,),
            Ok(Vec::new())
        );

        let mut projected = admitted.clone();
        projected.insert(2, 50);
        assert!(
            preview_cache_evictions(&projected, &VecDeque::from([1, 2]), &protected, limits,)
                .is_err()
        );
        assert_eq!(admitted, BTreeMap::from([(1, 60)]));
    }

    #[test]
    fn navigation_can_release_the_old_view_before_admitting_a_large_valid_target() {
        let combined = BTreeMap::from([(1, 45), (2, 45)]);
        let recent = VecDeque::from([1, 2]);
        let limits = PreviewCacheLimits {
            pages: 2,
            bytes: 64,
        };
        assert!(
            preview_cache_evictions(&combined, &recent, &BTreeSet::from([1, 2]), limits,).is_err()
        );
        assert_eq!(
            preview_cache_evictions(&combined, &recent, &BTreeSet::from([2]), limits),
            Ok(vec![1])
        );
    }
}
