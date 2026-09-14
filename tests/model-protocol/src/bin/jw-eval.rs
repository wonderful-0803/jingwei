//! Private JW-08 driver; never a public framework dependency.
#[path = "eval/config.rs"]
mod config;
#[path = "eval/runtime.rs"]
mod runtime;
use serde_json::json;
use std::{
    fs::{self, File},
    io::Write,
    path::PathBuf,
};
type Error = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    match run().await {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("jw-eval: {error}");
            std::process::exit(2);
        }
    }
}
async fn run() -> Result<bool, Error> {
    let mut args = std::env::args().skip(1);
    let mut config_path = None;
    let mut output = None;
    let mut selected = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--help" {
            println!(
                "jw-eval --config FILE --output NEW_DIRECTORY [--case ID ...]\nExit: 0 all passed; 1 task failures; 2 configuration/reporting error."
            );
            return Ok(true);
        }
        let value = args.next().ok_or("missing option value")?;
        match arg.as_str() {
            "--config" if config_path.is_none() => config_path = Some(PathBuf::from(value)),
            "--output" if output.is_none() => output = Some(PathBuf::from(value)),
            "--case" => selected.push(value),
            _ => return Err(format!("unknown or repeated option: {arg}").into()),
        }
    }
    let path = config_path.ok_or("--config is required")?;
    let config: config::Config = serde_json::from_slice(&fs::read(&path)?)?;
    config.validate()?;
    let tasks_path = path
        .parent()
        .unwrap_or(std::path::Path::new("."))
        .join(&config.tasks);
    let tasks: Vec<config::Case> = serde_json::from_slice(&fs::read(tasks_path)?)?;
    config::validate_cases(&tasks, &selected)?;
    // Validate provider and credentials before creating any output or running tasks.
    let provider = runtime::provider_config(&config)?;
    let output = output.ok_or("--output is required")?;
    fs::create_dir(&output)?; // Never overwrite or mix with a previous run.
    fs::write(
        output.join("manifest.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version":1,"stage":"JW-08-a pilot, not AT-F5 acceptance",
            "config":config,"tasks":tasks,"selected_cases":selected,
            "sampling":"provider defaults; not frozen by this driver",
            "host_peak_memory_bytes":null,"model_server_peak_memory_bytes":null,
            "memory_note":"not measured in JW-08-a"
        }))?,
    )?;
    let mut records = File::create(output.join("results.jsonl"))?;
    let mut passed = 0;
    let mut completed = 0;
    let planned = tasks
        .iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.id))
        .count()
        * config.runs as usize;
    // A partial summary exists even if the process is interrupted later.
    let summary = |completed, passed| {
        json!({"schema_version":1,"planned":planned,
        "completed":completed,"passed":passed,"failed":completed-passed,
        "incomplete":planned-completed,"real_model":config.backend == "openai"})
    };
    fs::write(
        output.join("summary.json"),
        serde_json::to_vec_pretty(&summary(completed, passed))?,
    )?;
    for case in tasks
        .iter()
        .filter(|c| selected.is_empty() || selected.contains(&c.id))
    {
        for repetition in 1..=config.runs {
            let record =
                runtime::execute(&config, provider.clone(), case, repetition, &output).await;
            serde_json::to_writer(&mut records, &record)?;
            writeln!(records)?;
            records.flush()?;
            passed += usize::from(record["passed"] == true);
            completed += 1;
            fs::write(
                output.join("summary.json"),
                serde_json::to_vec_pretty(&summary(completed, passed))?,
            )?;
            println!(
                "{} #{repetition}: {}",
                case.id,
                if record["passed"] == true {
                    "PASS"
                } else {
                    "FAIL"
                }
            );
        }
    }
    println!("{passed}/{completed} passed; results: {}", output.display());
    Ok(passed == completed)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (config::Config, Vec<config::Case>) {
        (
            serde_json::from_str(include_str!("../../eval/fake.json")).unwrap(),
            serde_json::from_str(include_str!("../../eval/pilot.json")).unwrap(),
        )
    }
    async fn execute(config: &config::Config, case: &config::Case) -> serde_json::Value {
        let path =
            std::env::temp_dir().join(format!("jw-eval-test-{}", jingwei_core::SessionId::new()));
        fs::create_dir(&path).unwrap();
        let result = runtime::execute(config, None, case, 1, &path).await;
        fs::remove_dir_all(path).unwrap();
        result
    }
    #[tokio::test]
    async fn pilot_runs_through_both_protocols_and_isolates_repetitions() {
        let (mut config, cases) = fixture();
        for protocol in ["json", "native"] {
            config.protocol = protocol.into();
            for case in &cases {
                let record = execute(&config, case).await;
                assert_eq!(record["passed"], true, "{record:#}");
                assert_eq!(
                    record["details"]["task_run_report"]["capabilities_drained"],
                    true
                );
            }
        }
    }
    #[tokio::test]
    async fn false_completion_and_forbidden_write_are_failures_with_evidence() {
        let (config, mut cases) = fixture();
        let case = &mut cases[0];
        case.fake_actions = vec![json!({"action":"final","text":"完成"})];
        let false_claim = execute(&config, case).await;
        assert_eq!(false_claim["passed"], false);
        assert_eq!(false_claim["failure_category"], "state_mismatch");
        case.fake_actions = vec![
            json!({"action":"call_tool","name":"write_record","arguments":{"key":"source","value":"bad"}}),
        ];
        let denied = execute(&config, case).await;
        assert_eq!(denied["passed"], false);
        assert_eq!(denied["rejected_writes"], 1);
        assert_eq!(
            denied["actual_state"],
            serde_json::to_value(&case.initial).unwrap()
        );
        assert!(denied["details"]["failure_evidence"].is_string());
    }
    #[test]
    fn invalid_configuration_and_selection_fail_before_execution() {
        let (mut config, mut cases) = fixture();
        config.max_steps = 0;
        assert!(config.validate().is_err());
        assert!(config::validate_cases(&cases, &["unknown".into()]).is_err());
        cases[0].id = "../escape".into();
        assert!(config::validate_cases(&cases, &[]).is_err());
    }
}
