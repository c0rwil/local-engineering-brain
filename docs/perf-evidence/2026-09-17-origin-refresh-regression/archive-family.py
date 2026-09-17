"""Archive the final matrix; reject missing cells, leakage or zero brain context."""
from pathlib import Path
import collections,hashlib,json,shutil,sys
root=Path(__file__).parent
repo=Path('/Users/corwil/Documents/ChatGPT/engram')
family=sys.argv[1]
assert family in ('hooks','local')
assert (root/f'{family}.complete').exists(), 'matrix incomplete'
rows=[json.loads(l) for l in (root/family/'runs.jsonl').read_text().splitlines()]
seeds={1,2} if family=='hooks' else {2,3}
assert len(rows)==20 and len({(x['task_id'],x['mode'],x['seed']) for x in rows})==20
assert len({x['task_id'] for x in rows})==5
for task in {x['task_id'] for x in rows}:
 assert {(x['mode'],x['seed']) for x in rows if x['task_id']==task}=={(m,s) for m in ('off','brain') for s in seeds}
tele=json.loads((root/f'{family}-telemetry.json').read_text());summary=[]
for r in rows:
 arm=Path(r['artifact_path']);t=tele[arm.name]
 assert t['completed'], 'telemetry not final: '+arm.name
 assert not t['mcp_calls'] and not t['brain_cli_call_ids'],arm
 assert 'injected_bytes' in t,arm
 assert t['injected_bytes']==0 if r['mode']=='off' else t['injected_bytes']>0,arm
 files={p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in arm.iterdir() if p.is_file()}
 summary.append({k:r[k] for k in ('task_id','mode','seed','success','timed_out','wall_ms','agent_exit_code','validation_exit_code')}|{'context_bytes':t['injected_bytes'],'context_tokens':t['injected_tokens'],'mcp_calls':0,'engram_cli_calls':0,'tool_calls':t['tool_count'],'artifact_sha256':files})
output=repo/'docs/perf-evidence/2026-09-17-origin-refresh-regression';output.mkdir(exist_ok=True)
for source,dest in [(root/family/'runs.jsonl',f'{family}-runs.jsonl'),(root/family/'comparison.json',f'{family}-comparison.json'),(root/f'{family}.yaml',f'{family}.yaml'),(root/f'{family}-telemetry.json',f'{family}-telemetry.json'),(root/'input-hashes.json','input-hashes.json'),(root/'runtime.json','runtime.json')]:shutil.copyfile(source,output/dest)
(output/f'{family}-audit.json').write_text(json.dumps({'implementation_commit':'3b65868','runtime':json.loads((root/'runtime.json').read_text()),'family':family,'cells':summary},indent=2)+'\n')
if family=='local':shutil.copyfile(root/'local-warmup.log',output/'local-warmup.txt')
print(json.dumps({'family':family,'successes':{m:sum(r['success'] for r in rows if r['mode']==m) for m in ('off','brain')},'mean_wall_ms':{m:sum(r['wall_ms'] for r in rows if r['mode']==m)/10 for m in ('off','brain')},'total_bytes':{m:sum(r['context_bytes'] for r in summary if r['mode']==m) for m in ('off','brain')}},indent=2))
