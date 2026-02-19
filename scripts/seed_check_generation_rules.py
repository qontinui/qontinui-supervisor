"""
Seed generation rules for check-group workflow patterns.

These rules guide the AI builder to produce clean check-group workflows
matching the ground truth structure: only check + gate steps in verification,
one prompt step in agentic, no setup/completion steps.

Usage: python3 scripts/seed_check_generation_rules.py
"""
import json
import urllib.request

BASE = "http://localhost:9876"


def api_get(path):
    req = urllib.request.Request(f"{BASE}{path}", method="GET")
    try:
        with urllib.request.urlopen(req) as resp:
            return json.loads(resp.read())
    except Exception as e:
        return {"error": str(e)}


def api_post(path, data):
    body = json.dumps(data).encode()
    req = urllib.request.Request(
        f"{BASE}{path}",
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req) as resp:
            return json.loads(resp.read())
    except Exception as e:
        return {"error": str(e)}


def get_next_rule_number():
    """Query existing rules to find the next available rule_number."""
    result = api_get("/generation-rules")
    if "error" in result:
        print(f"Warning: Could not fetch existing rules: {result['error']}")
        return 100  # Start at 100 to avoid conflicts
    rules = result.get("data", result) if isinstance(result, dict) else result
    if isinstance(rules, list) and rules:
        max_num = max(r.get("rule_number", 0) for r in rules)
        return max_num + 1
    return 1


RULES = [
    {
        "agent": "schema_context",
        "section": "important_rules",
        "title": "Check-group workflow structure",
        "content": (
            "When the prompt asks to 'run code quality checks', 'lint and format', "
            "or 'check' on a repository: "
            "setup_steps MUST be [] (empty). "
            "completion_steps MUST be [] (empty). "
            "verification_steps MUST contain ONLY 'check' type steps plus exactly one "
            "'gate' step at the end. "
            "agentic_steps MUST contain exactly ONE 'prompt' step. "
            "Do NOT add api_request, spec, test, shell_command, or sdk steps."
        ),
    },
    {
        "agent": "schema_context",
        "section": "important_rules",
        "title": "Check step tool field required",
        "content": (
            "Every 'check' type step MUST include a 'tool' field with the tool "
            "identifier. Examples: tool='ruff', tool='eslint', tool='cargo', "
            "tool='prettier', tool='tsc', tool='mypy'."
        ),
    },
    {
        "agent": "schema_context",
        "section": "verification_quality",
        "title": "Gate must reference all check steps",
        "content": (
            "For check-group workflows: the gate step's required_steps array MUST "
            "list every check step ID. If there are N check steps, the gate must "
            "reference all N IDs."
        ),
    },
]


if __name__ == "__main__":
    print("=== Seeding check-group generation rules ===")
    next_num = get_next_rule_number()
    print(f"Starting at rule_number={next_num}")

    success = 0
    fail = 0
    for i, rule in enumerate(RULES):
        rule_data = {
            "rule_number": next_num + i,
            "agent": rule["agent"],
            "section": rule["section"],
            "title": rule["title"],
            "content": rule["content"],
            "provenance": "seed",
        }
        result = api_post("/generation-rules", rule_data)
        if "error" in result:
            print(f"  FAIL rule {next_num + i}: {result['error']}")
            fail += 1
        else:
            print(f"  OK   rule {next_num + i} ({rule['section']})")
            success += 1

    print(f"\n=== Done: {success} added, {fail} failed ===")
