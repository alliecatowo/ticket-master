import unittest

from solution import binary_search


class BinarySearchTest(unittest.TestCase):
    def test_found_middle(self):
        self.assertEqual(binary_search([1, 3, 5, 7, 9], 5), 2)

    def test_found_last(self):
        self.assertEqual(binary_search([1, 3, 5, 7, 9], 9), 4)

    def test_single_element_found(self):
        self.assertEqual(binary_search([42], 42), 0)

    def test_missing(self):
        self.assertEqual(binary_search([1, 3, 5], 4), -1)


if __name__ == "__main__":
    unittest.main()
