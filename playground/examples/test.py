"""
A small data pipeline with realistic-feeling behavior.

Decorate the entry point, run it, and ask Claude things like:
  - "What did process_orders do? Are there any bugs?"
  - "Which order took the longest to process and why?"
  - "Did any orders get skipped? If so, which ones and why?"
  - "Walk me through what happened to order 4."
  - "Is there any code path that never got exercised?"
"""

import hindsight


def calculate_discount(price: float, customer_tier: str) -> float:
    """
    Compute a discount based on customer tier.
    Bug: 'platinum' is misspelled as 'platnium' in one branch.
    """
    if customer_tier == "gold":
        return price * 0.10
    elif customer_tier == "platnium":  # typo — never matches real "platinum"
        return price * 0.20
    elif customer_tier == "vip":
        return price * 0.30
    else:
        return 0.0


def is_valid_order(order: dict) -> bool:
    """Check if an order has the fields we need."""
    if "id" not in order:
        return False
    if "items" not in order or not order["items"]:
        return False
    if "customer" not in order:
        return False
    return True


def compute_order_total(order: dict) -> float:
    """
    Sum item prices, apply discount, return total.
    Bug: when an item is missing 'price', it's treated as 0 silently.
    """
    subtotal = 0.0
    for item in order["items"]:
        price = item.get("price", 0.0)  # silent fallback hides missing data
        quantity = item.get("quantity", 1)
        subtotal += price * quantity

    customer_tier = order["customer"].get("tier", "none")
    discount = calculate_discount(subtotal, customer_tier)
    total = subtotal - discount

    return total


def categorize_total(total: float) -> str:
    """Bucket the order into a size category."""
    if total < 50:
        return "small"
    elif total < 200:
        return "medium"
    elif total <= 1000:
        return "large"
    else:
        return "huge"


@hindsight.record
def process_orders(orders: list[dict]) -> dict:
    """
    Process a batch of orders, returning a summary.
    Skips invalid orders silently. This is intentional but worth noticing.
    """
    hindsight.note("processing batch", count=len(orders))

    results = {
        "processed": 0,
        "skipped": 0,
        "by_category": {"small": 0, "medium": 0, "large": 0, "huge": 0},
        "total_revenue": 0.0,
    }

    for order in orders:
        if not is_valid_order(order):
            results["skipped"] += 1
            continue

        total = compute_order_total(order)
        category = categorize_total(total)

        results["processed"] += 1
        results["by_category"][category] += 1
        results["total_revenue"] += total

    hindsight.note(
        "batch complete",
        processed=results["processed"],
        skipped=results["skipped"],
        revenue=results["total_revenue"],
    )

    return results


if __name__ == "__main__":
    # A realistic batch with several edge cases mixed in.
    orders = [
        # Normal gold customer
        {
            "id": 1,
            "customer": {"name": "Alice", "tier": "gold"},
            "items": [
                {"name": "widget", "price": 25.0, "quantity": 2},
                {"name": "gadget", "price": 15.0, "quantity": 1},
            ],
        },
        # Customer with the typo'd tier — discount silently doesn't apply
        {
            "id": 2,
            "customer": {"name": "Bob", "tier": "platinum"},
            "items": [
                {"name": "premium-thing", "price": 500.0, "quantity": 1},
            ],
        },
        # Item missing price field — silently treated as 0
        {
            "id": 3,
            "customer": {"name": "Carol", "tier": "vip"},
            "items": [
                {"name": "expensive", "price": 200.0, "quantity": 1},
                {"name": "mystery", "quantity": 3},  # no price
            ],
        },
        # Missing customer field entirely — skipped
        {
            "id": 4,
            "items": [
                {"name": "orphan", "price": 50.0, "quantity": 1},
            ],
        },
        # Empty items list — skipped
        {
            "id": 5,
            "customer": {"name": "Dave", "tier": "gold"},
            "items": [],
        },
        # Large order
        {
            "id": 6,
            "customer": {"name": "Eve", "tier": "vip"},
            "items": [
                {"name": "luxury", "price": 800.0, "quantity": 2},
            ],
        },
        # Tiny order, no tier
        {
            "id": 7,
            "customer": {"name": "Frank"},
            "items": [
                {"name": "trinket", "price": 5.0, "quantity": 1},
            ],
        },
    ]

    summary = process_orders(orders)
    print("Summary:", summary)