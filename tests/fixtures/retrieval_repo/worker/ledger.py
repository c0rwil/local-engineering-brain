"""Invoice ledger arithmetic."""

from decimal import Decimal


class InvoiceLedger:
    def __init__(self):
        self.lines = []

    def add_line(self, description, amount):
        self.lines.append((description, Decimal(amount)))

    def reconcile_totals(self, expected):
        return sum(amount for _, amount in self.lines) == Decimal(expected)
