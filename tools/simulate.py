"""How long a generated build workflow takes, from measured build times.

Simulates the run: a job starts when all its `needs` are done and a runner is
free (jobs in file order), and takes job_overhead plus its recipes' times.
    python simulate.py <workflow.yaml> <timings.json> [runners ...]
"""

import heapq
import json
import statistics
import sys

import yaml

wf = yaml.safe_load(open(sys.argv[1]))
t = json.load(open(sys.argv[2]))
times = t["recipes"]
default = t.get("default") or statistics.median(times.values())
overhead = t.get("job_overhead") or 30.0

jobs = {}
for key, job in wf["jobs"].items():
    recipes = None
    for step in job["steps"]:
        recipes = recipes or (step.get("env") or {}).get("CURRENT_RECIPES") or (step.get("with") or {}).get("recipes")
    dur = overhead + sum(times.get(r, default) for r in str(recipes).split())
    needs = job.get("needs") or []
    jobs[key] = (dur, [needs] if isinstance(needs, str) else needs)
order = list(jobs)


def run(runners):
    done, running, t_now, finished = {}, [], 0.0, set()
    pending = list(order)
    while pending or running:
        started = True
        while started and len(running) < runners:
            started = False
            for k in pending:
                if all(n in finished for n in jobs[k][1]):
                    pending.remove(k)
                    heapq.heappush(running, (t_now + jobs[k][0], k))
                    started = True
                    break
        t_now, k = heapq.heappop(running)
        finished.add(k)
        done[k] = t_now
    return max(done.values())


busy = sum(d for d, _ in jobs.values())
print(f"{sys.argv[1]}: {len(jobs)} jobs, {busy / 3600:.1f} runner-hours")
for r in [int(x) for x in sys.argv[3:]] or [20, 10000]:
    print(f"  {r:>5} runners: {run(r) / 3600:.1f} h")
