"""A shunting-yard expression evaluator — tokenize, to RPN, then evaluate."""

import operator

OPS = {
    "+": (1, operator.add),
    "-": (1, operator.sub),
    "*": (2, operator.mul),
    "/": (2, operator.truediv),
    "^": (3, operator.pow),
}


def tokenize(expr):
    tokens, num = [], ""
    for ch in expr:
        if ch.isdigit() or ch == ".":
            num += ch
        else:
            if num:
                tokens.append(float(num))
                num = ""
            if ch in OPS or ch in "()":
                tokens.append(ch)
    if num:
        tokens.append(float(num))
    return tokens


def to_rpn(tokens):
    output, stack = [], []
    for tok in tokens:
        if isinstance(tok, float):
            output.append(tok)
        elif tok in OPS:
            while stack and stack[-1] in OPS and OPS[stack[-1]][0] >= OPS[tok][0]:
                output.append(stack.pop())
            stack.append(tok)
        elif tok == "(":
            stack.append(tok)
        elif tok == ")":
            while stack[-1] != "(":
                output.append(stack.pop())
            stack.pop()
    while stack:
        output.append(stack.pop())
    return output


def evaluate(rpn):
    stack = []
    for tok in rpn:
        if isinstance(tok, float):
            stack.append(tok)
        else:
            b, a = stack.pop(), stack.pop()
            stack.append(OPS[tok][1](a, b))
    return stack[0]


for expr in ["2 + 3 * 4", "(2 + 3) * 4", "2 ^ 3 ^ 2", "10 / 2 - 3"]:
    rpn = to_rpn(tokenize(expr))
    result = evaluate(rpn)
    print(f"{expr:14} = {result:g}")
