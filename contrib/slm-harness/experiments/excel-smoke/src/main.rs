//! Experimental host: domain schemas and skills are loaded from a trusted profile.
use std::{path::PathBuf, sync::{Arc,atomic::{AtomicBool,Ordering}},time::{Duration,Instant},process::Stdio};
use jingwei::{agent::*,budget::*,context::*,reference::*,plugin::*,tool::*,llm::*,id::*,HarnessBuilder};
use jingwei::action::ContextActionProtocol;
use jingwei_openai::{OpenAiConfig,OpenAiLlmPlugin};
use jingwei_standard::StandardCoreBundle;
use jingwei_tool_runtime::CanonicalToolRuntimePlugin;
use jingwei_journal_jsonl::JsonlSessionPersistencePlugin;
use jingwei_core::{CancellationSignal,CapabilityId};
use serde::Deserialize;
use serde_json::{Value,json};
use tokio::io::AsyncWriteExt;

#[derive(Deserialize)]
struct Profile { id:String, skill:String, worker:String, tools:Vec<Definition> }
#[derive(Deserialize,Clone)]
struct Definition { name:String, description:String, parameters:Value, #[serde(default)] read_only:bool }
#[derive(Deserialize)]
struct Config { profile:PathBuf, task_dir:PathBuf, worker_args:Vec<String>, endpoint:String, model:String, prompt:String }
struct Bridge { def:Definition, script:PathBuf, args:Vec<String>, receipt:Arc<AtomicBool> }
impl Tool for Bridge {
    fn metadata(&self)->ToolMetadata {
        ToolMetadata::new(&self.def.description,self.def.parameters.clone())
            .with_timeout_ceiling(Duration::from_secs(40))
            .with_effect(if self.def.read_only {ToolEffect::ReadOnly} else {ToolEffect::NonIdempotent},"smoke-v1")
    }
    fn execute<'a>(&'a self,request:ToolBodyRequest<'a>,cancel:Arc<dyn CancellationSignal>)->ToolFuture<'a,Result<String,ToolBodyError>> {
        Box::pin(async move {
            // Invalidate any earlier validation receipt before a potentially mutating call.
            if !self.def.read_only {self.receipt.store(false,Ordering::SeqCst);}
            let mut child=tokio::process::Command::new("/usr/bin/python3").arg(&self.script).args(&self.args)
                .arg(&self.def.name).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
                .kill_on_drop(true).spawn().map_err(tool_error)?;
            let mut stdin=child.stdin.take().ok_or_else(||tool_error("stdin missing"))?;
            stdin.write_all(request.arguments().to_string().as_bytes()).await.map_err(tool_error)?;
            drop(stdin);
            let output=tokio::select!{
                result=child.wait_with_output()=>result.map_err(tool_error)?,
                _=cancel.cancelled()=>return Err(tool_error("cancelled")),
            };
            if !output.status.success(){return Err(tool_error(String::from_utf8_lossy(&output.stderr)));}
            let text=String::from_utf8(output.stdout).map_err(tool_error)?;
            let value:Value=serde_json::from_str(&text).map_err(tool_error)?;
            if value.get("validated")==Some(&Value::Bool(true)) && value.get("ok")==Some(&Value::Bool(true)) {
                self.receipt.store(true,Ordering::SeqCst);
            }
            Ok(text)
        })
    }
}
fn tool_error(e:impl std::fmt::Display)->ToolBodyError{ToolBodyError::new("worker",e.to_string(),false)}
struct DomainTools(Vec<(String,Arc<dyn Tool>)>);
impl Plugin for DomainTools {
    fn descriptor(&self)->PluginDescriptor{PluginDescriptor::new("smoke.domain-tools")}
    fn mount(&self,ctx:&mut MountContext<'_>)->Result<(),MountError>{
        for(name,tool)in &self.0{ctx.register_tool(name,tool.clone())?;}Ok(())
    }
}
struct AgentPlugin(Arc<dyn Agent>);
impl Plugin for AgentPlugin {
    fn descriptor(&self)->PluginDescriptor{
        const CAPS:&[CapabilityId]=&[LLM_RUNTIME,TOOL_RUNTIME];
        PluginDescriptor::new("smoke.agent").requires_capabilities(CAPS)
    }
    fn mount(&self,ctx:&mut MountContext<'_>)->Result<(),MountError>{ctx.register_agent("worker",self.0.clone())}
}
#[tokio::main]
async fn main()->Result<(),Box<dyn std::error::Error>>{
    let config:Config=serde_json::from_slice(&std::fs::read(std::env::args().nth(1).ok_or("config required")?)?)?;
    let profile:Profile=serde_json::from_slice(&std::fs::read(&config.profile)?)?;
    let root=config.profile.parent().ok_or("profile parent")?;
    let skill=std::fs::read_to_string(root.join(&profile.skill))?;
    let receipt=Arc::new(AtomicBool::new(false));
    let names:Vec<_>=profile.tools.iter().map(|x|x.name.clone()).collect();
    let tools=profile.tools.into_iter().map(|def|{
        let name=def.name.clone();(name,Arc::new(Bridge{def,script:root.join(&profile.worker),args:config.worker_args.clone(),receipt:receipt.clone()}) as Arc<dyn Tool>)
    }).collect();
    let mut ac=ReferenceAgentConfig::new(ContextTarget{model:config.model.clone(),template_revision:"ollama-smoke-v1".into()},ContextBudget{
        window_tokens:8192,output_reserve:1024,output_evidence:TokenBoundEvidence::Estimate,safety_margin:256,mode:TokenBudgetMode::Soft,
    });
    ac.protocol=ContextActionProtocol::Json;ac.max_steps=12;ac.max_corrections=2;ac.system=vec![skill];
    let store=MemoryContentStore::new(ContentStoreConfig{store_id:"smoke".into(),max_entries:64,max_bytes:1024*1024,max_content_bytes:65536,max_page_bytes:32768,max_ttl_ms:600_000})?;
    let agent=ReferenceAgent::new(ac,ReferenceAgentPolicies{
        counter:Arc::new(ByteHeuristicCounter::default()),selector:Arc::new(GroupedToolSelector::default()),
        result:Arc::new(BoundedResultPolicy{inline_bytes:32768,preview_bytes:1024}),store:Arc::new(store),clock:Arc::new(SystemReferenceClock),
    })?;
    let model=OpenAiConfig::new(&config.endpoint,&config.model)?.with_json_schema(GenerationSupport{complete:CapabilitySupport::Supported,stream:CapabilitySupport::Unsupported});
    let harness=StandardCoreBundle::new().with_tool_runtime(CanonicalToolRuntimePlugin::new().grant_all(PluginId::new("smoke.agent")).with_default_timeout(Duration::from_secs(40)))
        .install_into(HarnessBuilder::new()).plugin(JsonlSessionPersistencePlugin::new(config.task_dir.join("journal")))
        .plugin(OpenAiLlmPlugin::new(model)).plugin(DomainTools(tools)).plugin(AgentPlugin(Arc::new(agent)))
        .select_persistence("jsonl").select_session_runtime("canonical").select_agent_runtime("canonical")
        .select_llm("openai").select_llm_runtime("canonical").select_tool_runtime("canonical").build().await?;
    let session=SessionId::new();let task=TaskId::new();
    let limits=BudgetLimits{resources:BudgetAmounts{steps:16,model_requests:16,tool_calls:32,corrections:2,input_tokens:100_000,output_tokens:20_000,tool_output_bytes:2*1024*1024},active_time:Duration::from_secs(180)};
    let budget=TaskBudget::new(BudgetIdentity{task_id:task,session_id:session.clone(),agent_key:"worker".into()},limits,limits,TokenBudgetMode::Soft,Arc::new(MonotonicBudgetClock::default()))?;
    let start=Instant::now();
    let controller=harness.start_turn_request(AgentTurnRequest::new(session,"worker",&config.prompt).with_budget(budget,limits))?;
    let result=controller.wait().await;let shutdown=harness.shutdown().await;
    let mut report=json!({"profile":profile.id,"visible_tools":names,"elapsed_seconds":start.elapsed().as_secs_f64(),"receipt":receipt.load(Ordering::SeqCst),"shutdown_ok":shutdown.is_ok()});
    match result{
        Ok(r)=>{report["runtime_ok"]=json!(true);report["disposition"]=json!(format!("{:?}",r.disposition()));report["final_text"]=json!(r.final_text());report["artifact"]=json!(r.artifact());report["events"]=json!(r.events());}
        Err(e)=>{report["runtime_ok"]=json!(false);report["error"]=json!(format!("{e:?}"));}
    }
    std::fs::write(config.task_dir.join("agent-report.json"),serde_json::to_vec_pretty(&report)?)?;
    println!("{}",json!({"runtime_ok":report["runtime_ok"],"elapsed_seconds":report["elapsed_seconds"],"receipt":report["receipt"]}));
    Ok(())
}
