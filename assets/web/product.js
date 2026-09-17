/* Engram's product model. No network assets or framework runtime. */
(() => {
"use strict";
const root = document.getElementById("product-surface");
const repoSurface = document.getElementById("repository-surface");
const catalog = { records: [], repositories: [] };
let loaded = false, environment = "", capability = "", query = "";
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
  root.innerHTML=`<div class="eyebrow">Evidence into memory</div><h1>Keep the lessons worth carrying forward.</h1><p class="lede">Review the claim and its actual verification. Accepting a lesson validates that memory; it does not record user acceptance of the task.</p><div id="p-inbox"><p class="empty">Loading review inbox…</p></div>`;
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
  const current=["product","repositories","flows","changes","knowledge"].includes(page)?page:"product";
  document.querySelectorAll(".product-nav a").forEach(a=>{if(a.hash===`#${current}`)a.setAttribute("aria-current","page");else a.removeAttribute("aria-current");});
  root.hidden=current==="repositories";repoSurface.hidden=current!=="repositories";
  document.querySelector("header .view-toggle").hidden=current!=="repositories";
  document.querySelector("header .search").hidden=current!=="repositories";
  if(current==="repositories")return;
  if(!loaded)return;
  if(id && current==="product"){detailPage(decodeURIComponent(id));return;}
  if(current==="changes"){changesPage();return;}
  if(current==="knowledge"){knowledgePage();return;}
  productPage(current==="flows");
}
async function load() {const data=await queryApi({operation:"snapshot"});Object.assign(catalog,data);loaded=true;}
window.addEventListener("hashchange",render);
render();load().then(render).catch(error=>{root.innerHTML=`<h1>Product knowledge is unavailable</h1><p class="p-error">${h(error.message)}</p><p class="p-muted">Start the updated Engram daemon, then reload.</p>`;});
})();
