"""Numbers: the math module, big integers, and a little number theory."""

import math


def sieve(limit):
    is_prime = [True] * (limit + 1)
    is_prime[0] = is_prime[1] = False
    for n in range(2, int(limit**0.5) + 1):
        if is_prime[n]:
            for multiple in range(n * n, limit + 1, n):
                is_prime[multiple] = False
    return [n for n, prime in enumerate(is_prime) if prime]


def factorize(n):
    factors = []
    d = 2
    while d * d <= n:
        while n % d == 0:
            factors.append(d)
            n //= d
        d += 1
    if n > 1:
        factors.append(n)
    return factors


print("primes < 30:", sieve(30))
print("factorize 360:", factorize(360))
print("factorize 97:", factorize(97))

print("gcd/lcm:", math.gcd(48, 36), math.lcm(4, 6))
print("gcd of many:", math.gcd(24, 36, 48))
print("isqrt(1000):", math.isqrt(1000))
print("comb/perm:", math.comb(10, 3), math.perm(5, 2))
print("hypot:", math.hypot(3, 4))

print("trig:", round(math.sin(math.pi / 2), 6), round(math.cos(0), 6))
print("logs:", round(math.log2(1024), 6), round(math.log10(1000), 6))

# Big integers are exact and unbounded.
print("2**100:", 2**100)
print("100!:", math.factorial(100))
print("floor(1e20):", math.floor(1e20))

# Fibonacci-golden-ratio approximation.
phi = (1 + math.sqrt(5)) / 2
print(f"golden ratio: {phi:.10f}")
