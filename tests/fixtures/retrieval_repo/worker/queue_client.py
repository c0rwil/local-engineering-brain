"""Publish messages with bounded retries."""

import time

from worker.retry_policy import RetryBudget, compute_backoff_delay


class QueueClient:
    def __init__(self, transport):
        self.transport = transport

    def publish(self, topic, payload, budget=None):
        budget = budget or RetryBudget()
        while True:
            try:
                return self.transport.send(topic, payload)
            except ConnectionError:
                budget.record_attempt()
                if not budget.allow():
                    raise
                time.sleep(compute_backoff_delay(budget.used) / 1000)
