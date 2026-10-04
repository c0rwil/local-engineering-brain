#!/usr/bin/env python3
"""Private behavioral grader for h04 (standard library only).

Executes the candidate's real RevenueCatService and SubscriptionRepository.
Repository methods that write state are replaced by recording fakes; plan
lookups run the candidate's own repository code against a bounded in-memory
query emulation, so a fix in either the service or the repository is graded
by its observable behavior. Run only against an expendable grading copy.
"""
from __future__ import annotations

import enum
import importlib.util
import operator
from pathlib import Path
import re
import sys
from types import SimpleNamespace
import uuid

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "_shared"))
sys.dont_write_bytecode = True
from py_harness import main, module, package  # noqa: E402

TASK_ID = "h04_play_base_plan_lookup"
ENVIRONMENT = {"APP_ENV": "test", "ENV": "test", "REVENUECAT_BILLING": "true", "PYTHONDONTWRITEBYTECODE": "1"}

# ---------------------------------------------------------------------------
# Bounded query emulation: single-model queries, comparisons, boolean
# composition, a few SQL functions and ordering. Unsupported constructs raise.
# ---------------------------------------------------------------------------


class Unsupported(NotImplementedError):
    pass


def _value(value, row):
    return value.evaluate(row) if isinstance(value, Expr) else value


def _like(pattern: str, flags=0):
    regex = "".join(".*" if ch == "%" else "." if ch == "_" else re.escape(ch) for ch in pattern)
    return re.compile(f"^{regex}$", flags | re.DOTALL)


class Pred:
    def __init__(self, function):
        self.function = function

    def __call__(self, row):
        return bool(self.function(row))

    def __and__(self, other):
        return Pred(lambda row: self(row) and as_pred(other)(row))

    def __or__(self, other):
        return Pred(lambda row: self(row) or as_pred(other)(row))

    def __invert__(self):
        return Pred(lambda row: not self(row))

    def self_group(self):
        return self


def as_pred(value):
    if isinstance(value, Pred):
        return value
    if isinstance(value, bool):
        return Pred(lambda row: value)
    if isinstance(value, Expr):
        return Pred(lambda row: bool(value.evaluate(row)))
    raise Unsupported(f"unsupported filter expression {value!r}")


class Expr:
    def evaluate(self, row):
        raise Unsupported(repr(self))

    def _compare(self, other, function):
        return Pred(lambda row: function(self.evaluate(row), _value(other, row)))

    def __eq__(self, other):
        return self._compare(other, operator.eq)

    def __ne__(self, other):
        return self._compare(other, operator.ne)

    def __lt__(self, other):
        return self._compare(other, operator.lt)

    def __le__(self, other):
        return self._compare(other, operator.le)

    def __gt__(self, other):
        return self._compare(other, operator.gt)

    def __ge__(self, other):
        return self._compare(other, operator.ge)

    __hash__ = object.__hash__

    def in_(self, values):
        return Pred(lambda row: self.evaluate(row) in [_value(item, row) for item in values])

    def not_in(self, values):
        return ~self.in_(values)

    notin_ = not_in

    def is_(self, other):
        return Pred(lambda row: self.evaluate(row) is _value(other, row))

    def is_not(self, other):
        return Pred(lambda row: self.evaluate(row) is not _value(other, row))

    isnot = is_not

    def like(self, pattern):
        return Pred(lambda row: self.evaluate(row) is not None and bool(_like(_value(pattern, row)).match(self.evaluate(row))))

    def ilike(self, pattern):
        return Pred(lambda row: self.evaluate(row) is not None
                    and bool(_like(_value(pattern, row), re.IGNORECASE).match(self.evaluate(row))))

    def startswith(self, prefix):
        return Pred(lambda row: str(self.evaluate(row) or "").startswith(_value(prefix, row)))

    def endswith(self, suffix):
        return Pred(lambda row: str(self.evaluate(row) or "").endswith(_value(suffix, row)))

    def contains(self, part):
        return Pred(lambda row: _value(part, row) in str(self.evaluate(row) or ""))

    def asc(self):
        return Order(self, False)

    def desc(self):
        return Order(self, True)

    def label(self, name):
        return self


class Column(Expr):
    def __init__(self, name):
        self.name = name

    def evaluate(self, row):
        return getattr(row, self.name, None)

    def __repr__(self):
        return f"Column({self.name})"


class Literal(Expr):
    def __init__(self, value):
        self.value = value

    def evaluate(self, row):
        return self.value


def _split_part(value, delimiter, index):
    if value is None:
        return None
    parts = str(value).split(delimiter)
    return parts[index - 1] if 0 < index <= len(parts) else ""


_FUNCTIONS = {
    "lower": lambda value: None if value is None else str(value).lower(),
    "upper": lambda value: None if value is None else str(value).upper(),
    "trim": lambda value: None if value is None else str(value).strip(),
    "coalesce": lambda *values: next((value for value in values if value is not None), None),
    "split_part": _split_part,
}


class FuncExpr(Expr):
    def __init__(self, name, args):
        self.name, self.args = name, args

    def evaluate(self, row):
        if self.name not in _FUNCTIONS:
            raise Unsupported(f"SQL function {self.name} is outside the bounded emulation")
        return _FUNCTIONS[self.name](*(_value(arg, row) for arg in self.args))


class _Func:
    def __getattr__(self, name):
        if name.startswith("__"):
            raise AttributeError(name)
        return lambda *args: FuncExpr(name, args)


class Order:
    def __init__(self, expression, descending, nulls_first=None):
        self.expression, self.descending, self.nulls_first = expression, descending, nulls_first

    def nulls_first(self):
        return Order(self.expression, self.descending, True)

    def nulls_last(self):
        return Order(self.expression, self.descending, False)

    nullsfirst, nullslast = nulls_first, nulls_last


class Query:
    def __init__(self, rows):
        self.rows = list(rows)

    def filter(self, *predicates):
        return Query(row for row in self.rows if all(as_pred(p)(row) for p in predicates))

    where = filter

    def filter_by(self, **values):
        return Query(row for row in self.rows if all(getattr(row, key, None) == value for key, value in values.items()))

    def order_by(self, *orders):
        rows = list(self.rows)
        for order in reversed(orders):
            if isinstance(order, Expr):
                order = Order(order, False)
            nulls_first = order.nulls_first if order.nulls_first is not None else not order.descending
            present = [row for row in rows if order.expression.evaluate(row) is not None]
            missing = [row for row in rows if order.expression.evaluate(row) is None]
            present.sort(key=order.expression.evaluate, reverse=order.descending)
            rows = missing + present if nulls_first else present + missing
        return Query(rows)

    def limit(self, count):
        return Query(self.rows[:count])

    def offset(self, count):
        return Query(self.rows[count:])

    def with_for_update(self, *args, **kwargs):
        return self

    def first(self):
        return self.rows[0] if self.rows else None

    def one_or_none(self):
        if len(self.rows) > 1:
            raise RuntimeError("multiple rows")
        return self.first()

    def one(self):
        if len(self.rows) != 1:
            raise RuntimeError(f"expected one row, found {len(self.rows)}")
        return self.rows[0]

    def all(self):
        return list(self.rows)

    def count(self):
        return len(self.rows)

    def scalar(self):
        return self.first()


class FakeSession:
    def __init__(self, tables):
        self.tables = tables
        self.queries = []

    def query(self, *entities):
        self.queries.append(entities)
        if len(entities) != 1 or entities[0] not in self.tables:
            raise Unsupported("the bounded emulation supports single-model queries of seeded tables")
        return Query(self.tables[entities[0]])

    def add(self, value):
        pass

    def flush(self):
        pass

    def commit(self):
        pass

    def rollback(self):
        pass

    def refresh(self, value):
        pass

    def execute(self, *args, **kwargs):
        raise Unsupported("raw SQL execution is outside the bounded emulation")


class _ModelMeta(type):
    def __getattr__(cls, name):
        if name.startswith("__"):
            raise AttributeError(name)
        return Column(name)


def _model(name):
    return _ModelMeta(name, (), {"__tablename__": name.lower()})


class _Placeholder:
    def __init__(self, name):
        self.name = name

    def __call__(self, *args, **kwargs):
        raise Unsupported(f"sqlalchemy.{self.name} is outside the bounded emulation")

    def __getattr__(self, name):
        if name.startswith("__"):
            raise AttributeError(name)
        return _Placeholder(f"{self.name}.{name}")

    def __getitem__(self, item):
        return self

    def __or__(self, other):
        return self

    __ror__ = __or__


class SQLAlchemyError(Exception):
    pass


def _placeholder_attribute(name):
    if name.startswith("__"):
        raise AttributeError(name)
    return _Placeholder(name)


def _exception_attribute(name):
    if name.startswith("__"):
        raise AttributeError(name)
    return type(name, (SQLAlchemyError,), {})


def _model_attribute(name):
    if name.startswith("__"):
        raise AttributeError(name)
    return _model(name)


def _all(*predicates):
    return Pred(lambda row: all(as_pred(p)(row) for p in predicates))


def _any(*predicates):
    return Pred(lambda row: any(as_pred(p)(row) for p in predicates))


def _install_stubs():
    sqlalchemy = module("sqlalchemy", func=_Func(), and_=_all, or_=_any, not_=lambda p: ~as_pred(p),
                        true=lambda: Pred(lambda row: True), false=lambda: Pred(lambda row: False),
                        literal=Literal, null=lambda: None)
    sqlalchemy.__getattr__ = _placeholder_attribute
    exceptions = {name: type(name, (SQLAlchemyError,), {}) for name in
                  ("IntegrityError", "OperationalError", "NoResultFound", "MultipleResultsFound", "DBAPIError")}
    exc = module("sqlalchemy.exc", SQLAlchemyError=SQLAlchemyError, **exceptions)
    exc.__getattr__ = _exception_attribute
    orm = module("sqlalchemy.orm", Session=type("Session", (), {}))
    orm.__getattr__ = _placeholder_attribute

    class SubscriptionStatus(str, enum.Enum):
        ACTIVE = "active"
        PAST_DUE = "past_due"
        CANCELED = "canceled"
        INCOMPLETE = "incomplete"
        INCOMPLETE_EXPIRED = "incomplete_expired"
        TRIALING = "trialing"
        UNPAID = "unpaid"

    class CreditType(str, enum.Enum):
        PURCHASE = "purchase"
        SUBSCRIPTION = "subscription"
        TRIAL = "trial"
        ADMIN = "admin"
        REFUND = "refund"
        EXPIRY = "expiry"
        CLAWBACK = "clawback"

    module("db")
    models = module("db.models", SubscriptionStatus=SubscriptionStatus, CreditType=CreditType,
                    **{name: _model(name) for name in ("Plan", "User", "UserSubscription", "CreditLog")})
    models.__getattr__ = _model_attribute
    return models


class _Logger:
    def __init__(self):
        self.records = []

    def _record(self, level):
        def log(message, *args, **kwargs):
            self.records.append((level, str(message)))
        return log

    def __getattr__(self, name):
        if name in ("debug", "info", "warning", "warn", "error", "exception", "critical"):
            return self._record(name)
        raise AttributeError(name)


LOGGER = _Logger()


def _exec_package(name, directory):
    init = directory / "__init__.py"
    spec = importlib.util.spec_from_file_location(name, init, submodule_search_locations=[str(directory)])
    value = importlib.util.module_from_spec(spec)
    sys.modules[name] = value
    parent, child = name.rsplit(".", 1)
    setattr(sys.modules[parent], child, value)
    spec.loader.exec_module(value)
    return value


def load_candidate(repo: Path):
    src = repo / "src"
    models = _install_stubs()
    package("agency", src / "agency")
    package("agency.api", src / "agency/api")
    module("agency.api.logging_config", get_logger=lambda name=None: LOGGER,
           configure_logging=lambda *args, **kwargs: None)
    package("agency.api.repositories", src / "agency/api/repositories")
    package("agency.api.services", src / "agency/api/services")
    module("agency.api.repositories.billing_repository", BillingRepository=type("BillingRepository", (), {}),
           apply_grant_with_debt=lambda *args, **kwargs: (_ for _ in ()).throw(Unsupported("apply_grant_with_debt")))
    _exec_package("agency.api.config", src / "agency/api/config")
    import importlib
    repository = importlib.import_module("agency.api.repositories.subscription_repository")
    service = importlib.import_module("agency.api.services.revenuecat_service")
    revenuecat_config = importlib.import_module("agency.api.config.revenuecat")
    return SimpleNamespace(models=models, repository=repository, service=service, config=revenuecat_config)


# ---------------------------------------------------------------------------
# Synthetic fixtures: no real product ids, users or store transactions.
# ---------------------------------------------------------------------------

USER = uuid.UUID("00000000-0000-4000-8000-0000000000a1")
OTHER_SUBSCRIPTION = "test-subscription-other"
SUBSCRIPTION = "test-subscription-1"
PLANS = (
    SimpleNamespace(id="plan_plus", play_product_id="sub_plus", monthly_credits_cents=3900,
                    price_monthly_cents=3900, daily_credit_limit=None),
    SimpleNamespace(id="plan_pro", play_product_id="sub_pro", monthly_credits_cents=9900,
                    price_monthly_cents=9900, daily_credit_limit=None),
)


class Billing:
    def __init__(self):
        self.calls = []

    def __getattr__(self, name):
        if name.startswith("__"):
            raise AttributeError(name)

        def record(*args, **kwargs):
            self.calls.append((name, args, kwargs))
            return {"success": True}
        return record


def repository_for(context, *, rows=(), live_subscription=None):
    base = context.repository.SubscriptionRepository

    class Repository(base):
        """Recording fakes for writes and state; plan lookups stay the candidate's own."""

        def __init__(self):
            super().__init__(FakeSession({context.models.Plan: list(PLANS)}))
            self.rows = {row.provider_subscription_id: row for row in rows}
            self.calls = []

        def _record(self, name, **kwargs):
            self.calls.append((name, kwargs))

        def period_already_granted(self, *, invoice_id=None, provider_event_id=None):
            self._record("period_already_granted", provider_event_id=provider_event_id)
            return False

        def get_subscription_by_provider_id(self, provider, provider_subscription_id):
            return self.rows.get(provider_subscription_id)

        def get_user_subscription(self, user_id):
            return live_subscription

        def upsert_play_subscription(self, **kwargs):
            self._record("upsert_play_subscription", **kwargs)
            return "created"

        def grant_subscription_credits(self, **kwargs):
            self._record("grant_subscription_credits", **kwargs)
            return {"success": True}

        def update_play_subscription_plan(self, provider_subscription_id, plan_id):
            self._record("update_play_subscription_plan", provider_subscription_id=provider_subscription_id,
                         plan_id=plan_id)
            return True

        def update_subscription_status(self, provider_subscription_id, **kwargs):
            self._record("update_subscription_status", provider_subscription_id=provider_subscription_id, **kwargs)
            return provider_subscription_id in self.rows

    return Repository()


def service_for(context, repository):
    billing = Billing()
    return context.service.RevenueCatService(billing_repository=billing, subscription_repository=repository), billing


def event(event_type, **fields):
    value = {"type": event_type, "id": f"evt-{event_type.lower()}-1", "app_user_id": str(USER),
             "original_transaction_id": SUBSCRIPTION, "purchased_at_ms": 1767225600000,
             "expiration_at_ms": 1769904000000, "environment": "PRODUCTION", "store": "PLAY_STORE"}
    value.update(fields)
    return value


def existing_row():
    return SimpleNamespace(user_id=USER, provider_subscription_id=SUBSCRIPTION, status="active", plan_id="plan_plus")


def calls(repository, name):
    return [kwargs for call, kwargs in repository.calls if call == name]


def expect_retry(service, payload, label):
    try:
        service.handle_event(payload)
    except Exception:  # noqa: BLE001 - any raise is the webhook's retry (500) signal
        return
    raise AssertionError(f"{label}: expected a retryable failure, but the event was acknowledged")


# ---------------------------------------------------------------------------
# Checks
# ---------------------------------------------------------------------------


def initial_purchase_with_base_plan_grants_mapped_plan(context):
    repository = repository_for(context)
    service, _ = service_for(context, repository)
    service.handle_event(event("INITIAL_PURCHASE", product_id="sub_plus:monthly"))
    upserts = calls(repository, "upsert_play_subscription")
    assert len(upserts) == 1 and upserts[0]["plan_id"] == "plan_plus", f"subscription row: {upserts}"
    grants = calls(repository, "grant_subscription_credits")
    assert len(grants) == 1 and grants[0]["credits_cents"] == 3900, f"grant: {grants}"


def renewal_with_base_plan_grants_mapped_plan(context):
    repository = repository_for(context, rows=(existing_row(),))
    service, _ = service_for(context, repository)
    service.handle_event(event("RENEWAL", id="evt-renewal-2", product_id="sub_pro:annual"))
    upserts = calls(repository, "upsert_play_subscription")
    assert len(upserts) == 1 and upserts[0]["plan_id"] == "plan_pro", f"subscription row: {upserts}"
    grants = calls(repository, "grant_subscription_credits")
    assert len(grants) == 1 and grants[0]["credits_cents"] == 9900, f"grant: {grants}"


def product_change_with_base_plan_switches_plan(context):
    repository = repository_for(context, rows=(existing_row(),))
    service, _ = service_for(context, repository)
    service.handle_event(event("PRODUCT_CHANGE", product_id="sub_plus:monthly", new_product_id="sub_pro:annual"))
    changes = calls(repository, "update_play_subscription_plan")
    assert changes == [{"provider_subscription_id": SUBSCRIPTION, "plan_id": "plan_pro"}], f"plan change: {changes}"


def missing_row_for_mapped_base_plan_keeps_retrying(context):
    # Out-of-order delivery for a product we DO map: the row may still arrive,
    # so every terminal/grace handler must keep RevenueCat retrying.
    for event_type in ("EXPIRATION", "BILLING_ISSUE", "CANCELLATION"):
        repository = repository_for(context)
        service, _ = service_for(context, repository)
        expect_retry(service, event(event_type, product_id="sub_plus:monthly"), event_type)


def unmapped_base_plan_still_retries(context):
    for payload in (event("INITIAL_PURCHASE", product_id="sub_unknown:monthly"),
                    event("PRODUCT_CHANGE", product_id="sub_plus:monthly", new_product_id="sub_unknown:annual")):
        repository = repository_for(context, rows=(existing_row(),))
        service, _ = service_for(context, repository)
        expect_retry(service, payload, payload["type"])
        assert not calls(repository, "grant_subscription_credits"), "no grant for an unmapped product"
        assert not calls(repository, "update_play_subscription_plan"), "no plan change to an unmapped product"


def unmapped_product_missing_row_is_still_acknowledged(context):
    # Neither a pack nor a plan: no retry can ever produce the row.
    for event_type in ("EXPIRATION", "BILLING_ISSUE", "CANCELLATION"):
        repository = repository_for(context)
        service, _ = service_for(context, repository)
        service.handle_event(event(event_type, product_id="sub_unknown:monthly"))


def plain_subscription_ids_still_map(context):
    repository = repository_for(context)
    service, _ = service_for(context, repository)
    service.handle_event(event("INITIAL_PURCHASE", product_id="sub_plus"))
    grants = calls(repository, "grant_subscription_credits")
    assert len(grants) == 1 and grants[0]["credits_cents"] == 3900, f"grant: {grants}"
    repository = repository_for(context)
    service, _ = service_for(context, repository)
    expect_retry(service, event("EXPIRATION", product_id="sub_plus"), "EXPIRATION for a plain mapped id")


def credit_packs_are_unchanged(context):
    pack, cents = next(iter(context.config.PLAY_PRODUCT_CREDITS_CENTS.items()))
    repository = repository_for(context)
    service, billing = service_for(context, repository)
    service.handle_event(event("NON_RENEWING_PURCHASE", product_id=pack, transaction_id="test-pack-transaction"))
    grants = [kwargs for name, _, kwargs in billing.calls if name == "grant_play_credits_idempotent"]
    assert len(grants) == 1 and grants[0]["amount_cents"] == cents, f"pack grant: {billing.calls}"
    service, _ = service_for(context, repository_for(context))
    expect_retry(service, event("NON_RENEWING_PURCHASE", product_id="pack_unknown"), "unmapped pack")


CHECKS = (
    ("initial_purchase_with_base_plan_grants_mapped_plan", initial_purchase_with_base_plan_grants_mapped_plan),
    ("renewal_with_base_plan_grants_mapped_plan", renewal_with_base_plan_grants_mapped_plan),
    ("product_change_with_base_plan_switches_plan", product_change_with_base_plan_switches_plan),
    ("missing_row_for_mapped_base_plan_keeps_retrying", missing_row_for_mapped_base_plan_keeps_retrying),
    ("unmapped_base_plan_still_retries", unmapped_base_plan_still_retries),
    ("unmapped_product_missing_row_is_still_acknowledged", unmapped_product_missing_row_is_still_acknowledged),
    ("plain_subscription_ids_still_map", plain_subscription_ids_still_map),
    ("credit_packs_are_unchanged", credit_packs_are_unchanged),
)

if __name__ == "__main__":
    sys.exit(main(TASK_ID, load_candidate, CHECKS, ENVIRONMENT))
