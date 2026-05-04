"""
Inventory rebalancing service.

A warehouse needs to redistribute stock across regional fulfillment centers
based on projected demand. This module computes the rebalancing plan.
"""

from __future__ import annotations

import hindsight


def project_demand(historical: list[int], days_ahead: int) -> float:
    """Exponentially weighted moving average over recent history."""
    if not historical:
        return 0.0

    alpha = 0.3
    smoothed = historical[0]
    for value in historical[1:]:
        smoothed = alpha * value + (1 - alpha) * smoothed

    daily_rate = smoothed / 7.0
    return daily_rate * days_ahead


def compute_safety_stock(daily_demand: float, lead_time_days: int, service_level: float) -> int:
    """Safety stock = daily demand * lead time * service level multiplier."""
    multipliers = {0.90: 1.28, 0.95: 1.65, 0.99: 2.33}
    multiplier = multipliers.get(service_level, 1.65)

    raw = daily_demand * lead_time_days * multiplier
    return int(raw)


def determine_target_stock(center: dict, projected_demand: float) -> int:
    """Target = projected demand + safety stock, capped by capacity."""
    safety = compute_safety_stock(
        daily_demand=projected_demand / center["forecast_days"],
        lead_time_days=center["lead_time_days"],
        service_level=center.get("service_level", 0.95),
    )

    target = int(projected_demand) + safety
    capacity = center["capacity"]

    if target > capacity:
        return capacity
    return target


def compute_transfer(current: int, target: int, in_transit: int) -> int:
    """How many units to ship to reach target, accounting for in-transit stock."""
    needed = target - current - in_transit
    if needed < 0:
        return 0
    return needed


def find_donor_centers(centers: list[dict], min_surplus: int) -> list[dict]:
    """Centers with significant surplus that could donate stock."""
    donors = []
    for center in centers:
        surplus = center["current_stock"] - center["target_stock"]
        if surplus > min_surplus:
            donors.append({
                "center_id": center["id"],
                "available": surplus,
                "region": center["region"],
            })
    return donors


def match_transfers(needs: list[dict], donors: list[dict]) -> list[dict]:
    """Pair centers needing stock with donor centers, preferring same region."""
    transfers = []

    for need in needs:
        remaining = need["amount"]
        if remaining == 0:
            continue

        same_region_donors = [d for d in donors if d["region"] == need["region"]]
        other_donors = [d for d in donors if d["region"] != need["region"]]

        for donor in same_region_donors + other_donors:
            if remaining <= 0:
                break
            if donor["available"] <= 0:
                continue

            ship = min(remaining, donor["available"])
            transfers.append({
                "from": donor["center_id"],
                "to": need["center_id"],
                "units": ship,
                "cross_region": donor["region"] != need["region"],
            })
            donor["available"] -= ship
            remaining -= ship

    return transfers


@hindsight.record
def rebalance_inventory(centers: list[dict], horizon_days: int = 14) -> dict:
    """Compute a rebalancing plan across all fulfillment centers."""
    hindsight.note("rebalance start", center_count=len(centers), horizon=horizon_days)

    plan = {
        "transfers": [],
        "total_units_moved": 0,
        "centers_processed": 0,
        "centers_at_capacity": 0,
        "centers_understocked": 0,
    }

    needs = []

    for center in centers:
        history = center.get("demand_history", [])
        projected = project_demand(history, horizon_days)
        target = determine_target_stock(center, projected)
        center["target_stock"] = target

        if target == center["capacity"]:
            plan["centers_at_capacity"] += 1

        transfer_needed = compute_transfer(
            current=center["current_stock"],
            target=target,
            in_transit=center.get("in_transit", 0),
        )

        if transfer_needed > 0:
            plan["centers_understocked"] += 1
            needs.append({
                "center_id": center["id"],
                "amount": transfer_needed,
                "region": center["region"],
            })

        plan["centers_processed"] += 1

    donors = find_donor_centers(centers, min_surplus=50)
    transfers = match_transfers(needs, donors)

    plan["transfers"] = transfers
    plan["total_units_moved"] = sum(t["units"] for t in transfers)

    hindsight.note(
        "rebalance complete",
        transfer_count=len(transfers),
        units_moved=plan["total_units_moved"],
        unmet_needs=sum(1 for n in needs if n["amount"] > 0),
    )

    return plan


if __name__ == "__main__":
    centers = [
        {
            "id": "fc-east-01",
            "region": "east",
            "current_stock": 850,
            "capacity": 2000,
            "in_transit": 0,
            "lead_time_days": 5,
            "forecast_days": 7,
            "demand_history": [120, 135, 128, 140, 132, 138, 145, 150],
        },
        {
            "id": "fc-east-02",
            "region": "east",
            "current_stock": 1400,
            "capacity": 1800,
            "in_transit": 50,
            "lead_time_days": 3,
            "forecast_days": 7,
            "demand_history": [80, 75, 82, 78, 85, 80, 79],
        },
        {
            "id": "fc-west-01",
            "region": "west",
            "current_stock": 600,
            "capacity": 1500,
            "in_transit": 100,
            "lead_time_days": 4,
            "forecast_days": 7,
            "demand_history": [200, 195, 210, 205, 215, 220, 218, 225],
            "service_level": 0.99,
        },
        {
            "id": "fc-west-02",
            "region": "west",
            "current_stock": 1100,
            "capacity": 1200,
            "in_transit": 0,
            "lead_time_days": 6,
            "forecast_days": 7,
            "demand_history": [90, 95, 88, 92, 100, 105, 98],
        },
        {
            "id": "fc-central-01",
            "region": "central",
            "current_stock": 300,
            "capacity": 1000,
            "in_transit": 0,
            "lead_time_days": 7,
            "forecast_days": 7,
            "demand_history": [50, 55, 48, 52, 60, 58, 55],
        },
        {
            "id": "fc-central-02",
            "region": "central",
            "current_stock": 950,
            "capacity": 1100,
            "in_transit": 0,
            "lead_time_days": 5,
            "forecast_days": 7,
            "demand_history": [],
        },
    ]

    plan = rebalance_inventory(centers, horizon_days=14)
    print(f"Plan: {len(plan['transfers'])} transfers, {plan['total_units_moved']} units moved")
    print(f"At capacity: {plan['centers_at_capacity']}, understocked: {plan['centers_understocked']}")
    for t in plan["transfers"]:
        print(f"  {t['from']} → {t['to']}: {t['units']} units (cross-region: {t['cross_region']})")