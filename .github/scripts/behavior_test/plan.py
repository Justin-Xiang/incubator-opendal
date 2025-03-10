#!/usr/bin/env python3
# Licensed to the Apache Software Foundation (ASF) under one
# or more contributor license agreements.  See the NOTICE file
# distributed with this work for additional information
# regarding copyright ownership.  The ASF licenses this file
# to you under the Apache License, Version 2.0 (the
# "License"); you may not use this file except in compliance
# with the License.  You may obtain a copy of the License at
#
#   http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing,
# software distributed under the License is distributed on an
# "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
# KIND, either express or implied.  See the License for the
# specific language governing permissions and limitations
# under the License.

import json
import os
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

# The path for current script.
SCRIPT_PATH = Path(__file__).parent.absolute()
# The path for `.github` dir.
GITHUB_DIR = SCRIPT_PATH.parent.parent
# The project dir for opendal.
PROJECT_DIR = GITHUB_DIR.parent


def provided_cases() -> list[dict[str, str]]:
    root_dir = f"{GITHUB_DIR}/services"

    cases = [
        {
            "service": service,
            "setup": setup,
            "feature": "services-{}".format(service.replace("_", "-")),
            "content": Path(
                os.path.join(root_dir, service, setup, "action.yml")
            ).read_text(),
        }
        for service in os.listdir(root_dir)
        for setup in os.listdir(os.path.join(root_dir, service))
    ]

    # Check if this workflow needs to read secrets.
    #
    # We will check if pattern `op://services` exist in content.
    if not os.getenv("GITHUB_HAS_SECRETS") == "true":
        cases[:] = [v for v in cases if "op://services" not in v["content"]]

    # Remove content from cases.
    cases = [
        {
            "setup": v["setup"],
            "service": v["service"],
            "feature": v["feature"],
        }
        for v in cases
    ]

    # Make sure the order is stable.
    sorted_cases = sorted(cases, key=lambda x: (x["service"], x["setup"]))
    return sorted_cases


@dataclass
class Hint:
    # Is core affected?
    core: bool = field(default=False, init=False)
    # Is binding java affected?
    binding_java: bool = field(default=False, init=False)
    # Is binding python affected?
    binding_python: bool = field(default=False, init=False)
    # Is binding nodejs affected?
    binding_nodejs: bool = field(default=False, init=False)

    # Should we run all services test?
    all_service: bool = field(default=False, init=False)
    # affected services set.
    services: set = field(default_factory=set, init=False)


def calculate_hint(changed_files: list[str]) -> Hint:
    hint = Hint()

    # Remove all files that ends with `.md`
    changed_files = [f for f in changed_files if not f.endswith(".md")]

    service_pattern = r"core/src/services/([^/]+)/"
    test_pattern = r".github/services/([^/]+)/"

    for p in changed_files:
        # workflow behavior tests affected
        if p == ".github/workflows/behavior_test.yml":
            hint.core = True
            hint.binding_java = True
            hint.binding_python = True
            hint.binding_nodejs = True
            hint.all_service = True
        if p == ".github/workflows/behavior_test_core.yml":
            hint.core = True
            hint.all_service = True
        if p == ".github/workflows/behavior_test_binding_java.yml":
            hint.binding_java = True
            hint.all_service = True
        if p == ".github/workflows/behavior_test_binding_python.yml":
            hint.binding_python = True
            hint.all_service = True
        if p == ".github/workflows/behavior_test_binding_nodejs.yml":
            hint.binding_nodejs = True
            hint.all_service = True

        # core affected
        if (
            p.startswith("core/")
            and not p.startswith("core/benches/")
            and not p.startswith("core/edge/")
            and not p.startswith("core/fuzz/")
            and not p.startswith("core/src/services/")
        ):
            hint.core = True
            hint.binding_java = True
            hint.binding_python = True
            hint.binding_nodejs = True
            hint.all_service = True

        # binding java affected.
        if p.startswith("bindings/java/"):
            hint.binding_java = True
            hint.all_service = True

        # binding python affected.
        if p.startswith("bindings/python/"):
            hint.binding_python = True
            hint.all_service = True

        # binding nodejs affected.
        if p.startswith("bindings/nodejs/"):
            hint.binding_nodejs = True
            hint.all_service = True

        # core service affected
        match = re.search(service_pattern, p)
        if match:
            hint.core = True
            hint.binding_java = True
            hint.binding_python = True
            hint.binding_nodejs = True
            hint.services.add(match.group(1))

        # core test affected
        match = re.search(test_pattern, p)
        if match:
            hint.core = True
            hint.binding_java = True
            hint.binding_python = True
            hint.binding_nodejs = True
            hint.services.add(match.group(1))
    return hint


# unique_cases is used to only one setup for each service.
#
# We need this because we have multiple setup for each service and they have already been
# tested by `core` workflow. So we can only test unique setup for each service for bindings.
#
# We make sure that we return the first setup for each service in alphabet order.
def unique_cases(cases):
    ucases = {}
    for case in cases:
        service = case["service"]
        if service not in ucases:
            ucases[service] = case

    # Convert the dictionary back to a list if needed
    return list(ucases.values())


def generate_core_cases(
    cases: list[dict[str, str]], hint: Hint
) -> list[dict[str, str]]:
    # Always run all tests if it is a push event.
    if os.getenv("GITHUB_IS_PUSH") == "true":
        return cases

    # Return empty if core is False
    if not hint.core:
        return []

    # Return all services if all_service is True
    if hint.all_service:
        return cases

    # Filter all cases that not shown un in changed files
    cases = [v for v in cases if v["service"] in hint.services]
    return cases


def generate_language_binding_cases(
    cases: list[dict[str, str]], hint: Hint, language: str
) -> list[dict[str, str]]:
    cases = unique_cases(cases)

    if os.getenv("GITHUB_IS_PUSH") == "true":
        return cases

    # Return empty if core is False
    if not getattr(hint, f"binding_{language}"):
        return []

    # Return all services if all_service is True
    if hint.all_service:
        return cases

    # Filter all cases that not shown un in changed files
    cases = [v for v in cases if v["service"] in hint.services]
    return cases


def plan(changed_files: list[str]) -> dict[str, Any]:
    cases = provided_cases()
    hint = calculate_hint(changed_files)

    core_cases = generate_core_cases(cases, hint)
    binding_java_cases = generate_language_binding_cases(cases, hint, "java")
    binding_python_cases = generate_language_binding_cases(cases, hint, "python")
    binding_nodejs_cases = generate_language_binding_cases(cases, hint, "nodejs")

    jobs = {
        "components": {
            "core": False,
            "binding_java": False,
            "binding_python": False,
            "binding_nodejs": False,
        },
        "core": [],
        "binding_java": [],
        "binding_python": [],
        "binding_nodejs": [],
    }

    if len(core_cases) > 0:
        jobs["components"]["core"] = True
        jobs["core"].append({"os": "ubuntu-latest", "cases": core_cases})

        # fs is the only services need to run upon windows, let's hard code it here.
        if "fs" in [v["service"] for v in core_cases]:
            jobs["core"].append(
                {
                    "os": "windows-latest",
                    "cases": [
                        {"setup": "local_fs", "service": "fs", "feature": "services-fs"}
                    ],
                }
            )

    if len(binding_java_cases) > 0:
        jobs["components"]["binding_java"] = True
        jobs["binding_java"].append(
            {"os": "ubuntu-latest", "cases": binding_java_cases}
        )
    if len(binding_python_cases) > 0:
        jobs["components"]["binding_python"] = True
        jobs["binding_python"].append(
            {"os": "ubuntu-latest", "cases": binding_python_cases}
        )
    if len(binding_nodejs_cases) > 0:
        jobs["components"]["binding_nodejs"] = True
        jobs["binding_nodejs"].append(
            {"os": "ubuntu-latest", "cases": binding_nodejs_cases}
        )

    return jobs


if __name__ == "__main__":
    changed_files = sys.argv[1:]
    result = plan(changed_files)
    print(json.dumps(result))
