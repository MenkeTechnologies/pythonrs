"""A finite state machine — a coffee vending machine driven by an event log.

The event-driven dispatch pattern agents write for protocols, parsers, and UI
flows: an explicit state table, a transition function, and a replayable log.
"""

from enum import Enum, auto


class State(Enum):
    IDLE = auto()
    COLLECTING = auto()
    DISPENSING = auto()


PRICE = 75

# transitions[state][event] = handler returning the next state
class Machine:
    def __init__(self):
        self.state = State.IDLE
        self.credit = 0
        self.log = []

    def emit(self, msg):
        self.log.append(msg)

    def handle(self, event, amount=0):
        method = getattr(self, f"on_{event}", None)
        if method is None:
            self.emit(f"ignored {event!r} in {self.state.name}")
            return
        method(amount)

    def on_coin(self, amount):
        if self.state in (State.IDLE, State.COLLECTING):
            self.state = State.COLLECTING
            self.credit += amount
            self.emit(f"credit={self.credit}")
            if self.credit >= PRICE:
                self.state = State.DISPENSING
                change = self.credit - PRICE
                self.emit(f"dispense (change={change})")
                self.credit = 0
                self.state = State.IDLE
        else:
            self.emit("busy")

    def on_refund(self, _amount):
        if self.credit:
            self.emit(f"refund {self.credit}")
            self.credit = 0
            self.state = State.IDLE
        else:
            self.emit("nothing to refund")


events = [
    ("coin", 25),
    ("coin", 25),
    ("cancel", 0),   # unknown event → ignored
    ("coin", 25),
    ("coin", 25),    # crosses the price → dispense + change
    ("refund", 0),   # nothing pending
    ("coin", 100),   # single coin over price
]

m = Machine()
for name, amt in events:
    m.handle(name, amt)

for line in m.log:
    print(line)
print("final state:", m.state.name)
