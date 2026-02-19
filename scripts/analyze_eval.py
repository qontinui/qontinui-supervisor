import json, sys

with open(sys.argv[1]) as f:
    data = json.load(f)

results = data['results']
scored = [r for r in results if r['overall_score'] is not None]
errors = [r for r in results if r.get('generation_error') or r.get('scoring_error')]

print(f"Total: {len(results)}, Scored: {len(scored)}, Errors: {len(errors)}")
print(f"Overall avg: {data['avg_overall_score']:.2f}")
print()

dims = ['structural_correctness', 'command_accuracy', 'phase_flow_logic', 'step_completeness', 'prompt_quality', 'determinism']
print("=== Dimension Averages ===")
for d in dims:
    vals = [r[d] for r in scored if r[d] is not None]
    avg = sum(vals)/len(vals) if vals else 0
    print(f"  {d:25s} {avg:.2f}  (n={len(vals)})")

print()
print("=== Bottom 15 Scores ===")
scored.sort(key=lambda r: r['overall_score'])
for r in scored[:15]:
    print(f"  {r['overall_score']:.2f}  {r['test_prompt_id']}")

print()
print("=== Top 10 Scores ===")
scored.sort(key=lambda r: -r['overall_score'])
for r in scored[:10]:
    print(f"  {r['overall_score']:.2f}  {r['test_prompt_id']}")

print()
print("=== Scores by Category ===")
cats = {}
for r in scored:
    pid = r['test_prompt_id']
    if pid.startswith('gt-'): cat = 'curated(gt)'
    elif pid.startswith('uib-'): cat = 'ui_bridge'
    elif pid.startswith('api-'): cat = 'api'
    elif pid.startswith('check-') or pid.startswith('checkgroup-'): cat = 'checks'
    elif pid.startswith('shell-'): cat = 'shell'
    elif pid.startswith('test-'): cat = 'tests'
    elif pid.startswith('script-'): cat = 'script'
    elif pid.startswith('gui-'): cat = 'gui'
    elif pid.startswith('spec-'): cat = 'spec'
    elif pid.startswith('gate-'): cat = 'gate'
    elif pid.startswith('logwatch-'): cat = 'logwatch'
    elif pid.startswith('screenshot-'): cat = 'screenshot'
    elif pid.startswith('prompt-'): cat = 'ai_prompt'
    elif pid.startswith('mcp-'): cat = 'mcp'
    elif pid.startswith('awas-'): cat = 'awas'
    elif pid.startswith('macro-'): cat = 'macro'
    elif pid.startswith('state-'): cat = 'state'
    elif pid.startswith('wfref-'): cat = 'wfref'
    elif pid.startswith('errresolved-'): cat = 'errresolved'
    elif pid.startswith('artifact-'): cat = 'artifact'
    elif pid.startswith('lint-') or pid.startswith('typecheck-') or pid.startswith('full-stack'): cat = 'checks'
    elif pid.startswith('web-page') or pid.startswith('e2e-'): cat = 'e2e'
    elif pid.startswith('ai-session') or pid.startswith('playwright-'): cat = 'other'
    else: cat = 'other'
    cats.setdefault(cat, []).append(r['overall_score'])

for cat, scores in sorted(cats.items(), key=lambda x: sum(x[1])/len(x[1])):
    avg = sum(scores)/len(scores)
    print(f"  {cat:15s}  avg={avg:.2f}  n={len(scores):2d}  min={min(scores):.1f}  max={max(scores):.1f}")

print()
print("=== Weak Dimensions (score <= 3) ===")
for d in dims:
    weak = [(r['test_prompt_id'], r[d]) for r in scored if r[d] is not None and r[d] <= 3]
    if weak:
        print(f"  {d}: {len(weak)} prompts")
        for pid, score in weak[:5]:
            print(f"    score={score}  {pid}")
        if len(weak) > 5:
            print(f"    ... and {len(weak)-5} more")

print()
print("=== Dimension Correlation (which dimensions tend to be weak together) ===")
for i, d1 in enumerate(dims):
    for d2 in dims[i+1:]:
        both_weak = sum(1 for r in scored if r[d1] is not None and r[d2] is not None and r[d1] <= 3 and r[d2] <= 3)
        if both_weak >= 3:
            print(f"  {d1} + {d2}: {both_weak} prompts weak in both")

if errors:
    print(f"\n=== Errors ({len(errors)}) ===")
    for r in errors[:10]:
        err = r.get('generation_error') or r.get('scoring_error') or 'unknown'
        print(f"  {r['test_prompt_id']}: {err[:100]}")
    if len(errors) > 10:
        print(f"  ... and {len(errors)-10} more")
