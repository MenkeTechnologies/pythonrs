"""A small stateful simulation: bank accounts with a transaction ledger."""


class InsufficientFunds(Exception):
    pass


class Account:
    def __init__(self, owner, balance=0):
        self.owner = owner
        self.balance = balance
        self.ledger = []

    def deposit(self, amount):
        self.balance += amount
        self.ledger.append(("deposit", amount, self.balance))
        return self.balance

    def withdraw(self, amount):
        if amount > self.balance:
            raise InsufficientFunds(f"{self.owner} lacks {amount - self.balance}")
        self.balance -= amount
        self.ledger.append(("withdraw", amount, self.balance))
        return self.balance

    def transfer(self, other, amount):
        self.withdraw(amount)
        other.deposit(amount)

    def __repr__(self):
        return f"Account({self.owner!r}, balance={self.balance})"


alice = Account("Alice", 100)
bob = Account("Bob")

alice.deposit(50)
alice.transfer(bob, 30)
bob.deposit(20)

try:
    bob.withdraw(1000)
except InsufficientFunds as e:
    print("declined:", e)

print(alice)
print(bob)

print("\nAlice's ledger:")
for action, amount, balance in alice.ledger:
    print(f"  {action:10} {amount:>5}  -> {balance}")

total = alice.balance + bob.balance
print("\ntotal in system:", total)
