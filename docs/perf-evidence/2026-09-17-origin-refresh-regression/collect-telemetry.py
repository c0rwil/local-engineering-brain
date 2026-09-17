import json,pathlib,sqlite3,time,re,os
ROOT=pathlib.Path('/private/tmp/engram-refresh-regression-20260917')
ISOLATIONS={f:pathlib.Path('/private/tmp/egrefresh17-'+f) for f in ('hooks','local')}
telemetry={f:(json.loads((ROOT/f'{f}-telemetry.json').read_text()) if (ROOT/f'{f}-telemetry.json').exists() else {}) for f in ISOLATIONS}
for _ in range(14400):
 for family,iso in ISOLATIONS.items():
  live=list(pathlib.Path('/private/tmp/egrefresh17-'+family).glob('engram-eval-*'))
  if live: iso=max(live,key=lambda p:p.stat().st_mtime)
  if not (ROOT/family).exists():continue
  records={}
  jf=ROOT/family/'runs.jsonl'
  for l in jf.read_text().splitlines() if jf.exists() else []:
   try:r=json.loads(l);records[r['ordinal']]=r
   except ValueError:pass
  for arm in sorted((ROOT/family).glob('[0-9]*')):
   ordinal=int(arm.name.split('-')[0]);state=iso/f's{ordinal:06}'; db=state/'brain.sqlite'
   info=telemetry[family].setdefault(arm.name,{})
   transcript=arm/'agent.stdout';mcp=[];cli=[];tool_count=0
   for line in transcript.read_text(errors='replace').splitlines() if transcript.exists() else []:
    try:event=json.loads(line)
    except ValueError:continue
    blocks=event.get('message',{}).get('content',[])
    for block in blocks if isinstance(blocks,list) else []:
     if block.get('type')!='tool_use':continue
     tool_count+=1;name=block.get('name','')
     if name.startswith('mcp__'):mcp.append(name)
     command=block.get('input',{}).get('command','')
     if name=='Bash' and re.search(r'(?<![-\w])(?:engram|leb)\s+(?:recall|brief|context|memory|project|task|hook|mcp)\b',command):cli.append(block['id'])
   info.update(tool_count=tool_count,mcp_calls=mcp,brain_cli_call_ids=cli,completed=ordinal in records)
   if db.exists():
    try:
     c=sqlite3.connect(db.as_uri()+'?mode=ro',uri=True,timeout=2)
     rows=c.execute("SELECT mode,length(CAST(stable_prefix AS BLOB))+length(CAST(dynamic_context AS BLOB)),selected_tokens FROM context_runs WHERE created_at_ms>=1789661925152").fetchall()
     info.update(context_runs=len(rows),injected_bytes=sum(r[1] for r in rows),injected_tokens=sum(r[2] for r in rows),context_modes=sorted(set(r[0] for r in rows)))
     c.close()
    except sqlite3.Error:pass
  out=ROOT/f'{family}-telemetry.json';temp=out.with_suffix('.tmp');temp.write_text(json.dumps(telemetry[family],indent=2)+'\n');temp.replace(out)
 if all((ROOT/f'{f}.complete').exists() for f in ISOLATIONS):break
 time.sleep(3)
