const $ = id => document.getElementById(id);
const escape = value => String(value ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const pretty = value => JSON.stringify(value, null, 2);
const code = value => `<pre>${escape(typeof value === 'string' ? value : pretty(value))}</pre>`;
let modelState=null, catalogSignature='', runActive=false;
let bootstrap, current = null, runData = null, activeTab = 'request', selectedCall = 0, selectedEvent = null, caseId, pollBusy = false, historySignature = '', feedSignature = '';
const names = {'doc-copy':'文档复制','doc-extract':'文档字段提取','order-ready':'订单 · 有库存','order-hold':'订单 · 无库存'};
async function api(path, data) {
  const options = data === undefined ? {} : {method:'POST', headers:{'Content-Type':'application/json','X-Jingwei-Token':bootstrap.token},body:JSON.stringify(data)};
  const response = await fetch(path, options);
  const body = await response.json();
  if (!response.ok) throw new Error(body.error || `HTTP ${response.status}`);
  return body;
}
function showError(error) { $('error').textContent=error?.message || String(error); $('error').classList.remove('hidden'); }
function loadCase(c) {
  caseId=c.id; $('prompt').value=c.prompt;
  for(const key of ['initial','expected','writable']) $(key).value=pretty(c[key]);
}
function welcome() {
  $('feed').innerHTML='<div class="welcome"><span class="welcome-icon">⌘</span><h2>从一个可检查的任务开始</h2><p>选择场景，编辑任务。每次模型请求、工具动作与预算结算都会留在这里。</p><div id="presets" class="presets"></div><div class="note">工具仅操作模拟记录。每次运行从初始状态开始。</div></div>';
  for(const c of bootstrap.cases) {
    const button=document.createElement('button'); button.className='preset';
    button.innerHTML=`<span>↗</span>${escape(names[c.id])}<small>${escape(c.domain === 'documents' ? '读取 → 处理 → 写入' : '读取库存 → 判断 → 更新状态')}</small>`;
    button.onclick=()=>loadCase(c); $('presets').append(button);
  }
}
function reset() {
  current=null;runData=null;selectedCall=0;selectedEvent=null;feedSignature='';
  $('runId').textContent='新测试 · 独立任务状态'; $('runStatus').textContent='准备就绪'; $('runStatus').className='status';
  $('export').disabled=true;$('stop').classList.add('hidden');$('error').classList.add('hidden');
  for(const id of ['modelCount','toolCount','tokens']) $(id).textContent='—';
  welcome();renderInspector();refreshHistory();
}
async function refreshHistory() {
  const runs=await api('/api/runs');
  runActive=runs.some(r=>r.status==='running');
  updateModelButtons();
  const signature=JSON.stringify(runs.map(r=>[r.id,r.status,r.result?.passed]))+current;
  if(signature===historySignature)return;
  historySignature=signature;$('historyCount').textContent=runs.length;$('history').replaceChildren();
  for(const r of runs) {
    const b=document.createElement('button');b.className='history-item'+(r.id===current?' current':'');
    const status=r.status==='running'?'运行中':r.status==='interrupted'?'已中断':r.result?.passed?'通过':'未通过';
    b.innerHTML=`${escape(r.title)}<small>${escape(new Date(r.started_at*1000).toLocaleTimeString('zh-CN',{hour:'2-digit',minute:'2-digit'}))} · ${status} · ${escape(r.config.protocol.toUpperCase())}</small><small>${escape(r.config.model)}</small>`;
    b.title=r.title+' · '+r.config.model;
    b.onclick=async()=>{
      current=r.id;selectedCall=0;selectedEvent=null;feedSignature='';
      try {runData=await api('/api/runs/'+current);loadCase(runData.case);
        $('backend').value=runData.config.backend;$('protocol').value=runData.config.protocol;$('model').value=runData.config.model;
        $('maxSteps').value=runData.config.max_steps;$('maxTokens').value=runData.config.max_tokens;
        $('mergeSystem').checked=runData.merge_system;$('injectSchema').checked=runData.inject_schema;render();await refreshHistory();
      }catch(e){showError(e);}
    };$('history').append(b);
  }
}
function modelResults(){return (runData?.events || []).filter(e=>e.kind.type==='model_result');}
function render() {
  if(!runData)return;
  const events=runData.events || [], result=runData.result, busy=runData.status==='running';
  $('runStatus').textContent=busy?'● 正在执行':runData.status==='interrupted'?'已中断':result?.passed?'✓ 状态核验通过':'状态核验未通过';
  $('runStatus').className='status'+(busy?' busy':result?.passed?'':' failure');
  $('runId').textContent=runData.id;$('stop').classList.toggle('hidden',!busy);$('export').disabled=false;
  $('modelCount').textContent=events.filter(e=>e.kind.type==='model_request').length;
  $('toolCount').textContent=events.filter(e=>e.kind.type==='tool_call').length;
  const usages=modelResults().map(e=>e.kind.result.outcome.response?.usage?.total_tokens);
  const known=usages.filter(n=>typeof n==='number');
  $('tokens').textContent=known.length?known.reduce((a,b)=>a+b,0)+(known.length<usages.length?' + ?':''):'—';
  const signature=events.length+runData.status+JSON.stringify(result);
  if(signature!==feedSignature){
    const nearBottom=$('feed').scrollHeight-$('feed').scrollTop-$('feed').clientHeight<120;
    feedSignature=signature;
    let html=`<div class="user-message">${escape(runData.case.prompt)}</div>`;
    const requests=events.filter(e=>e.kind.type==='model_request');
    for(const event of events){
      const k=event.kind;
      if(k.type==='model_request'){
        const index=requests.indexOf(event);const response=modelResults().find(e=>e.kind.result.call_id===k.request.call_id);
        html+=`<button class="event-card" data-call="${index}"><span class="event-icon">◇</span><span class="event-copy"><b>模型请求 ${index+1} <span class="badge">${escape(runData.config.protocol.toUpperCase())}</span></b><small>${response?'响应已记录':'等待响应…'} · ${k.request.input.messages.length} 条消息 · 查看输入与约束</small></span><time>#${event.seq}</time></button>`;
        if(response){const o=response.kind.result.outcome;html+=`<div class="reply">${escape(o.response?.content || (o.response?.tool_calls?.length?pretty(o.response.tool_calls):o.status==='failed'?pretty(o):'无文本响应'))}</div>`;}
      } else if(k.type==='tool_call' || k.type==='tool_result'){
        const raw=k.call || k.result || k;
        html+=`<button class="event-card" data-event="${escape(event.event_id)}"><span class="event-icon">⌁</span><span class="event-copy"><b>${k.type==='tool_call'?'受控工具调用':'工具结果'} · ${escape(raw.name || raw.tool || raw.outcome?.status || '')}</b><small>${escape(pretty(raw.arguments || raw.outcome || raw).slice(0,150))}</small></span><time>#${event.seq}</time></button>`;
      } else if(k.type==='custom' && k.kind==='correction_v1'){
        html+=`<div class="verdict">有限纠错 · ${escape(pretty(k.payload))}</div>`;
      }
    }
    if(result)html+=`<div class="verdict ${result.passed?'pass':''}"><b>${result.passed?'✓ 最终状态与预期一致':'未通过最终状态核验'}</b><br>${escape(result.failure_category || 'Completed · 关闭成功')} · ${result.elapsed_ms} ms · 被拒写入 ${result.rejected_writes} 次<br>模型声明完成与业务验证分别记录。</div>`;
    if(runData.status==='interrupted')html+='<div class="verdict">宿主已中断运行进程。日志可能不完整；这不等同于 Jingwei 的正常取消与结算。</div>';
    if(runData.status==='error')html+=`<div class="verdict">运行器错误${code(runData.error || runData.log)}</div>`;
    if(busy)html+='<p class="inspect-note busy">● 正在等待下一条规范事件…</p>';
    $('feed').innerHTML=html;
    for(const b of $('feed').querySelectorAll('[data-call]'))b.onclick=()=>{selectedCall=Number(b.dataset.call);selectedEvent=null;activeTab='request';openLogs();};
    for(const b of $('feed').querySelectorAll('[data-event]'))b.onclick=()=>{selectedEvent=b.dataset.event;activeTab='trace';openLogs();};
    if(nearBottom)$('feed').scrollTop=$('feed').scrollHeight;
  }
  renderInspector();
}
let inspectSignature='';
function openLogs(){
  renderInspector();
  if(!$('logsDialog').open)$('logsDialog').showModal();
}
$('openLogs').onclick=openLogs;
$('closeLogs').onclick=()=>$('logsDialog').close();
function renderInspector(){
  $('logsContext').textContent=runData ? `${runData.id} · ${runData.config.model}` : '选择一条运行记录，检查各层证据。';
  document.querySelectorAll('[data-tab]').forEach(b=>b.classList.toggle('selected',b.dataset.tab===activeTab));
  const signature=JSON.stringify([current,activeTab,selectedCall,selectedEvent,runData?.events?.length,runData?.transport,runData?.status]);
  if(signature===inspectSignature)return;inspectSignature=signature;
  if(!runData){$('inspectBody').innerHTML='<div class="empty-inspect">运行后在这里检查证据<br><small>消息 → HTTP 请求 → 模板渲染 → 响应</small></div>';return;}
  const events=runData.events || [];
  if(activeTab==='request'){
    const requests=events.filter(e=>e.kind.type==='model_request');
    const e=requests[selectedCall];
    if(!e){$('inspectBody').innerHTML='<p class="inspect-note">尚未产生模型请求。</p>';return;}
    const request=e.kind.request.input;const wire=(runData.transport || [])[selectedCall];
    const select=`<label class="sr-only" for="callSelect">选择模型请求</label><select id="callSelect">${requests.map((_,i)=>`<option value="${i}" ${i===selectedCall?'selected':''}>模型请求 ${i+1}</option>`).join('')}</select>`;
    const note='<p class="inspect-note">Schema 出现在 HTTP 请求中，不代表其说明进入模型提示。下面分别展示各层证据。实验变更发生在本地传输桥，原始 Jingwei 事件不被改写；新增提示不计入框架推理前的上下文估算。</p>';
    let html=select+note+`<details open><summary>① Jingwei 原始 messages</summary>${code(request.messages)}</details><details><summary>② JSON Schema / 工具约束</summary>${code(request.constraint)}</details>`;
    if(wire){html+=`<details><summary>③ 适配器原始 HTTP 请求</summary>${code(wire.before)}</details><details open><summary>④ 实际发送的 messages</summary>${code(wire.sent.messages)}</details><details><summary>完整发送请求 · 含 response_format / tools</summary>${code(wire.sent)}</details><details><summary>⑤ 服务端模板渲染（诊断）</summary><p class="inspect-note">${escape(wire.render_note)}。该接口的诊断渲染不是推理内部 token 的捕获。</p>${code(wire.rendered ?? '未取得模板渲染')}</details><details><summary>⑥ 原始 HTTP 响应 · ${escape(wire.http_status ?? '等待中')}</summary>${code(wire.response ?? '等待响应')}</details>`;}
    else html+='<p class="inspect-note">模拟后端没有 HTTP 请求；真实后端请等待传输记录。</p>';
    $('inspectBody').innerHTML=html;$('callSelect').onchange=e=>{selectedCall=Number(e.target.value);renderInspector();};
  } else if(activeTab==='trace'){
    const selected=events.find(e=>e.event_id===selectedEvent);
    $('inspectBody').innerHTML=(selected?`<div class="inspect-title">所选规范事件</div>${code(selected)}`:'')+events.map(e=>`<details class="event-list-item"><summary><span>#${e.seq}</span>${escape(e.kind.type)} ${escape(e.kind.kind || '')}</summary>${code(e)}</details>`).join('');
  } else if(activeTab==='state'){
    const actual=runData.result?.actual_state;
    $('inspectBody').innerHTML=`<div class="inspect-title">初始状态</div>${code(runData.case.initial)}<div class="inspect-title">允许写入的字段</div>${code(runData.case.writable)}<div class="inspect-title">预期最终状态（不发送给模型）</div>${code(runData.case.expected)}<div class="inspect-title">实际最终状态</div>${code(actual ?? '执行结束后生成')}<p class="inspect-note">按完整业务状态核验，不以模型的完成声明替代。中断时不推测最终状态。</p>`;
  } else {
    const reports=events.filter(e=>e.kind.type==='task_run_report');
    $('inspectBody').innerHTML='<p class="inspect-note">预算 charged 可能包含估算与预留，不等于实际 Token 使用量。顶部仅合计模型响应中的已知 total_tokens，缺失记为未知。</p>'+ (reports.length?reports.map(e=>code(e.kind.report)).join(''):'<p class="inspect-note">等待 canonical TaskRunReport 结算事件。</p>');
  }
}
async function start(){
  $('error').classList.add('hidden');
  try {
    $('run').disabled=true;
    const payload={case_id:caseId,prompt:$('prompt').value,backend:$('backend').value,protocol:$('protocol').value,model:$('model').value,max_steps:Number($('maxSteps').value),max_tokens:Number($('maxTokens').value),merge_system:$('mergeSystem').checked,inject_schema:$('injectSchema').checked};
    for(const key of ['initial','expected','writable'])payload[key]=JSON.parse($(key).value);
    const run=await api('/api/runs',payload);current=run.id;selectedCall=0;selectedEvent=null;feedSignature='';activeTab='request';
    await poll();
  }catch(e){showError(e);$('run').disabled=false;}
}
async function poll(){
  if(pollBusy)return;pollBusy=true;
  try {await refreshHistory();await refreshModels();if(current){const id=current;const data=await api('/api/runs/'+id);if(current===id){runData=data;render();}}}
  catch(e){showError(e);}finally{pollBusy=false;}
}
let stateDraft;
$('run').onclick=start;$('newRun').onclick=reset;
$('editState').onclick=()=>{stateDraft=Object.fromEntries(['initial','expected','writable'].map(k=>[k,$(k).value]));$('stateDialog').showModal();};
$('stateDialog').onclose=()=>{if($('stateDialog').returnValue!=='save')for(const [k,v] of Object.entries(stateDraft))$(k).value=v;};
$('prompt').onkeydown=e=>{if(e.key==='Enter'&&(e.ctrlKey||e.metaKey)&&!$('run').disabled){e.preventDefault();start();}};
$('stop').onclick=async()=>{try{await api('/api/runs/'+current+'/stop',{});await poll();}catch(e){showError(e);}};
$('export').onclick=()=>{const blob=new Blob([pretty(runData)],{type:'application/json'});const url=URL.createObjectURL(blob);const a=document.createElement('a');a.href=url;a.download=`jingwei-${current}.json`;a.click();setTimeout(()=>URL.revokeObjectURL(url),1000);};
document.querySelectorAll('[data-tab]').forEach(b=>b.onclick=()=>{activeTab=b.dataset.tab;renderInspector();});
async function health(){try{const h=await api('/api/health');$('health').textContent=h.ready?'● 本地模型在线':'○ 模型未连接 · 可用模拟脚本';$('health').className='health'+(h.ready?'':' off');$('health').title=h.detail;}catch{$('health').textContent='○ 工作台连接中断';}}
(async()=>{try{bootstrap=await api('/api/bootstrap');$('model').value=bootstrap.model;loadCase(bootstrap.cases[0]);welcome();await poll();await health();setInterval(poll,900);setInterval(health,10000);}catch(e){showError(e);}})();

function updateModelButtons(){
  const loading=modelState?.status==='loading';
  $('run').disabled=runActive || ($('backend').value==='openai' && modelState?.managed && (modelState.status!=='ready' || $('modelFile').value!==modelState.current));
  $('switchModel').disabled=runActive || loading || !$('modelFile').value;
  $('modelFile').disabled=runActive || loading;
}
async function refreshModels(force=false){
  const state=await api('/api/models');modelState=state;
  $('managedModels').classList.toggle('hidden',!state.managed);
  $('externalModel').classList.toggle('hidden',state.managed);
  if(!state.managed){updateModelButtons();return;}
  const signature=JSON.stringify(state.models);
  if(signature!==catalogSignature || force){
    const previous=$('modelFile').value;catalogSignature=signature;
    $('modelFile').replaceChildren();
    for(const m of state.models){const option=document.createElement('option');option.value=m.id;option.textContent=`${m.id} · ${(m.bytes/1024**3).toFixed(2)} GiB`;$('modelFile').append(option);}
    const choose=[previous,state.current,state.target].find(id=>state.models.some(m=>m.id===id));if(choose)$('modelFile').value=choose;
  }
  if(state.current)$('model').value=state.current;
  const labels={idle:'选择一个 GGUF，然后点击加载。',loading:`正在加载 ${state.target}，请稍候…`,ready:`CUDA 已就绪 · ${state.current}`,error:`加载失败：${state.error}`};
  $('modelState').textContent=state.models.length?(labels[state.status] || state.status):'目录中尚无 GGUF 文件，下载完成后刷新。';
  $('modelState').title=state.directory+(state.log?' · 日志：'+state.log:'');
  $('modelState').classList.toggle('error',state.status==='error');
  updateModelButtons();
}
$('refreshModels').onclick=()=>refreshModels(true).catch(showError);
$('switchModel').onclick=async()=>{try{await api('/api/models/switch',{model:$('modelFile').value});await refreshModels();}catch(e){showError(e);}};
$('backend').addEventListener('change',updateModelButtons);
$('modelFile').addEventListener('change',updateModelButtons);
