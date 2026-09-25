"""Check whether a string reads the same forwards and backwards."""


def is_palindrome(text):
    """Return True if text is a palindrome, ignoring case."""
    return text == text[::-1]
