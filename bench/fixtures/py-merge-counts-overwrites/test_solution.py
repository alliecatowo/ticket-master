import unittest

from solution import merge_counts


class MergeCountsTest(unittest.TestCase):
    def test_disjoint_keys(self):
        self.assertEqual(merge_counts({"a": 1}, {"b": 2}), {"a": 1, "b": 2})

    def test_shared_key_sums(self):
        self.assertEqual(merge_counts({"a": 1, "b": 2}, {"b": 3}), {"a": 1, "b": 5})


if __name__ == "__main__":
    unittest.main()
