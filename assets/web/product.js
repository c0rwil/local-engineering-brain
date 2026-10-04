/* Engram's product model. No network assets or framework runtime. */
(() => {
"use strict";
const root = document.getElementById("product-surface");
const repoSurface = document.getElementById("repository-surface");
const catalog = { records: [], repositories: [] };
let loaded = false, environment = "", capability = "", query = "";
let effectivenessDays = 7, effectivenessGeneration = 0, effectivenessTimer = null;
const h = esc;
const get = id => catalog.records.find(r => r.id === id);
const entity = id => get(id)?.item.value || catalog.repositories.find(r => r.id === id);
const title = id => { const value = entity(id); return value?.name || value?.title || value?.contract || id; };
const href = id => `#product/${encodeURIComponent(id)}`;
const time = ms => new Date(ms).toLocaleString();
const queryApi = request => api("/api/product/query", request);
const mutate = request => api("/api/product/mutate", request);
const kinds = ["product","capability","flow","service","api","queue","database","schema","deployment"];
const relations = ["implements","calls","publishes","consumes","reads","writes","uses_schema","deploys","depends_on"];
const sourceLabel = a => a.path || a.node_id || a.evidence_id || "Source";
const basisLabel = basis => ({declared:"Declared intent",observed:"Recorded observation",inferred:"Inferred — verify"}[basis] || basis);
const freshnessLabel = value => ({verified:"Sources unchanged",needs_review:"Needs review",inferred:"Inferred",unknown:"Sources unknown"}[value] || value);
const badge = r => `<span class="p-badge ${h(r.freshness)}">${h(freshnessLabel(r.freshness))}</span>`;
function matching(r) {
  const v = r.item.value;
  if (environment && v.environment && environment !== v.environment) return false;
  if (query && !(JSON.stringify(v)+" "+(v.source_id?title(v.source_id):"")+" "+(v.target_id?title(v.target_id):"")).toLowerCase().includes(query.toLowerCase())) return false;
  if (capability && ![r.id,v.source_id,v.target_id,v.entity_id].filter(Boolean).some(id=>id===capability||descends(id,capability))) return false;
  return true;
}
function descends(id, parent) {
  const seen = new Set();
  while (id && !seen.has(id)) { seen.add(id); id = get(id)?.item.value.parent_id; if (id === parent) return true; }
  return false;
}
function sourceButtons(anchors=[]) {
  return `<div class="p-sources">${anchors.map(a => `<button type="button" data-source="${h(JSON.stringify(a))}" title="Inspect indexed source and evidence">${h(sourceLabel(a))}${a.revision ? ` @ ${h(a.revision.slice(0,10))}` : ""}</button>`).join("")}</div>`;
}
function recordCard(r) {
  const v = r.item.value;
  return `<article class="p-card"><span class="p-kind">${h(v.kind || r.item.type)} · ${h(basisLabel(v.basis || "declared"))}</span>
    <h3><a href="${href(r.id)}">${h(v.name || v.title || v.contract || r.id)}</a></h3>
    <p class="p-muted">${h((v.description || v.intent || v.curated || "Purpose has not been documented yet.").slice(0,240))}</p>
    <div class="p-flex">${badge(r)}<span class="p-muted">${v.owner ? h(v.owner) : "Owner not recorded"}</span></div></article>`;
}
function toolbar() {
  const environments = [...new Set(catalog.records.map(r=>r.item.value.environment).filter(Boolean))].sort();
  const capabilities = catalog.records.filter(r=>r.item.type==="entity" && r.item.value.kind==="capability");
  return `<div class="p-toolbar">
    <label class="p-field">Find a capability, service or contract<input type="search" id="p-search" value="${h(query)}" placeholder="Search product knowledge"></label>
    <label class="p-field">Environment<select id="p-environment"><option value="">All recorded environments</option>${environments.map(e=>`<option ${e===environment?"selected":""}>${h(e)}</option>`).join("")}</select></label>
    <label class="p-field">Capability<select id="p-capability"><option value="">All capabilities</option>${capabilities.map(r=>`<option value="${h(r.id)}" ${r.id===capability?"selected":""}>${h(r.item.value.name)}</option>`).join("")}</select></label></div>`;
}
function contracts(records) {
  if (!records.length) return `<p class="p-empty">No contracts recorded here. Add a typed dependency with an endpoint, event or schema and its source.</p>`;
  return `<div class="p-table-wrap"><table class="p-contracts"><thead><tr><th scope="col">From</th><th scope="col">Interaction</th><th scope="col">To</th><th scope="col">Basis / scope</th><th scope="col">Source</th></tr></thead><tbody>${records.map(r=>{
    const d=r.item.value;
    return `<tr><td><a class="p-link" href="${href(d.source_id)}">${h(title(d.source_id))}</a></td><td><a class="p-link" href="${href(r.id)}">${h(d.relation.replaceAll("_"," "))}</a><br><code>${h(d.contract)}</code></td><td><a class="p-link" href="${href(d.target_id)}">${h(title(d.target_id))}</a></td><td>${h(basisLabel(d.basis))}<br><span class="p-muted">${h(d.environment || "Environment unspecified")}</span></td><td>${badge(r)}${sourceButtons(d.anchors)}</td></tr>`;
  }).join("")}</tbody></table></div>`;
}
function productPage(flows=false) {
  const entities=catalog.records.filter(r=>r.item.type==="entity" && matching(r));
  const primary=entities.filter(r=>flows?r.item.value.kind==="flow":["product","capability"].includes(r.item.value.kind));
  root.innerHTML=`<div class="eyebrow">${flows?"Follow the work":"Living product"}</div><div class="p-flex"><h1>${flows?"From user action to running system.":"Understand the product behind the code."}</h1><button data-new="entity" data-kind="${flows?"flow":"product"}">${flows?"Add a flow":"Add a product"}</button></div>
    <p class="lede">${flows?"Walk an ordered flow across repositories. Each step can point to its handler, contract, test and operational evidence.":"Connect intent, capabilities, services and contracts. Keep human explanations beside source evidence, with uncertainty visible."}</p>
    ${toolbar()}
    <div class="p-kpis"><div><strong>${catalog.repositories.filter(r=>!r.derived).length}</strong><span>Registered repositories</span></div><div><strong>${entities.filter(r=>r.item.value.kind==="flow").length}</strong><span>Documented flows</span></div><div><strong>${catalog.records.filter(r=>r.freshness==="needs_review").length}</strong><span>Records needing review</span></div></div>
    ${primary.length?`<div class="p-grid">${primary.map(recordCard).join("")}</div>`:`<div class="p-empty">${flows?"Add your first user flow and connect its steps to services or repositories.":"Start with the product’s purpose and constraints, then add a capability and a real flow. Engram will keep declared intent distinct from observations."}</div>`}
    ${!flows?`<div class="p-flex"><h2>Services and resources</h2><button class="quiet" data-new="entity" data-kind="service">Add component</button></div><div class="p-grid">${entities.filter(r=>!["product","capability","flow"].includes(r.item.value.kind)).map(recordCard).join("")}</div>
    <div class="p-flex"><h2>Interaction map</h2><button class="quiet" data-new="dependency">Add contract</button></div>${contracts(catalog.records.filter(r=>r.item.type==="dependency" && matching(r)))}`:""}
    <p class="p-muted" style="margin-top:2rem">Sources unchanged means the cited indexed bytes still match. It does not prove that prose is correct or that a deployed environment is unchanged.</p>`;
  bindFilters();
}
function sectionCard(r) {
  const s=r.item.value;
  return `<article class="p-card"><div class="p-flex"><h3>${h(s.title)}</h3>${badge(r)}</div><p class="p-muted">${h(basisLabel(s.basis))} · revision ${r.version}</p>
    <div class="p-prose">${h(s.curated || "No human explanation recorded.")}</div>
    ${s.generated?`<details><summary>Generated reference · requires review</summary><div class="p-prose p-muted">${h(s.generated)}</div></details>`:""}
    ${sourceButtons(s.anchors)}<div class="p-actions"><button class="quiet" data-edit="${h(r.id)}">Edit / review</button><button class="quiet" data-refresh-section="${h(r.id)}">Refresh source summary</button><button class="quiet" data-history="${h(r.id)}">History</button></div></article>`;
}
function detailPage(id) {
  const r=get(id), repo=catalog.repositories.find(v=>v.id===id);
  if (!r && !repo) { root.innerHTML=`<h1>Record unavailable</h1><p class="p-empty">This record is missing or archived. <a class="p-link" href="#product">Return to the product.</a></p>`;return; }
  const v=r?.item.value || repo;
  const parent=v.parent_id || v.entity_id;
  const sections=catalog.records.filter(x=>x.item.type==="section" && x.item.value.entity_id===id);
  const children=catalog.records.filter(x=>x.item.type==="entity" && x.item.value.parent_id===id);
  const dependencies=catalog.records.filter(x=>x.item.type==="dependency" && [x.item.value.source_id,x.item.value.target_id].includes(id) && (!environment || !x.item.value.environment || x.item.value.environment===environment));
  const deployments=catalog.records.filter(x=>x.item.type==="deployment" && x.item.value.entity_id===id);
  root.innerHTML=`<div class="p-breadcrumb"><a class="p-link" href="#product">Product</a>${parent?` / <a class="p-link" href="${href(parent)}">${h(title(parent))}</a>`:""}</div>
    <div class="eyebrow">${h(v.kind || r?.item.type || "Repository")}</div><h1>${h(v.name || v.title || v.contract || id)}</h1>
    <div class="row">${r?badge(r):`<span class="p-badge">${repo.derived?"origin/main snapshot":"Local index"}</span>`}<button data-bundle="${h(id)}">Start working on this</button>${r?`<button class="quiet" data-edit="${h(id)}">Edit</button><button class="quiet" data-history="${h(id)}">History</button>`:""}${r?.item.type==="entity" && ["service","deployment"].includes(v.kind)?`<button class="quiet" data-new="deployment" data-owner="${h(id)}">Record deployment</button>`:""}${repo?`<button class="quiet" data-repo="${h(repo.project_id)}">Browse indexed code</button>`:""}</div>
    ${v.description || repo || r?.item.type==="entity"?`<p class="lede">${h(v.description || (repo ? `Indexed revision: ${repo.head || "unknown"}` : "Purpose has not been documented yet."))}</p>`:""}
    ${v.intent?`<div class="p-notice"><strong>Product intent</strong><div class="p-prose">${h(v.intent)}</div></div>`:""}
    ${v.owner?`<p class="p-muted">Owner: ${h(v.owner)}</p>`:""}${v.project_id && !repo?`<p><a class="p-link" href="${href("repo:"+v.project_id)}">Open repository</a></p>`:""}
    ${v.constraints?.length?`<h2>Constraints</h2><ul>${v.constraints.map(c=>`<li>${h(c)}</li>`).join("")}</ul>`:""}
    ${v.steps?.length?`<h2>Flow walkthrough</h2><ol class="p-flow">${v.steps.map(step=>`<li><h3>${h(step.title)}</h3><a class="p-link" href="${href(step.entity_id)}">${h(title(step.entity_id))}</a><p class="p-prose">${h(step.detail)}</p>${sourceButtons(step.anchors)}</li>`).join("")}</ol>`:""}
    ${r?.item.type==="dependency"?contracts([r]):""}${r?.item.type==="section"?sectionCard(r):""}
    ${r?.item.type==="section"||r?.item.type==="dependency"?"":sourceButtons(v.anchors)}
    ${children.length?`<h2>Inside this ${h(v.kind)}</h2><div class="p-grid">${children.map(recordCard).join("")}</div>`:""}
    ${r?.item.type==="entity"||repo?`<div class="p-flex"><h2>Living documentation</h2><button class="quiet" data-new="section" data-owner="${h(id)}">Add section</button></div>${sections.length?`<div class="p-grid">${sections.map(sectionCard).join("")}</div>`:`<p class="p-empty">Document a setup path, design decision, invariant or operational lesson. Attach source anchors so Engram can identify which section needs review.</p>`}`:""}
    ${dependencies.length?`<h2>Contracts and dependencies</h2>${contracts(dependencies)}`:""}
    ${deployments.length?`<h2>Recorded deployments</h2><p class="p-muted">These are cited observations, not live environment checks.</p><ul class="p-list">${deployments.map(d=>`<li><strong>${h(d.item.value.environment)}</strong> · <code>${h(d.item.value.revision)}</code> · ${h(time(d.updated_at_ms))}${sourceButtons([{project_id:d.item.value.project_id,evidence_id:d.item.value.evidence_id}])}</li>`).join("")}</ul>`:""}`;
}
function bindFilters() {
  document.getElementById("p-search")?.addEventListener("input",e=>{query=e.target.value;const start=e.target.selectionStart;render();const input=document.getElementById("p-search");input?.focus();input?.setSelectionRange(start,start);});
  document.getElementById("p-environment")?.addEventListener("change",e=>{environment=e.target.value;render();});
  document.getElementById("p-capability")?.addEventListener("change",e=>{capability=e.target.value;render();});
}
function changesPage() {
  root.innerHTML=`<div class="eyebrow">Change with context</div><h1>Trace what a change could affect.</h1><p class="lede">Direct source intersections are separate from possible downstream impacts. Missing contracts stay visible as gaps.</p>
    <form id="p-impact" class="p-card p-form-grid"><label class="p-field">Repository<select name="project_id">${repoOptions()}</select></label><label class="p-field">Environment (optional)<input name="environment" value="${h(environment)}" placeholder="production"></label>
    <label class="p-field wide">Changed paths, one per line<textarea name="paths" placeholder="src/api/routes.rs"></textarea></label>
    <details class="wide"><summary>Or paste a unified diff</summary><label class="p-field">Diff<textarea name="diff" placeholder="Paste git diff output"></textarea></label></details><div class="wide"><button>Analyze impact</button> <span class="p-error" role="status"></span></div></form><div id="p-impact-result" aria-live="polite"></div>
    <h2>Recently changed knowledge</h2><ul class="p-list">${[...catalog.records].sort((a,b)=>b.updated_at_ms-a.updated_at_ms).slice(0,20).map(r=>`<li><a class="p-link" href="${href(r.id)}">${h(r.item.value.name || r.item.value.title || r.item.value.contract || r.id)}</a> ${badge(r)}<span class="p-muted"> · v${r.version} · ${h(time(r.updated_at_ms))}</span> <button class="quiet" data-history="${h(r.id)}">History</button></li>`).join("")}</ul>`;
  document.getElementById("p-impact").addEventListener("submit",async e=>{
    e.preventDefault();await submitting(e.target,async()=>{
      const f=new FormData(e.target);const result=await queryApi({operation:"impact",project_id:f.get("project_id"),paths:lines(f.get("paths")),diff:f.get("diff"),environment:f.get("environment")||null});
      const list=(items,empty)=>items.length?`<ul class="p-list">${items.map(v=>`<li><a class="p-link" href="${href(v.id)}">${h(title(v.id))}</a><p class="p-muted">${h(v.reason)}</p>${v.via.length?`<p class="p-muted">Via ${v.via.map(h).join(" → ")}</p>`:""}</li>`).join("")}</ul>`:`<p class="p-empty">${empty}</p>`;
      document.getElementById("p-impact-result").innerHTML=`<h2>Direct source intersections</h2>${list(result.direct,"No source anchors intersect these files.")}<h2>Possible downstream impact</h2>${list(result.possible,"No downstream impact is known from the recorded graph.")}<h2>Coverage gaps</h2><ul>${result.gaps.map(g=>`<li>${h(g)}</li>`).join("")}</ul>`;
    });
  });
}
async function knowledgePage() {
  root.innerHTML=`<div class="eyebrow">Evidence into memory</div><h1>Keep the lessons worth carrying forward.</h1><p class="lede">Automatic review can validate supported lessons from their evidence. Human review remains available for pending proposals. Accepting a lesson does not record user acceptance of the task.</p><p id="p-knowledge-judge" class="p-notice" role="status">Checking automatic review…</p><div id="p-inbox"><p class="empty">Loading review inbox…</p></div>`;
  api("/api/judge").then(judge=>{
    const notice=document.getElementById("p-knowledge-judge");if(!notice)return;
    notice.innerHTML=`${judge.enabled?`Automatic review is enabled${judge.running?" and running":""}. Supported proposals can be accepted automatically; uncertain claims stay pending.`:"Automatic review is not enabled in this workspace. You can review proposals here or configure a local judge."} <a class="p-link" href="#effectiveness">View review status and decision history</a>.`;
  }).catch(()=>{const notice=document.getElementById("p-knowledge-judge");if(notice)notice.textContent="Automatic review status is unavailable. Manual review remains available.";});
  try {
    const reviews=await queryApi({operation:"reviews"});
    if(!document.getElementById("p-inbox"))return;
    const pending=reviews.filter(r=>r.status==="pending");
    document.getElementById("p-inbox").innerHTML=pending.length?pending.map(r=>`<form class="p-card p-review" data-review="${h(r.id)}"><div class="p-flex"><span class="p-badge">Proposed lesson</span><span class="p-muted">${h(r.task_id)}</span><button type="button" class="quiet" data-signals="${h(r.task_id)}">Outcome signals</button></div>
      <label class="p-field">Reusable claim<textarea name="claim" required>${h(r.claim)}</textarea></label><label class="p-field">Rationale<textarea name="rationale">${h(r.rationale)}</textarea></label>
      ${sourceButtons(r.evidence_ids.map(id=>({project_id:r.project_id,evidence_id:id})))}${r.evidence_ids.length?"":`<p class="p-error">No supporting evidence attached. Acceptance will remain blocked until the proposal has evidence.</p>`}
      ${r.expires_at_ms?`<p class="p-muted">Expires ${h(time(r.expires_at_ms))}</p>`:""}
      ${r.related_memories.length?`<details open><summary>Similar existing memories · check for duplicates or conflicts</summary>${r.related_memories.map(m=>`<p>${h(m.claim)} <span class="p-badge">${m.exact?"Exact duplicate":"Lexical overlap"}</span></p>${m.exact?"":`<label class="p-field"><span class="check"><input type="checkbox" name="contradicts" value="${h(m.id)}">This lesson contradicts that memory; quarantine the old claim</span></label>`}`).join("")}<label class="p-field"><span class="check"><input type="checkbox" name="reviewed_related">I reviewed these related claims</span></label></details>`:""}
      <div class="p-actions"><button type="submit" value="accept">Accept lesson</button><button type="submit" value="reject" class="quiet">Reject</button><span class="p-error" role="status"></span></div></form>`).join(""):`<p class="p-empty">No lessons awaiting review. Agents can finish a task with <code>engram_complete_task</code> to attach verification and propose lessons in one call.</p>`;
    document.querySelectorAll("[data-review]").forEach(form=>form.addEventListener("submit",async e=>{
      e.preventDefault();const r=reviews.find(r=>r.id===form.dataset.review);const decision=e.submitter.value;
      await submitting(form,async()=>{const f=new FormData(form);await api("/api/product/lessons",{operation:"review",id:r.id,expected_updated_at_ms:r.updated_at_ms,decision,claim:f.get("claim"),rationale:f.get("rationale"),contradicts:f.getAll("contradicts"),reviewed_related:f.has("reviewed_related")});await knowledgePage();});
    }));
  }catch(error){const inbox=document.getElementById("p-inbox");if(inbox)inbox.innerHTML=`<p class="p-error">${h(error.message)}</p>`;}
}

const measured = value => typeof value === "number" && Number.isFinite(value);
const amount = value => measured(value) ? value.toLocaleString(undefined,{maximumFractionDigits:0}) : "Unknown";
const dollars = value => measured(value) ? value.toLocaleString(undefined,{style:"currency",currency:"USD",minimumFractionDigits:2,maximumFractionDigits:value>0&&value<0.01?6:4}) : "Unknown";
const usageValue = (usage,field) => field==="cost_usd" ? dollars(usage?.totals?.[field]) : amount(usage?.totals?.[field]);
const coverage = (usage,field) => `${amount(usage?.reported_samples?.[field]??0)} / ${amount(usage?.samples??0)} samples reporting`;
const sourceName = name => ({memory:"Lessons",linked_memory:"Linked lessons",repository_symbol:"Repo symbols",repository_fact:"Repo facts",repository_doc:"Repo documents",symbol:"Symbols",fact:"Facts",document:"Documents"}[name] || name.replaceAll("_"," "));

function usageCard(label,field,usage) {
  return `<article class="p-card p-measure"><h3>${h(label)}</h3><strong>${h(usageValue(usage,field))}</strong><span class="p-muted">${h(coverage(usage,field))}</span></article>`;
}
function contextRows(projects) {
  if(!projects.length)return `<p class="p-empty">No repositories are indexed in this workspace yet. Index a repository to start collecting context and learning coverage.</p>`;
  return `<div class="p-table-wrap" tabindex="0" role="region" aria-label="Repository context and learning coverage"><table class="p-contracts p-effect-table"><thead><tr><th scope="col">Repository</th><th scope="col">Context compilation</th><th scope="col">Selected sources</th><th scope="col">Learning state now</th></tr></thead><tbody>${projects.map(project=>{
    const c=project.contexts, m=project.memories;
    const sources=Object.entries(c.selected_by_source||{});
    return `<tr><th scope="row"><a class="p-link" href="${href("repo:"+project.project_id)}">${h(project.project_name)}</a><small>${h(amount(project.usage?.tasks_with_samples))} tasks with usage</small></th><td><strong>${h(amount(c.runs))} compilations</strong><small>${h(amount(c.runs_with_memories))} included lessons · ${h(amount(c.usage_linked_runs))} linked to usage</small><small>~${h(amount(c.estimated_selected_tokens))} selected tokens · ${h(amount(c.candidate_count))} candidates</small></td><td>${sources.length?sources.map(([name,value])=>`<div>${h(sourceName(name))}: ${h(amount(value.items))}<small>~${h(amount(value.estimated_tokens))} tokens · ${h(amount(value.runs))} compilations</small></div>`).join(""):"No selected items recorded"}</td><td><strong>${h(amount(m.active_validated))} active validated lessons</strong><small>${h(amount(m.pending_reviews))} awaiting review · ${h(amount(m.total))} total memories</small><details><summary>Memory states</summary><ul class="p-compact-list">${Object.entries(m.by_state||{}).map(([state,count])=>`<li>${h(sourceName(state))}: ${h(amount(count))}</li>`).join("")||"<li>No memories recorded</li>"}</ul></details></td></tr>`;
  }).join("")}</tbody></table></div>`;
}
const percent = value => measured(value) ? `${(value*100).toLocaleString(undefined,{maximumFractionDigits:1})}%` : "Unknown";
const kib = value => measured(value) ? `${(value/1024).toLocaleString(undefined,{maximumFractionDigits:1})} KiB` : "Unknown";
const useMode = mode => ({brain:"After delivered context",unattributed:"Before any delivered context"}[mode] || mode);
const contextMode = mode => ({brain:"Brain context",off:"Off context",repo:"Repo context"}[mode] || mode);
const counted = (value,word) => `${amount(value)} ${word}${value===1?"":"s"}`;
function contextUseRows(projects) {
  const rows=projects.filter(project=>project.context_use&&(project.context_use.observed_tool_calls||Object.keys(project.context_use.pointers_by_mode||{}).length));
  if(!rows.length)return `<p class="p-empty">No hook-observed tool calls or delivered contexts in this period. Updated native hooks record which repository files agents read or edit, plus Read sizes; they never store file content.</p>`;
  return `<div class="p-table-wrap" tabindex="0" role="region" aria-label="Delivered context use"><table class="p-contracts p-effect-table"><thead><tr><th scope="col">Repository</th><th scope="col">Pointers touched later</th><th scope="col">Touched files pointed to first</th><th scope="col">Read calls</th></tr></thead><tbody>${rows.map(project=>{
    const u=project.context_use, modes=Object.entries(u.pointers_by_mode||{}), reads=Object.entries(u.reads_by_mode||{});
    const pointers=modes.length?modes.map(([mode,p])=>`<div>${h(contextMode(mode))}: <strong>${h(percent(p.precision))}</strong><small>${h(amount(p.touched_later))} of ${h(amount(Math.max(0,(p.pointers||0)-(p.already_touched||0))))} pointers · ${h(amount(p.already_touched))} already touched at delivery</small><small>${h(amount(p.observed_contexts))} of ${h(amount(p.contexts_with_pointers))} contexts observed · ${h(amount(p.unobserved_pointers))} pointers unobserved${measured(p.first_touch_calls?.median)?` · median ${h(p.first_touch_calls.median.toLocaleString(undefined,{maximumFractionDigits:1}))} calls to first touch`:""}</small></div>`).join(""):"No delivered contexts";
    const coverage=modes.length?modes.map(([mode,p])=>`<div>${h(contextMode(mode))}: <strong>${h(percent(p.coverage))}</strong><small>${h(amount(p.touched_files_pointed))} of ${h(counted(p.touched_files,"file"))} touched after delivery</small></div>`).join(""):"";
    const readCells=reads.length?reads.map(([mode,r])=>`<div>${h(useMode(mode))}: <strong>${h(amount(r.reads))} reads</strong><small>${h(percent(r.ranged_share))} ranged (${h(amount(r.ranged))} of ${h(amount((r.ranged||0)+(r.whole_file||0)))})</small><small>mean ${h(kib(r.mean_result_bytes))} over ${h(amount(r.sized_reads))} sized · ${h(amount(r.limit_hits))} limit hits · ${h(amount(r.failed))} failed</small></div>`).join(""):"No Read calls observed";
    return `<tr><th scope="row"><a class="p-link" href="${href("repo:"+project.project_id)}">${h(project.project_name)}</a><small>${h(counted(u.observed_tool_calls,"observed call"))} · ${h(counted(u.observed_sessions,"agent session"))}</small></th><td>${pointers}</td><td>${coverage}<small>${h(counted(u.touched_files,"file"))} first touched · ${h(amount(u.touched_files_before_context))} before any delivered context</small></td><td>${readCells}</td></tr>`;
  }).join("")}</tbody></table></div>`;
}
function overheadTable(projects) {
  const rows=projects.flatMap(project=>(project.usage_by_source||[]).filter(group=>group.source==="internal_judge").map(group=>({project:project.project_name,...group})));
  if(!rows.length)return `<p class="p-empty">No judge usage measurements recorded in this period. A local model’s cost stays unknown unless its provider reports it.</p>`;
  return `<div class="p-table-wrap" tabindex="0" role="region" aria-label="Automatic review resource usage"><table class="p-contracts p-effect-table"><thead><tr><th scope="col">Repository / model</th><th scope="col">Input tokens</th><th scope="col">Output tokens</th><th scope="col">Reported cost</th></tr></thead><tbody>${rows.map(row=>`<tr><th scope="row">${h(row.project)}<small>${h(row.model||"Model unknown")} · ${h(row.provider||"Provider unknown")}</small></th>${["input_tokens","output_tokens","cost_usd"].map(field=>`<td>${h(usageValue(row.usage,field))}<small>${h(coverage(row.usage,field))}</small></td>`).join("")}</tr>`).join("")}</tbody></table></div>`;
}
const comparisonNames={total_tokens:"Provider total tokens",tool_calls:"Tool calls",agent_wall_seconds:"Agent seconds",cost_usd:"Reported USD estimate",turns:"Agent turns",input_tokens:"Reported input tokens",output_tokens:"Output tokens",cache_read_tokens:"Cache read tokens",cache_write_tokens:"Cache write tokens",processed_input_tokens:"Processed input tokens"};
const comparisonFormat = name => name==="cost_usd"?dollars:name==="agent_wall_seconds"?value=>measured(value)?value.toLocaleString(undefined,{maximumFractionDigits:3}):"Unknown":amount;
function comparisonTable(metrics,pairs,label,primaryOnly=false) {
  const names=Object.entries(comparisonNames).filter(([name],index)=>!primaryOnly||index<4);
  return `<div class="p-table-wrap" tabindex="0" role="region" aria-label="${h(label)}"><table class="p-contracts p-effect-table"><thead><tr><th scope="col">Measurement</th><th scope="col">Off</th><th scope="col">Brain</th><th scope="col">Brain vs off</th><th scope="col">Measured pairs</th></tr></thead><tbody>${names.map(([name,title])=>{
    const metric=metrics?.[name]||{}, format=comparisonFormat(name);
    const change=measured(metric.brain_minus_off_pct)?`${metric.brain_minus_off_pct>0?"+":""}${metric.brain_minus_off_pct.toLocaleString(undefined,{maximumFractionDigits:1})}%`:"Unknown";
    return `<tr><th scope="row">${h(title)}</th><td>${h(format(metric.off_total))}</td><td>${h(format(metric.brain_total))}</td><td>${h(change)}</td><td>${h(amount(metric.measured_pairs))} / ${h(amount(pairs))}</td></tr>`;
  }).join("")}</tbody></table></div>`;
}
function allAttemptTables(attempts,label) {
  if(!attempts)return "";
  const arms=[["off","Off"],["brain","Brain"]];
  const outcomes=`<div class="p-table-wrap" tabindex="0" role="region" aria-label="${h(label)} outcomes"><table class="p-contracts p-effect-table"><thead><tr><th scope="col">Arm</th><th scope="col">Passed</th><th scope="col">Failed</th><th scope="col">Infrastructure / unscorable</th><th scope="col">Unknown</th><th scope="col">Attempts</th></tr></thead><tbody>${arms.map(([arm,title])=>{const data=attempts[arm]||{},counts=data.outcome_counts||{};return `<tr><th scope="row">${title}</th>${["pass","fail","infrastructure_error","unknown"].map(name=>`<td>${h(amount(counts[name]))}</td>`).join("")}<td>${h(amount(data.attempts))}</td></tr>`;}).join("")}</tbody></table></div>`;
  const resources=`<div class="p-table-wrap" tabindex="0" role="region" aria-label="${h(label)} all-attempt resources"><table class="p-contracts p-effect-table"><thead><tr><th scope="col">Observed resource</th><th scope="col">Off</th><th scope="col">Brain</th></tr></thead><tbody>${Object.entries(comparisonNames).slice(0,4).map(([name,title])=>`<tr><th scope="row">${h(title)}</th>${arms.map(([arm])=>{const data=attempts[arm]||{},coverage=data.measurement_coverage?.[name]||{},format=comparisonFormat(name);return `<td>${h(format(data.totals_observed?.[name]))}<small>${h(amount(coverage.available))} / ${h(amount(coverage.total))} attempts reporting${coverage.partial?` · ${h(amount(coverage.partial))} partial`:""}</small></td>`;}).join("")}</tr>`).join("")}</tbody></table></div>`;
  return `<h4>All attempts</h4>${outcomes}${resources}<p class="p-muted">Includes failed, interrupted and unmatched attempts. Reported usage from an interrupted run may be incomplete.</p>`;
}
function comparisonCards(comparisons) {
  if(!comparisons?.length)return `<p class="p-empty">No off/brain trials were recorded in this period. Daily totals alone do not establish token or cost savings.</p>`;
  return comparisons.map(report=>{
    const outcomes=report.pair_outcomes,clean=report.both_clean_pass_pairs;
    const cleanSection=measured(clean)?`<h4>Both runs completed and passed</h4>${clean?`${comparisonTable(report.both_clean_pass_metrics,clean,report.benchmark_id+" clean passing pairs",true)}<details><summary>All measurements for passing pairs</summary>${comparisonTable(report.both_clean_pass_metrics,clean,report.benchmark_id+" passing-pair detail")}</details>`:`<p class="p-empty">No eligible pairs yet. There is no successful-pair resource comparison.</p>`}<p class="p-muted">${h(amount(clean))} eligible pairs. Negative change means lower observed use with Engram. This compares matched successful work; it does not include the cost of failed attempts above.</p>`:"";
    return `<article class="p-card p-comparison"><h3>${h(report.benchmark_id)}</h3><p class="p-muted">${h(amount(report.paired_runs))} matched task/repetition pairs · passed matched trials: off ${h(amount(report.off_successes))} / ${h(amount(report.paired_runs))}, brain ${h(amount(report.brain_successes))} / ${h(amount(report.paired_runs))}${report.excluded_runs?` · ${h(amount(report.excluded_runs))} unmatched or duplicate runs excluded from paired comparisons`:""}</p>${outcomes?`<p class="p-muted">Both pass ${h(amount(outcomes.both_pass))} · only Off passes ${h(amount(outcomes.off_only_pass))} · only Brain passes ${h(amount(outcomes.brain_only_pass))} · both fail ${h(amount(outcomes.both_fail))} · infrastructure or unknown ${h(amount(outcomes.infrastructure_or_unknown))}</p>`:""}${allAttemptTables(report.all_attempts,report.benchmark_id)}${cleanSection}<details ${measured(clean)?"":"open"}><summary>All matched pairs · includes failures and interruptions</summary><p class="p-muted">These deltas describe observed resources, regardless of correctness. A cheaper failed run is not a saving on completed work.</p>${comparisonTable(report.metrics,report.paired_runs,report.benchmark_id+" all matched pairs")}</details><details><summary>Matched controls</summary><dl class="p-controls"><dt>Model</dt><dd>${h(report.model_hash)}</dd><dt>Configuration</dt><dd>${h(report.config_hash)}</dd><dt>Brain snapshot</dt><dd>${h(report.brain_snapshot_hash)}</dd></dl></details></article>`;
  }).join("");
}
function judgmentAudits(judge) {
  if(!judge.audits?.length)return `<p class="p-empty">No automatic decisions recorded yet. When enabled, the judge reviews evidence and records accepted, rejected or deferred decisions here.</p>`;
  return `<ul class="p-list p-audits">${judge.audits.map(audit=>{
    const evaluation=audit.evaluation||{}, verdict=evaluation.verdict;
    return `<li><div class="p-flex"><strong>${h(sourceName(audit.outcome))}</strong><span class="p-muted">${h(time(audit.recorded_at_ms))} · attempt ${h(amount(audit.attempt))}</span></div>${verdict?.claim?`<p>${h(verdict.claim)}</p>`:""}<p class="p-muted">${h(audit.reason)}</p><details><summary>Decision evidence and usage</summary><p class="p-muted">Model ${h(audit.model)} · review ${h(audit.review_id)}${measured(evaluation.duration_ms)?` · ${h(amount(evaluation.duration_ms))} ms`:""}</p><p class="p-muted">Input ${h(amount(evaluation.usage?.input_tokens))} · output ${h(amount(evaluation.usage?.output_tokens))} · reported cost ${h(dollars(evaluation.usage?.reported_cost_usd))}</p>${(verdict?.citations||[]).map(citation=>`<blockquote><p>${h(citation.quote)}</p><cite>Evidence ${h(citation.evidence_id)}</cite></blockquote>`).join("")||`<p class="p-muted">No acceptance citations recorded.</p>`}${evaluation.error?`<p class="p-error">${h(evaluation.error)}</p>`:""}${audit.retry_after_ms?`<p class="p-muted">Eligible for retry after ${h(time(audit.retry_after_ms))}</p>`:""}</details></li>`;
  }).join("")}</ul>`;
}
function judgePanel(judge,error) {
  if(error)return `<h2>Automatic review</h2><p class="p-error" role="alert">Review status unavailable: ${h(error.message)}</p>`;
  return `<section aria-labelledby="p-judge-heading"><div class="p-flex"><h2 id="p-judge-heading">Automatic review</h2><span class="p-badge ${judge.enabled?"verified":""}">${judge.enabled?judge.running?"Running":"Enabled":"Disabled"}</span></div><div class="p-card"><div class="p-flex"><div><h3>${judge.enabled?"Evidence reviewed by the configured judge":"Automatic review is not enabled"}</h3><p class="p-muted">${judge.enabled?`Model: ${h(judge.model)}${judge.provider?` · ${h(judge.provider)}`:""}. Supported claims can become validated lessons; insufficient evidence stays pending.`:"Configure a local model or opt into TypeSafe under [judge] in this workspace’s config. Manual review remains available."}</p></div><button id="p-judge-run" type="button" ${!judge.enabled||judge.running?"disabled":""}>${judge.running?"Review running…":"Run review now"}</button></div><p id="p-judge-result" class="p-muted" role="status"></p><a class="p-link" href="#knowledge">Open lesson review inbox</a></div><h2>Recent decisions</h2><p class="p-muted">Latest ${h(amount(judge.audits?.length??0))} recorded decisions, across all dates. Claims, citations and retry reasons remain inspectable.</p>${judgmentAudits(judge)}</section>`;
}
function effectivenessData(report) {
  const usage=report.usage;
  return `<section aria-labelledby="p-usage-heading"><h2 id="p-usage-heading">Observed resource use</h2><p class="p-muted">${h(time(report.since_ms))} – ${h(time(report.until_ms))} · ${h(amount(usage.samples))} recorded samples · ${h(amount(usage.tasks_with_samples))} tasks with usage</p><div class="p-grid p-measures">${usageCard("Input tokens","input_tokens",usage)}${usageCard("Output tokens","output_tokens",usage)}${usageCard("Tool calls","tool_calls",usage)}${usageCard("Reported cost · USD","cost_usd",usage)}</div><p class="p-notice">Totals include only reported measurements, including automatic review overhead. Each field may cover different samples. Unknown means unreported; it does not mean zero.</p><div class="p-grid p-secondary-measures">${usageCard("Cache read tokens","cache_read_tokens",usage)}${usageCard("Cache write tokens","cache_write_tokens",usage)}</div><p class="p-muted">Cache categories follow the provider’s accounting and are kept separate from input tokens.</p>${usage.samples?"":`<p class="p-empty">No resource measurements yet. Updated coding-agent hooks capture tool events and available usage. Context can still be compiled before its token or cost usage is reported.</p>`}</section><section aria-labelledby="p-context-heading"><h2 id="p-context-heading">Repository context and learning</h2><p class="p-muted">Context figures are estimates from stored compilations. Compilation does not prove the context was delivered or used. Memory state reflects the current database.</p>${contextRows(report.projects)}</section><section aria-labelledby="p-use-heading"><h2 id="p-use-heading">Delivered context use</h2><p class="p-muted">Associations from hook-observed reads and edits in the same agent session, not proof that a pointer caused a read. Files already touched when a context was compiled are excluded from the pointer share. Contexts without a later observed call are unobserved, not unused. Shell reads are not attributed.</p>${contextUseRows(report.projects)}</section><section aria-labelledby="p-overhead-heading"><h2 id="p-overhead-heading">Automatic review overhead</h2><p class="p-muted">These measurements are included in the totals above.</p>${overheadTable(report.projects)}</section><section aria-labelledby="p-comparisons-heading"><h2 id="p-comparisons-heading">Controlled off / brain comparisons</h2><p class="p-muted">Outcomes and resources are grouped by recorded model, configuration and brain snapshot. Paired comparisons require a unique Off/Brain task and repetition. Successful-pair comparisons and all-attempt resource use are shown separately.</p>${comparisonCards(report.controlled_comparisons)}</section><details class="p-report-notes"><summary>Measurement scope and limitations</summary><ul>${(report.limitations||[]).map(note=>`<li>${h(note)}</li>`).join("")}</ul></details>`;
}
async function effectivenessPage(message="") {
  const generation=++effectivenessGeneration;
  clearTimeout(effectivenessTimer);
  const restoreFocus=document.activeElement?.id;
  root.setAttribute("aria-label","Engram effectiveness");
  root.innerHTML=`<div class="eyebrow" id="p-effect-workspace">Workspace · checking…</div><div class="p-flex"><h1>Is the brain earning its context?</h1><div class="p-effect-controls"><label class="p-field">Report period<select id="p-effect-period"><option value="7" ${effectivenessDays===7?"selected":""}>Last 7 days</option><option value="30" ${effectivenessDays===30?"selected":""}>Last 30 days</option></select></label><button type="button" class="quiet" id="p-effect-refresh">Refresh</button></div></div><p class="lede">See what Engram selects, what resource use the agent reports, and how evidence becomes reusable knowledge.</p><p id="p-effect-status" class="p-muted" role="status">${h(message||"Loading usage and review status…")}</p><div id="p-effect-content" aria-busy="true"><p class="p-empty">Loading measured usage…</p></div><div id="p-effect-judge" aria-busy="true"></div>`;
  document.getElementById("p-effect-period").addEventListener("change",event=>{effectivenessDays=Number(event.target.value);effectivenessPage();});
  document.getElementById("p-effect-refresh").addEventListener("click",()=>effectivenessPage());
  if(["p-effect-period","p-effect-refresh"].includes(restoreFocus))document.getElementById(restoreFocus)?.focus({preventScroll:true});
  const until=Date.now(), since=until-effectivenessDays*86400000;
  const [reportResult,judgeResult]=await Promise.allSettled([api("/api/effectiveness",{since_ms:since,until_ms:until}),api("/api/judge")]);
  if(generation!==effectivenessGeneration||!document.getElementById("p-effect-content"))return;
  const content=document.getElementById("p-effect-content"), judgeContent=document.getElementById("p-effect-judge");
  content.setAttribute("aria-busy","false");judgeContent.setAttribute("aria-busy","false");
  content.innerHTML=reportResult.status==="fulfilled"?effectivenessData(reportResult.value):`<p class="p-error" role="alert">Usage report unavailable: ${h(reportResult.reason.message)}</p><p class="p-muted">Use Refresh to retry. Previously recorded measurements remain in the database.</p>`;
  const judge=judgeResult.status==="fulfilled"?judgeResult.value:null;
  document.getElementById("p-effect-workspace").textContent=`Workspace · ${judge?.workspace||"unknown"}`;
  judgeContent.innerHTML=judgePanel(judge,judgeResult.status==="rejected"?judgeResult.reason:null);
  document.getElementById("p-effect-status").textContent=message||(reportResult.status==="fulfilled"?`Updated ${time(Date.now())}`:"Report could not be loaded.");
  document.getElementById("p-judge-run")?.addEventListener("click",async event=>{
    const button=event.currentTarget, status=document.getElementById("p-judge-result");button.disabled=true;status.textContent="Starting a review pass…";
    try{const result=await api("/api/judge/run",{});if(generation!==effectivenessGeneration)return;await effectivenessPage(result.queued?"Review queued. Decisions will appear here as they complete.":"A review is already running.");}
    catch(error){if(generation===effectivenessGeneration){status.className="p-error";status.textContent=error.message;button.disabled=false;}}
  });
  if(judge?.running)pollJudge(generation,6);
}
function pollJudge(generation,remaining) {
  effectivenessTimer=setTimeout(async()=>{
    if(generation!==effectivenessGeneration||location.hash!=="#effectiveness"||document.visibilityState==="hidden")return;
    try {
      const judge=await api("/api/judge");
      if(generation!==effectivenessGeneration||location.hash!=="#effectiveness")return;
      if(!judge.running){effectivenessPage("Review pass finished. Usage and decisions refreshed.");return;}
      if(remaining>1){pollJudge(generation,remaining-1);return;}
      document.getElementById("p-effect-status").textContent="Review is still running. Use Refresh to check for new decisions.";
    } catch {
      if(generation===effectivenessGeneration)document.getElementById("p-effect-status").textContent="Could not refresh review status. Use Refresh to retry.";
    }
  },10000);
}
function lines(value) { return String(value||"").split("\n").map(v=>v.trim()).filter(Boolean); }
function repoOptions(selected="",empty=false) {
  return `${empty?'<option value="">Not linked to a repository</option>':""}${catalog.repositories.map(r=>`<option value="${h(r.project_id)}" ${r.project_id===selected?"selected":""}>${h(r.name)} · ${r.derived?"origin/main":"local"}</option>`).join("")}`;
}
function entityOptions(selected="",empty=false) {
  return `${empty?'<option value="">None</option>':""}${catalog.records.filter(r=>r.item.type==="entity").map(r=>`<option value="${h(r.id)}" ${r.id===selected?"selected":""}>${h(r.item.value.name)} · ${h(r.item.value.kind)}</option>`).join("")}${catalog.repositories.map(r=>`<option value="${h(r.id)}" ${r.id===selected?"selected":""}>${h(r.name)} · repository</option>`).join("")}`;
}
function dialog(title,content) {
  const d=document.createElement("dialog");d.className="p-dialog";
  const id="pd-"+crypto.randomUUID();d.setAttribute("aria-labelledby",id);
  d.innerHTML=`<div class="p-dialog-head"><h2 id="${id}">${h(title)}</h2><form method="dialog"><button class="quiet">Close</button></form></div><div class="p-dialog-body">${content}</div>`;
  document.body.append(d);d.addEventListener("close",()=>d.remove());d.showModal();return d;
}
const field=(label,name,value="",type="input",wide=false)=>`<label class="p-field ${wide?"wide":""}">${h(label)}${type==="textarea"?`<textarea name="${name}">${h(value)}</textarea>`:`<input name="${name}" value="${h(value)}">`}</label>`;
function anchorEditor(anchors) {
  return `<details class="wide" open><summary>Source anchors</summary><p class="p-muted">Choose a repository and add a file path, symbol ID or evidence ID. Use the indexed revision for the selected view.</p><div data-anchors>${anchors.map(anchorRow).join("")}</div><button class="quiet" type="button" data-add-anchor>Add source</button></details>`;
}
function anchorRow(a={}) {
  const kind=a.evidence_id?"evidence_id":a.node_id?"node_id":"path";
  return `<div class="p-step-edit p-form-grid" data-anchor><label class="p-field">Repository<select data-anchor-project>${repoOptions(a.project_id)}</select></label><label class="p-field">Source kind<select data-anchor-kind>${["path","node_id","evidence_id"].map(k=>`<option value="${k}" ${kind===k?"selected":""}>${k==="path"?"File path":k==="node_id"?"Symbol ID":"Evidence ID"}</option>`).join("")}</select></label><label class="p-field">Source<input data-anchor-value value="${h(a[kind]||"")}"></label><label class="p-field">Revision assertion (optional)<input data-anchor-revision value="${h(a.revision||"")}"></label><button class="quiet" type="button" data-remove-row>Remove source</button></div>`;
}
function readAnchors(container) {
  return [...container.querySelectorAll(":scope > [data-anchor]")].map(row=>({project_id:row.querySelector("[data-anchor-project]").value,[row.querySelector("[data-anchor-kind]").value]:row.querySelector("[data-anchor-value]").value.trim(),revision:row.querySelector("[data-anchor-revision]").value.trim()||null}));
}
function stepRow(step={}) {
  return `<div class="p-step-edit" data-step><label class="p-field">Step title<input data-step-title value="${h(step.title||"")}" required></label><label class="p-field">Component or repository<select data-step-entity>${entityOptions(step.entity_id)}</select></label><label class="p-field">What happens<textarea data-step-detail>${h(step.detail||"")}</textarea></label>${anchorEditor(step.anchors||[])}<div class="row"><button class="quiet" type="button" data-step-up>Move up</button><button class="quiet" type="button" data-remove-row>Remove step</button></div></div>`;
}
function editRecord(type,id,kind,owner) {
  const record=get(id), v=record?.item.value || {}, editing=Boolean(record);
  const basis=`<label class="p-field">Basis<select name="basis">${["declared","observed","inferred"].map(b=>`<option value="${b}" ${(v.basis||"declared")===b?"selected":""}>${h(basisLabel(b))}</option>`).join("")}</select></label>`;
  let fields="";
  if(type==="entity") fields=`<label class="p-field">Kind<select name="kind">${kinds.map(k=>`<option ${k===(v.kind||kind||"service")?"selected":""}>${k}</option>`).join("")}</select></label>${field("Name","name",v.name)}
    <label class="p-field">Parent<select name="parent_id">${entityOptions(v.parent_id,true)}</select></label><label class="p-field">Repository<select name="project_id">${repoOptions(v.project_id,true)}</select></label>${field("Owner","owner",v.owner)}${field("Environment (optional)","environment",v.environment)}${basis}${field("Purpose / current description","description",v.description,"textarea",true)}${field("Product intent / desired behavior","intent",v.intent,"textarea",true)}${field("Constraints (one per line)","constraints",(v.constraints||[]).join("\n"),"textarea",true)}<details class="wide" ${(v.kind||kind)==="flow"?"open":""}><summary>Ordered flow steps (for flows)</summary><div data-steps>${(v.steps||[]).map(stepRow).join("")}</div><button class="quiet" type="button" data-add-step>Add step</button></details>${anchorEditor(v.anchors||[])}`;
  if(type==="dependency")fields=`<label class="p-field">From<select name="source_id">${entityOptions(v.source_id)}</select></label><label class="p-field">To<select name="target_id">${entityOptions(v.target_id)}</select></label><label class="p-field">Relationship<select name="relation">${relations.map(r=>`<option ${r===v.relation?"selected":""}>${r}</option>`).join("")}</select></label>${field("Endpoint, event or schema","contract",v.contract)}${basis}${field("Environment (optional)","environment",v.environment)}${field("Contract details / compatibility","description",v.description,"textarea",true)}${anchorEditor(v.anchors||[])}`;
  if(type==="section")fields=`<label class="p-field">Belongs to<select name="entity_id">${entityOptions(v.entity_id||owner)}</select></label>${field("Section title","title",v.title)}${basis}${field("Human explanation","curated",v.curated,"textarea",true)}${field("Generated reference (never evidence)","generated",v.generated,"textarea",true)}${anchorEditor(v.anchors||[])}<label class="p-field wide"><span class="check"><input type="checkbox" name="reviewed">I checked this explanation against the current source anchors</span></label>`;
  if(type==="deployment")fields=`<label class="p-field">Service<select name="entity_id">${entityOptions(v.entity_id||owner)}</select></label><label class="p-field">Repository<select name="project_id">${repoOptions(v.project_id)}</select></label>${field("Environment","environment",v.environment)}${field("Observed revision","revision",v.revision)}${field("Deployment evidence ID","evidence_id",v.evidence_id,"input",true)}<p class="p-muted wide">Requires a runtime observation, trace or metric for this exact project and revision, attached to a successful task.</p>`;
  if(type==="guide") { dialog("Generated guide",`<p class="p-muted">Regenerate this derived guide with engram architect. Curated explanations belong in documentation sections.</p><pre>${h(v.content)}</pre>`);return; }
  const d=dialog(editing?"Edit knowledge":"Add knowledge",`<form id="p-editor" class="p-form-grid">${fields}<div class="wide row"><button>Save ${h(type)}</button>${editing?`<button type="button" class="quiet" data-archive="${h(id)}">Archive</button>`:""}<span class="p-error" role="status"></span></div></form>`);
  d.addEventListener("click",e=>{
    const b=e.target.closest("button");if(!b)return;
    if(b.hasAttribute("data-add-anchor"))b.parentElement.querySelector("[data-anchors]").insertAdjacentHTML("beforeend",anchorRow());
    if(b.hasAttribute("data-add-step"))d.querySelector("[data-steps]").insertAdjacentHTML("beforeend",stepRow());
    if(b.hasAttribute("data-remove-row"))b.closest(".p-step-edit").remove();
    if(b.hasAttribute("data-step-up")){const row=b.closest("[data-step]");if(row.previousElementSibling)row.parentElement.insertBefore(row,row.previousElementSibling);}
  });
  d.querySelector("form#p-editor").addEventListener("submit",async e=>{
    e.preventDefault();await submitting(e.target,async()=>{
      const f=new FormData(e.target),value=Object.fromEntries(f);
      for(const key of ["parent_id","project_id","environment"])if(key in value && !value[key])value[key]=null;
      if(type==="entity") {value.constraints=lines(value.constraints);value.steps=[...d.querySelectorAll("[data-step]")].map(row=>({entity_id:row.querySelector("[data-step-entity]").value,title:row.querySelector("[data-step-title]").value,detail:row.querySelector("[data-step-detail]").value,anchors:readAnchors(row.querySelector("[data-anchors]"))}));}
      if(type!=="deployment")value.anchors=readAnchors(e.target.querySelector(":scope > details > [data-anchors]"));
      if(type==="section")value.reviewed=f.has("reviewed");
      const saved=await mutate({operation:"save",id:id||null,expected_version:record?.version||0,item:{type,value}});
      d.close();await load();location.hash="product/"+encodeURIComponent(saved.id);render();
    });
  });
}
async function submitting(form,action) {
  const buttons=[...form.querySelectorAll("button")],error=form.querySelector(".p-error");if(error)error.textContent="";
  buttons.forEach(b=>b.disabled=true);
  try {await action();}catch(e){if(error)error.textContent=e.message;else throw e;}finally{buttons.forEach(b=>b.disabled=false);}
}
async function inspectSource(anchor) {
  const d=dialog("Source evidence",`<p class="empty">Loading canonical source…</p>`);
  try {
    const data=await queryApi({operation:"source",anchor});const s=data.stamp;
    d.querySelector(".p-dialog-body").innerHTML=`<p><strong>${h(sourceLabel(anchor))}</strong></p><p class="p-muted">Project ${h(anchor.project_id)} · revision ${h(s.revision||"unknown")}<br>Digest ${h(s.digest||"not indexed")}</p>${data.evidence?`<p>${h(data.evidence.kind)} · ${h(data.evidence.strength)} · exit ${h(data.evidence.exit_code??"not recorded")}</p><pre>${h(data.evidence.command||"")}</pre><pre>${h(data.evidence.artifact_text)}</pre>`:`<ul class="p-list">${(data.symbols||[]).map(n=>`<li><strong>${h(n.label)}</strong> <span class="p-badge">${h(n.kind)}</span><p class="p-muted">${h(n.path)} · bytes ${h(n.start_byte)}–${h(n.end_byte)}<br>${h(n.id)}</p></li>`).join("")||"No indexed symbols for this source."}</ul>`}`;
  }catch(error){d.querySelector(".p-dialog-body").innerHTML=`<p class="p-error">${h(error.message)}</p>`;}
}
function bundleDialog(id) {
  const d=dialog("Start working on this",`<p class="p-muted">Build the same cited context available to your coding agent. The copied text includes its limitations.</p><form class="p-form-grid">${field("Task","task","","textarea",true)}${field("Token budget","token_budget","2000")}${field("Environment (optional)","environment",environment)}<div class="wide"><button>Build task context</button><span class="p-error" role="status"></span></div></form><div data-bundle-output></div>`);
  d.querySelector("form.p-form-grid").addEventListener("submit",async e=>{e.preventDefault();await submitting(e.target,async()=>{
    const f=new FormData(e.target);const bundle=await queryApi({operation:"bundle",entity_id:id,task:f.get("task"),token_budget:Number(f.get("token_budget")),environment:f.get("environment")||null});
    const out=d.querySelector("[data-bundle-output]");out.innerHTML=`<p class="p-muted">${bundle.tokens} / ${bundle.budget} estimated tokens · ${bundle.omitted_count} omitted records</p><button type="button" data-copy>Copy context</button><span class="p-muted" role="status"></span><pre>${h(bundle.text)}</pre><details><summary>Coverage gaps</summary><ul>${bundle.gaps.map(g=>`<li>${h(g)}</li>`).join("")}</ul></details>`;
    out.querySelector("[data-copy]").addEventListener("click",async()=>{try{await navigator.clipboard.writeText(bundle.text);out.querySelector('[role="status"]').textContent=" Copied";}catch{out.querySelector('[role="status"]').textContent=" Select and copy the context below.";}});
  });});
}
async function historyDialog(id) {
  const d=dialog("Revision history",`<p class="empty">Loading revisions…</p>`);
  try {const versions=await queryApi({operation:"history",id});d.querySelector(".p-dialog-body").innerHTML=`<p class="p-muted">Immutable saved revisions. Historical source stamps describe what was recorded then.</p>${versions.map(r=>`<details><summary>Version ${r.version} · ${h(time(r.updated_at_ms))}${r.archived?" · archived":""}</summary><pre>${h(JSON.stringify({item:r.item,sources:r.stamps},null,2))}</pre></details>`).join("")}`;}catch(error){d.querySelector(".p-dialog-body").innerHTML=`<p class="p-error">${h(error.message)}</p>`;}
}
document.addEventListener("click",async e=>{
  const b=e.target.closest("button");if(!b)return;
  if(!["new","edit","history","source","bundle","signals","repo","refreshSection","archive"].some(key=>b.dataset[key]))return;
  try {
    if(b.dataset.new)editRecord(b.dataset.new,null,b.dataset.kind,b.dataset.owner);
    if(b.dataset.edit){const r=get(b.dataset.edit);if(r)editRecord(r.item.type,r.id);}
    if(b.dataset.history)await historyDialog(b.dataset.history);
    if(b.dataset.source)await inspectSource(JSON.parse(b.dataset.source));
    if(b.dataset.bundle)bundleDialog(b.dataset.bundle);
    if(b.dataset.signals){const signals=await queryApi({operation:"task_signals",task_id:b.dataset.signals});dialog("Outcome signals",`<p class="p-muted">Verification, commits and merges are distinct from explicit user acceptance.</p><pre>${h(JSON.stringify(signals,null,2))}</pre>`);}
    if(b.dataset.repo){const project=state.data?.projects.find(p=>p.id===b.dataset.repo||p.origin_view?.project_id===b.dataset.repo);if(project)openServiceModal(project.id);}
    if(b.dataset.refreshSection){b.disabled=true;const r=get(b.dataset.refreshSection);await mutate({operation:"refresh_section",id:r.id,expected_version:r.version});await load();render();}
    if(b.dataset.archive){const r=get(b.dataset.archive);await mutate({operation:"archive",id:r.id,expected_version:r.version});b.closest("dialog").close();await load();location.hash="product";render();}
  } catch(error){dialog("Could not save this change",`<p class="p-error">${h(error.message)}</p><p class="p-muted">Your existing saved knowledge is still available. Reload if another editor changed the record.</p>`);}finally{b.disabled=false;}
});
function render() {
  const [page,id]=location.hash.slice(1).split("/");
  const current=["product","repositories","flows","changes","knowledge","effectiveness"].includes(page)?page:"product";
  if(current!=="effectiveness"){clearTimeout(effectivenessTimer);effectivenessGeneration++;root.setAttribute("aria-label","Living product");}
  document.querySelectorAll(".product-nav a").forEach(a=>{if(a.hash===`#${current}`)a.setAttribute("aria-current","page");else a.removeAttribute("aria-current");});
  root.hidden=current==="repositories";repoSurface.hidden=current!=="repositories";
  document.querySelector("header .view-toggle").hidden=current!=="repositories";
  document.querySelector("header .search").hidden=current!=="repositories";
  if(current==="repositories")return;
  if(current==="effectiveness"){effectivenessPage();return;}
  if(!loaded)return;
  if(id && current==="product"){detailPage(decodeURIComponent(id));return;}
  if(current==="changes"){changesPage();return;}
  if(current==="knowledge"){knowledgePage();return;}
  productPage(current==="flows");
}
async function load() {const data=await queryApi({operation:"snapshot"});Object.assign(catalog,data);loaded=true;}
window.addEventListener("hashchange",render);
render();load().then(render).catch(error=>{if(location.hash==="#effectiveness")return;root.innerHTML=`<h1>Product knowledge is unavailable</h1><p class="p-error">${h(error.message)}</p><p class="p-muted">Start the updated Engram daemon, then reload.</p>`;});
})();
