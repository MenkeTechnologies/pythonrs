# Python floors // toward -inf and % takes the divisor's sign. A C-style
# truncating implementation diverges here.
print(-7 // 2, -7 % 2)
print(7 // -2, 7 % -2)
print(-7 // -2, -7 % -2)
print(divmod(-7, 3))
print(divmod(7, -3))
