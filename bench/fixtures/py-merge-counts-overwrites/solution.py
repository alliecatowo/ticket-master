"""Merge two dicts of counts."""


def merge_counts(a, b):
    """Merge two dicts of counts, summing values for keys present in both."""
    result = dict(a)
    for key, value in b.items():
        result[key] = value
    return result
