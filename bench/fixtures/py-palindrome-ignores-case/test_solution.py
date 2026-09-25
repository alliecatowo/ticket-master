import unittest

from solution import is_palindrome


class PalindromeTest(unittest.TestCase):
    def test_lowercase(self):
        self.assertTrue(is_palindrome("level"))

    def test_mixed_case(self):
        self.assertTrue(is_palindrome("Level"))

    def test_not_palindrome(self):
        self.assertFalse(is_palindrome("hello"))


if __name__ == "__main__":
    unittest.main()
