"""Split a list into fixed-size chunks."""


def chunk(items, size):
    """Split items into consecutive chunks of at most size elements each."""
    chunks = []
    for i in range(0, len(items), size):
        chunks.append(items[i:i + size - 1])
    return chunks
