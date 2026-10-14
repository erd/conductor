//! Cost of `conductor hook` in a session that isn't conductor's, which is every other Claude
//! session on the machine. Ignored by default; run it on the release build:
//!
//! ```sh
//! cargo test --release -p conductor --test hook_bench -- --ignored --nocapture
//! ```

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const RUNS: usize = 200;
const BUDGET: Duration = Duration::from_millis(15);

#[test]
#[ignore = "benchmark; run on the release build"]
fn fast_path_under_budget() {
    // Spawned through `sh -c`, as the plugin's hook shim runs it.
    let run = || {
        let status = Command::new("sh")
            .args(["-c", r#"exec "$0" hook pre-tool-use"#])
            .arg(env!("CARGO_BIN_EXE_conductor"))
            .env_remove("CONDUCTOR_TASK_ID")
            .env_remove("CONDUCTOR_ROLE")
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    };
    // Warm the page cache.
    run();

    let mut times: Vec<Duration> = (0..RUNS)
        .map(|_| {
            let start = Instant::now();
            run();
            start.elapsed()
        })
        .collect();
    times.sort();
    let mean = times.iter().sum::<Duration>() / u32::try_from(RUNS).unwrap();
    let p95 = times[RUNS * 95 / 100];
    println!("conductor hook fast path over {RUNS} runs: mean {mean:?}, p95 {p95:?}");
    assert!(mean < BUDGET, "mean {mean:?} is over {BUDGET:?}");
}
