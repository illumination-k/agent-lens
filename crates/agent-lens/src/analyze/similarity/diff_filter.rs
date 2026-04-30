use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;

use crate::analyze::diff::{LineRange, changed_line_ranges};

use super::{CandidatePairs, OwnedFunction};

pub(super) fn collect_changed_ranges(corpus: &[OwnedFunction]) -> HashMap<PathBuf, Vec<LineRange>> {
    let mut by_file: HashMap<PathBuf, Vec<LineRange>> = HashMap::new();
    for f in corpus {
        if !by_file.contains_key(&f.file) {
            by_file.insert(f.file.clone(), changed_line_ranges(&f.file));
        }
    }
    by_file
}

pub(super) fn filter_pairs_touching_changes<'a>(
    corpus: &[OwnedFunction],
    candidates: &'a CandidatePairs,
    changed_by_file: &HashMap<PathBuf, Vec<LineRange>>,
) -> (Cow<'a, [(usize, usize)]>, usize) {
    let mut filtered = 0usize;
    let pairs: Vec<_> = candidates
        .pairs
        .iter()
        .copied()
        .filter(|&(i, j)| {
            let keep = corpus
                .get(i)
                .zip(corpus.get(j))
                .is_some_and(|(a, b)| pair_touches_changes(a, b, changed_by_file));
            if !keep {
                filtered += 1;
            }
            keep
        })
        .collect();
    (Cow::Owned(pairs), filtered)
}

fn pair_touches_changes(
    a: &OwnedFunction,
    b: &OwnedFunction,
    changed: &HashMap<PathBuf, Vec<LineRange>>,
) -> bool {
    function_touches_changes(a, changed) || function_touches_changes(b, changed)
}

fn function_touches_changes(f: &OwnedFunction, changed: &HashMap<PathBuf, Vec<LineRange>>) -> bool {
    changed.get(&f.file).is_some_and(|ranges| {
        ranges
            .iter()
            .any(|r| r.overlaps(f.def.start_line, f.def.end_line))
    })
}
