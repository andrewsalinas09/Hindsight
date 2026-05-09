# SPDX-License-Identifier: Apache-2.0
"""parser.py — recursive-descent parser for arithmetic expressions.

Stresses **deep recursion + string slicing + exception-based control
flow**. Parser functions read characters, accumulate partial results,
and raise on parse errors that propagate via PY_UNWIND.

Grammar (simple integer arithmetic with parens):

    expr   := term ('+' term | '-' term)*
    term   := factor ('*' factor | '/' factor)*
    factor := number | '(' expr ')'
    number := [0-9]+

Expected trace shape: deep nested call trees during parens, lots of
short-lived containers (digit accumulator lists), Parser instance as
``self`` summary. Stresses recursive parsing patterns realistically.
"""
from __future__ import annotations

import os

os.environ.setdefault("HINDSIGHT_OUTPUT_PATH", "parser.hindsight")

import hindsight


class Parser:
    def __init__(self, text: str) -> None:
        self.text = text
        self.pos = 0

    def peek(self) -> str:
        return self.text[self.pos] if self.pos < len(self.text) else ""

    def consume(self) -> str:
        c = self.peek()
        self.pos += 1
        return c

    def skip_ws(self) -> None:
        while self.peek() == " ":
            self.pos += 1


def parse_number(p: Parser) -> int:
    p.skip_ws()
    digits: list[str] = []
    while p.peek().isdigit():
        digits.append(p.consume())
    if not digits:
        raise ValueError(f"expected number at pos {p.pos}")
    return int("".join(digits))


def parse_factor(p: Parser) -> int:
    p.skip_ws()
    if p.peek() == "(":
        p.consume()
        result = parse_expr(p)
        p.skip_ws()
        closer = p.consume()
        if closer != ")":
            raise ValueError(f"expected ')' at pos {p.pos}, got {closer!r}")
        return result
    return parse_number(p)


def parse_term(p: Parser) -> int:
    left = parse_factor(p)
    while True:
        p.skip_ws()
        op = p.peek()
        if not op or op not in "*/":
            return left
        p.consume()
        right = parse_factor(p)
        if op == "*":
            left = left * right
        else:
            left = left // right


def parse_expr(p: Parser) -> int:
    left = parse_term(p)
    while True:
        p.skip_ws()
        op = p.peek()
        if not op or op not in "+-":
            return left
        p.consume()
        right = parse_term(p)
        if op == "+":
            left = left + right
        else:
            left = left - right


@hindsight.record
def main() -> int:
    expression = "3 + 4 * (2 - 1) + 10 / 2"
    p = Parser(expression)
    result = parse_expr(p)
    hindsight.note(
        "parse complete",
        expression=expression,
        result=result,
        pos_at_end=p.pos,
    )
    return result


if __name__ == "__main__":
    out = main()
    print(f"3 + 4 * (2 - 1) + 10 / 2 = {out}")
    assert out == 12, f"expected 12, got {out}"
