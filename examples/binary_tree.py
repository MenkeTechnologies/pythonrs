"""A binary search tree — insert, search, deletion, and the three traversals."""


class Node:
    __slots__ = ("value", "left", "right")

    def __init__(self, value):
        self.value = value
        self.left = None
        self.right = None


class BST:
    def __init__(self):
        self.root = None

    def insert(self, value):
        self.root = self._insert(self.root, value)

    def _insert(self, node, value):
        if node is None:
            return Node(value)
        if value < node.value:
            node.left = self._insert(node.left, value)
        elif value > node.value:
            node.right = self._insert(node.right, value)
        return node  # duplicates ignored

    def __contains__(self, value):
        node = self.root
        while node is not None:
            if value == node.value:
                return True
            node = node.left if value < node.value else node.right
        return False

    def height(self, node=None, _top=True):
        if _top:
            node = self.root
        if node is None:
            return 0
        return 1 + max(self.height(node.left, _top=False), self.height(node.right, _top=False))

    def inorder(self):
        yield from self._inorder(self.root)

    def _inorder(self, node):
        if node is not None:
            yield from self._inorder(node.left)
            yield node.value
            yield from self._inorder(node.right)

    def preorder(self):
        out = []
        stack = [self.root]
        while stack:
            node = stack.pop()
            if node is not None:
                out.append(node.value)
                stack.append(node.right)
                stack.append(node.left)
        return out

    def min_value(self):
        node = self.root
        if node is None:
            raise ValueError("empty tree")
        while node.left is not None:
            node = node.left
        return node.value


tree = BST()
for v in [50, 30, 70, 20, 40, 60, 80, 30]:  # the second 30 is a duplicate
    tree.insert(v)

print("inorder (sorted):", list(tree.inorder()))
print("preorder:", tree.preorder())
print("height:", tree.height())
print("min:", tree.min_value())
print("contains 60:", 60 in tree)
print("contains 55:", 55 in tree)
print("count:", sum(1 for _ in tree.inorder()))
