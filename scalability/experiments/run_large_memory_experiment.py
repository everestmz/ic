#!/usr/bin/env python3
"""
P0 Experiment 2: Memory under load.

Purpose: Measure memory performance for a canister that has a high memory demand.

For request type t in { Query, Update }

  Topology: 13 node subnet, 1 machine NNS
  Deploy memory test canister on subnet
  Increase memory footprint of query + update calls over time
  Run workload generators on 13 machines at 50% max_capacity
  Measure and determine:
    Requests / second
    Error rate
    Request latency
    Memory performance
    AMD uProf L2 (page faults and cache misses on various levels)
    AMD uProf memory (memory throughput demand of the system)
    Metrics from Execution (see grafana dashboard)
    Flamegraphs (e.g. SIGSEGV issue was showing up there or time spent in kernel)
    Workload generator metrics

Suggested success criteria (Queries):
Maximum number of queries not be below yyy queries per second with less than 20% failure and a maximum latency of 5000ms

Suggested success criteria (Updates):
Maximum number of updates not be below xxx updates per second with less than 20% failure and a maximum latency of 10000ms
"""
import codecs
import itertools
import json
import os
import sys
import time

import gflags

sys.path.append(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from common import misc  # noqa
from common import workload_experiment  # noqa

CANISTER = "memory-test-canister"

FLAGS = gflags.FLAGS
gflags.DEFINE_integer("payload_size", 5000000, "Payload size to pass to memory test canister")
gflags.DEFINE_integer("num_canisters", 1, "Number of canisters to install and benchmark against.")


class LargeMemoryExperiment(workload_experiment.WorkloadExperiment):
    """Logic for experiment 2."""

    def __init__(self):
        """Construct experiment 2."""
        super().__init__(num_workload_gen=1)
        for _ in range(FLAGS.num_canisters):
            self.install_canister(self.target_nodes[0], CANISTER)

    def run_experiment_internal(self, config):
        """Run workload generator with the load specified in config."""
        duration = config["duration"] if "duration" in config else 300
        load = config["load_total"]
        call_method = config["call_method"]
        t_start = int(time.time())
        if len(self.machines) != 1:
            raise Exception("Expected number of workload generator machines to be exactly 1")
        # The workload generator can only target a single canister.
        # If we want to target num_canister canisters, we hence need num_canister workload generators.
        # They can all be on the same machine.
        num_canisters_installed = len(list(itertools.chain.from_iterable([i for _, i in self.canister_ids.items()])))
        assert num_canisters_installed == FLAGS.num_canisters
        r = self.run_workload_generator(
            [self.machines[0] for _ in range(FLAGS.num_canisters)],
            self.target_nodes,
            load,
            payload=codecs.encode(json.dumps({"size": config["payload_size"]}).encode("utf-8"), "hex"),
            method="Update" if self.use_updates else "Query",
            call_method=call_method,
            duration=duration,
        )
        self.last_duration = int(time.time()) - t_start

        t_median = max(r.t_median) if len(r.t_median) > 0 else None
        print(f"🚀  ... failure rate for {load} rps was {r.failure_rate} median latency is {t_median}")
        return r

    def run_iterations(self, iterations=None):
        """Run heavy memory experiment in defined iterations."""
        failure_rate = 0.0
        t_median = 0.0
        run = True
        rps = []

        rps_max = 0
        rps_max_in = None

        num_succ_per_iteration = []

        iteration = 0

        while run:

            load_total = iterations[iteration]
            iteration += 1

            rps.append(load_total)
            print(f"🚀 Testing with load: {load_total} and updates={self.use_updates}")

            evaluated_summaries = super().run_experiment(
                {
                    "load_total": load_total,
                    "payload_size": FLAGS.payload_size,
                    "duration": FLAGS.iter_duration,
                    "call_method": "update_copy" if self.use_updates else "query_copy",
                }
            )

            avg_succ_rate = evaluated_summaries.get_avg_success_rate(FLAGS.iter_duration)
            (
                failure_rate,
                t_median_list,
                t_average_list,
                t_max_list,
                t_min_list,
                _,
                total_requests,
                num_success,
                num_failure,
            ) = evaluated_summaries.convert_tuple()

            t_median, t_average, t_max, t_min, p99 = evaluated_summaries.get_latencies()

            print(f"🚀  ... failure rate for {load_total} rps was {failure_rate} median latency is {t_median}")

            if (
                failure_rate < workload_experiment.ALLOWABLE_FAILURE_RATE
                and t_median < workload_experiment.ALLOWABLE_LATENCY
            ):
                if avg_succ_rate > rps_max:
                    rps_max = avg_succ_rate
                    rps_max_in = load_total

            # Check termination condition
            run = misc.evaluate_stop_latency_failure_iter(
                t_median,
                workload_experiment.STOP_T_MEDIAN,
                failure_rate,
                workload_experiment.STOP_FAILURE_RATE,
                iteration,
                len(iterations),
            )

            # Write summary file in each iteration including experiment specific data.
            rtype = "update_copy" if self.use_updates else "query_copy"
            state = "running" if run else "done"
            self.write_summary_file(
                "run_large_memory_experiment",
                {
                    "is_update": FLAGS.use_updates,
                    "rps": rps,
                    "rps_max": rps_max,
                    "rps_max_in": rps_max_in,
                    "num_succ_per_iteration": num_succ_per_iteration,
                    "target_duration": FLAGS.iter_duration,
                    "success_rate": (num_success / total_requests) * 100,
                    "failure_rate": failure_rate * 100,
                    "failure_rate_color": "green" if failure_rate < 0.01 else "red",
                    "t_median": t_median,
                    "t_average": t_average,
                    "t_max": t_max,
                    "t_min": t_min,
                    "target_load": load_total,
                },
                rps,
                "requests / s",
                rtype=rtype,
                state=state,
            )

            print(f"🚀  ... maximum capacity so far is {rps_max}")

        self.end_experiment()
        return (failure_rate, t_median, t_average, t_max, t_min, total_requests, num_success, num_failure, rps_max)


if __name__ == "__main__":
    misc.parse_command_line_args()
    exp = LargeMemoryExperiment()
    iterations = [FLAGS.target_rps]
    exp.run_iterations(iterations)
