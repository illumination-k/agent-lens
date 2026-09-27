"""Metrics over scored pairs. Standard library only.

A scored pair is a dict with `label` (bool), `kind` (clone type for a
positive, negative kind otherwise) and `score` (float in [0, 1], 0 when
agent-lens reported nothing for the pair). The meaning of every number is in
docs/similarity-accuracy.md.
"""

from __future__ import annotations

from collections.abc import Iterable, Sequence

THRESHOLDS = [0.7, 0.8, 0.85, 0.9]


def roc_auc(pos: Sequence[float], neg: Sequence[float]) -> float | None:
    """P(score(positive) > score(negative)), ties counting one half.

    Computed by rank sum (Mann-Whitney U), O(n log n). None when either side
    is empty.
    """
    if not pos or not neg:
        return None
    ranked = sorted([(s, 1) for s in pos] + [(s, 0) for s in neg])
    rank_sum = 0.0
    i = 0
    while i < len(ranked):
        j = i
        while j < len(ranked) and ranked[j][0] == ranked[i][0]:
            j += 1
        mid_rank = (i + 1 + j) / 2  # 1-based ranks i+1..j share their mean
        rank_sum += mid_rank * sum(flag for _, flag in ranked[i:j])
        i = j
    n_pos, n_neg = len(pos), len(neg)
    return (rank_sum - n_pos * (n_pos + 1) / 2) / (n_pos * n_neg)


def rate_at(scores: Sequence[float], threshold: float) -> float | None:
    """Fraction of scores at or above the threshold (recall or FPR)."""
    if not scores:
        return None
    return sum(s >= threshold for s in scores) / len(scores)


def best_f1(pos: Sequence[float], neg: Sequence[float]) -> tuple[float, float] | None:
    """(F1, threshold) maximising F1 over every observed score as a cut."""
    if not pos:
        return None
    best = (0.0, 1.0)
    for t in sorted(set(pos) | set(neg)):
        tp = sum(s >= t for s in pos)
        fp = sum(s >= t for s in neg)
        if tp == 0:
            continue
        precision = tp / (tp + fp)
        recall = tp / len(pos)
        f1 = 2 * precision * recall / (precision + recall)
        if f1 > best[0]:
            best = (f1, t)
    return best


def split(pairs: Iterable[dict]) -> tuple[dict[str, list[float]], dict[str, list[float]]]:
    """Scores grouped by kind: (positives, negatives)."""
    pos: dict[str, list[float]] = {}
    neg: dict[str, list[float]] = {}
    for p in pairs:
        (pos if p["label"] else neg).setdefault(p["kind"], []).append(p["score"])
    return pos, neg


def summarize(pairs: Sequence[dict], negative_kinds: Sequence[str] = ("cross",)) -> dict:
    """Every metric for one (language, method) cell.

    AUC per positive kind is taken against the pooled `negative_kinds`; other
    negative kinds (e.g. `hard`) get their own AUC against all positives, so a
    small adversarial set does not dilute the headline number.
    """
    pos, neg = split(pairs)
    all_pos = [s for scores in pos.values() for s in scores]
    base_neg = [s for k in negative_kinds for s in neg.get(k, [])]
    out: dict = {
        "counts": {k: len(v) for k, v in sorted({**pos, **neg}.items())},
        "auc": {k: roc_auc(v, base_neg) for k, v in sorted(pos.items())},
        "recall": {k: {str(t): rate_at(v, t) for t in THRESHOLDS} for k, v in sorted(pos.items())},
        "fpr": {k: {str(t): rate_at(v, t) for t in THRESHOLDS} for k, v in sorted(neg.items())},
    }
    out["auc"]["all"] = roc_auc(all_pos, base_neg)
    for kind, scores in sorted(neg.items()):
        if kind not in negative_kinds:
            out["auc"][f"vs_{kind}"] = roc_auc(all_pos, scores)
    f1 = best_f1(all_pos, base_neg)
    out["best_f1"] = None if f1 is None else {"f1": f1[0], "threshold": f1[1]}
    return out


def fmt(value: float | None) -> str:
    return "-" if value is None else f"{value:.2f}"
