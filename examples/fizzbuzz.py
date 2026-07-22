"""FizzBuzz, three ways — the classic warm-up."""


def classic(n):
    for i in range(1, n + 1):
        if i % 15 == 0:
            print("FizzBuzz")
        elif i % 3 == 0:
            print("Fizz")
        elif i % 5 == 0:
            print("Buzz")
        else:
            print(i)


def one_liner(n):
    return [
        "Fizz" * (i % 3 == 0) + "Buzz" * (i % 5 == 0) or str(i)
        for i in range(1, n + 1)
    ]


classic(15)
print("---")
print(" ".join(one_liner(15)))
