from __future__ import annotations

import hindsight

import time

@hindsight.record
def slow_iteration_demo():
    results = []
    for i in range(100):
        if i == 47:
            time.sleep(0.5)
        results.append(i * 2)
    return results

if __name__ == "__main__":
    slow_iteration_demo()