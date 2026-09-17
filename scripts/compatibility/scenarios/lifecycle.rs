use std::{env, error::Error, fs, process::Command};

use microsandbox::Sandbox;
use serde_json::json;

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let mut passed = Vec::new();
    let result: Result<(), Box<dyn Error>> = async {
        let existing = env::var("MSB_COMPAT_EXISTING").ok();
        for count in if existing.is_some() {
            vec![1]
        } else {
            vec![0, 1, 3]
        } {
            let name = existing
                .clone()
                .unwrap_or_else(|| format!("compat-common-{count}"));
            let mut builder = Sandbox::builder(&name)
                .image(env::var("MSB_COMPAT_IMAGE")?)
                .memory(256u32)
                .cpus(1)
                .env("COMPAT_MARKER", "retained");
            for index in 0..count {
                builder = builder.volume(format!("/compat-data-{index}"), |v| v.tmpfs().size(8u32));
            }
            let mut sandbox = if existing.is_some() {
                Sandbox::start(&name).await?
            } else {
                builder.create().await?
            };
            for restart in 0..2 {
                let status = Command::new(env::var("MSB_COMPAT_PYTHON")?)
                    .args([env::var("MSB_COMPAT_VERIFY_RUNTIME")?, name.clone()])
                    .status()?;
                if !status.success() {
                    return Err("runtime identity mismatch".into());
                }
                let mut script = String::from("test \"$COMPAT_MARKER\" = retained; ");
                if existing.is_none() && restart == 0 {
                    script.push_str("printf retained > /root/compat-marker; ");
                }
                script.push_str("test \"$(cat /root/compat-marker)\" = retained; ");
                for index in 0..count {
                    script.push_str(&format!(
                        "test \"$(stat -f -c %T /compat-data-{index})\" = tmpfs; "
                    ));
                }
                let output = sandbox.exec("sh", ["-ec", &script]).await?;
                if !output.status().success {
                    return Err(format!("guest assertion: {}", output.stderr()?).into());
                }
                sandbox.stop().await?;
                if restart == 0 {
                    sandbox = Sandbox::start(&name).await?;
                }
            }
            passed.push(format!("{name}/runtime-env-mounts-disk-restart"));
            if existing.is_none() {
                Sandbox::remove(&name).await?;
            }
        }
        Ok(())
    }
    .await;
    let report = json!({"status": if result.is_ok() {"passed"} else {"failed"}, "passed": passed,
        "error": result.as_ref().err().map(ToString::to_string)});
    fs::write(env::var("MSB_COMPAT_REPORT").unwrap(), report.to_string()).unwrap();
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
