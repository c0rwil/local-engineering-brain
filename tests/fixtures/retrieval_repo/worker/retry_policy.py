"""Retry timing for queue publishers."""

from dataclasses import dataclass
import random


def compute_backoff_delay(attempt: int, base_ms: int = 200, cap_ms: int = 30_000) -> int:
    """Return the jittered exponential delay in milliseconds before retry ``attempt``."""
    exponent = min(max(attempt, 0), 16)
    delay = min(cap_ms, base_ms * (2 ** exponent))
    return random.randint(delay // 2, delay)


@dataclass
class RetryBudget:
    """Bound how many retries one message may consume."""

    max_attempts: int = 5
    used: int = 0

    def allow(self) -> bool:
        return self.used < self.max_attempts

    def record_attempt(self) -> None:
        self.used += 1
